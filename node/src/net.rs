//! P2P network manager: length-delimited TCP framing, handshake, gossip relay, sync
//! driver and misbehaviour bans (`node::net`, §4, §8.6).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant as TokioInstant;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, info, warn};
use wire::{
    Block, Digest, HANDSHAKE_TIMEOUT, MAX_FRAME_BYTES, MAX_REORG_DEPTH, MAX_SYNC_BLOCKS,
    MAX_SYNC_BYTES, SYNC_LOOKBACK, SYNC_TIMEOUT, ShareProof, WIRE_VERSION, WireMessage, codec,
};

use crate::chain_actor::{
    ChainCmd, ChainEvent, INVALID_BAN, ImportAck, Origin, PeerId, ShareAck, Source, StatusSnapshot,
};
use crate::mempool::{MempoolCmd, MempoolEvent, MempoolReject};

/// Ban for protocol violations (bad frames, unexpected messages), §4.8.
const PROTOCOL_BAN: Duration = Duration::from_secs(60);
/// Invalid jobs tolerated per connection before a ban.
const MAX_INVALID_JOBS: u32 = 10;
/// Gossiped shares a peer may send per minute that turn out useless (ignored, or a
/// duplicate under a new encoding). Past it, the peer's shares are dropped unread for
/// the rest of the minute. Honest peers relay a few shares per block.
const MAX_USELESS_SHARES_PER_MIN: u32 = 256;
/// Inbound connections accepted per source (an IP, or an IPv6 /64; loopback is exempt
/// for local testnets). Generous, for honest nodes sharing a NAT address; it bounds
/// sockets and per-connection budgets per source.
const MAX_INBOUND_PER_IP: usize = 16;
/// Time allowed to open a TCP connection to one address of a peer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection that lasted this long resets the dialer's backoff; shorter ones (a peer
/// that disconnects us at once) count as failed attempts.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Redial interval for a peer on another chain, genesis or protocol version.
const INCOMPATIBLE_BACKOFF_SECS: u64 = 600;
/// Delay before asking a peer again for blocks this node refused as busy.
const BUSY_RETRY: Duration = Duration::from_secs(2);
/// Accepted blocks more than this far below the tip are not relayed: peers that
/// need them fetch them with `SyncRequest`.
const RELAY_DEPTH: u64 = 2 * SYNC_LOOKBACK;

/// A static peer: a socket address, or a `host:port` name resolved again before every
/// connection attempt (bootnodes are named, and their addresses may change).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerAddr {
    /// `IP:port`.
    Socket(SocketAddr),
    /// `host:port`.
    Host(String),
}

impl std::str::FromStr for PeerAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(Self::Socket(addr));
        }
        let (host, port) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("peer {s:?} is not IP:port or host:port"))?;
        port.parse::<u16>()
            .map_err(|_| format!("peer {s:?} has an invalid port"))?;
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
        if host.is_empty() || !host.chars().all(valid) {
            return Err(format!("peer {s:?} has an invalid host name"));
        }
        Ok(Self::Host(s.to_owned()))
    }
}

impl std::fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(addr) => write!(f, "{addr}"),
            Self::Host(host) => f.write_str(host),
        }
    }
}

impl From<SocketAddr> for PeerAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::Socket(addr)
    }
}

/// How one dial attempt ended.
enum Dial {
    /// A connection lasted at least `STABLE_CONNECTION`.
    Stable,
    /// Already connected to it, or networking is paused: nothing was attempted.
    Skipped,
    /// The peer is on another chain, genesis or protocol version.
    Incompatible,
    /// Every address of the peer is this node.
    Myself,
    /// Unresolvable, unreachable, banned, refused, or a short-lived connection.
    Failed,
}

/// How a connection ended.
enum ConnEnd {
    /// No usable handshake: closed, timed out, banned, or a duplicate connection.
    Refused,
    /// The peer is on another chain, genesis or protocol version.
    Incompatible,
    /// The peer is this node: it sent our own nonce.
    Myself,
    /// The peer was registered; the connection lasted this long.
    Ended(Duration),
}

struct PeerHandle {
    addr: SocketAddr,
    outbound: bool,
    tx: mpsc::Sender<WireMessage>,
    best_height: u64,
    invalid_jobs: u32,
    useless_shares: u32,
    useless_window: Instant,
}

#[derive(Default)]
struct SyncState {
    from: u64,
    lookback: u64,
    /// A `SyncRequest` is outstanding.
    outstanding: bool,
    deadline: Option<TokioInstant>,
    /// Last block of the previous batch: the next request waits until it is applied.
    awaiting: Option<(Digest, u64)>,
    /// Ask again from about this height once the instant has passed: the node refused
    /// some of the peer's blocks as busy.
    retry: Option<(TokioInstant, u64)>,
}

struct LruSeen<T> {
    order: VecDeque<T>,
    set: HashSet<T>,
    capacity: usize,
}

impl<T: Copy + Eq + std::hash::Hash> LruSeen<T> {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Whether `item` was seen.
    fn contains(&self, item: &T) -> bool {
        self.set.contains(item)
    }

    /// Records `item`; returns false if it was already seen.
    fn mark_seen(&mut self, item: T) -> bool {
        if !self.set.insert(item) {
            return false;
        }
        if self.order.len() >= self.capacity
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        self.order.push_back(item);
        true
    }
}

struct NetShared {
    peers: HashMap<PeerId, PeerHandle>,
    bans: HashMap<Source, Instant>,
    /// Relay ids (`chain::block_relay_id`), not block ids: a copy with a forged
    /// signature or body has the honest `block_id` and must not shadow the genuine block.
    /// Only settled blocks (accepted or invalid) are recorded: copies of a pending block
    /// still reach the chain actor, which remembers who else delivered it.
    seen_blocks: LruSeen<Digest>,
    /// Relay ids (`chain::job_relay_id`), not tx ids, for the same reason.
    seen_jobs: LruSeen<Digest>,
    /// Relay ids (`chain::share_relay_id`), not work ids, for the same reason. Settled
    /// shares only, as for blocks.
    seen_shares: LruSeen<Digest>,
    /// Open inbound connections per remote source (loopback not counted).
    inbound_per_ip: HashMap<Source, usize>,
    paused: bool,
}

/// Why a connection loop ended.
enum Disconnect {
    Closed,
    Ban(Duration, String),
}

/// Manages TCP peers, gossip relay, sync and misbehaviour bans (§4, §8.6).
pub struct PeerManager {
    local_addr: SocketAddr,
    chain_id: u64,
    /// Random per process: a `Handshake` carrying it comes from this node.
    node_nonce: u64,
    next_peer_id: AtomicU64,
    shared: Mutex<NetShared>,
    chain_tx: mpsc::Sender<ChainCmd>,
    mempool_tx: mpsc::Sender<MempoolCmd>,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    shutdown_rx: watch::Receiver<bool>,
}

/// One inbound connection counted against its source's limit; released on drop.
struct InboundSlot {
    mgr: Arc<PeerManager>,
    source: Source,
}

impl InboundSlot {
    /// Counts a new inbound connection from `addr`, or `None` if its source is at its
    /// limit. Loopback connections are not limited (they carry a dummy slot).
    fn reserve(mgr: &Arc<PeerManager>, addr: SocketAddr) -> Option<Self> {
        let source = Source::of(addr);
        if let Source::Ip(_) = source {
            let mut guard = mgr.lock();
            let count = guard.inbound_per_ip.entry(source).or_insert(0);
            if *count >= MAX_INBOUND_PER_IP {
                return None;
            }
            *count += 1;
        }
        Some(Self {
            mgr: mgr.clone(),
            source,
        })
    }
}

impl Drop for InboundSlot {
    fn drop(&mut self) {
        if let Source::Addr(_) = self.source {
            return;
        }
        let mut guard = self.mgr.lock();
        if let Some(count) = guard.inbound_per_ip.get_mut(&self.source) {
            *count -= 1;
            if *count == 0 {
                guard.inbound_per_ip.remove(&self.source);
            }
        }
    }
}

/// The `version` of a frame that starts like a `Handshake` of any protocol version: the
/// variant tag 0, then the version, both little-endian `u32`.
fn handshake_version(frame: &[u8]) -> Option<u32> {
    let word = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes(frame.get(i..i + 4)?.try_into().ok()?))
    };
    (word(0)? == 0).then_some(word(4)?)
}

fn is_shutdown(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow()
}

async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

impl PeerManager {
    /// Binds the listener and starts the accept loop, the dialers and the relay loop.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        listen_addr: SocketAddr,
        static_peers: Vec<PeerAddr>,
        chain_id: u64,
        chain_tx: mpsc::Sender<ChainCmd>,
        mempool_tx: mpsc::Sender<MempoolCmd>,
        mut chain_events: broadcast::Receiver<ChainEvent>,
        mut mempool_events: broadcast::Receiver<MempoolEvent>,
        status_rx: watch::Receiver<Arc<StatusSnapshot>>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        let node_nonce = {
            use std::hash::{BuildHasher, Hasher};
            // RandomState is seeded from the operating system's RNG.
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
            );
            h.finish()
        };
        let manager = Arc::new(Self {
            local_addr,
            chain_id,
            node_nonce,
            next_peer_id: AtomicU64::new(1),
            shared: Mutex::new(NetShared {
                peers: HashMap::new(),
                bans: HashMap::new(),
                seen_blocks: LruSeen::new(8_192),
                seen_jobs: LruSeen::new(16_384),
                seen_shares: LruSeen::new(16_384),
                inbound_per_ip: HashMap::new(),
                paused: false,
            }),
            chain_tx,
            mempool_tx,
            status_rx,
            shutdown_rx: shutdown_rx.clone(),
        });

        // Accept loop
        let mgr = manager.clone();
        let mut sd = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = wait_shutdown(&mut sd) => break,
                    accepted = listener.accept() => {
                        let Ok((stream, addr)) = accepted else { continue };
                        if mgr.is_banned_or_paused(addr) {
                            continue;
                        }
                        let Some(slot) = InboundSlot::reserve(&mgr, addr) else {
                            debug!(%addr, "Too many inbound connections from this IP");
                            continue;
                        };
                        let conn = mgr.clone();
                        tokio::spawn(async move {
                            conn.run_connection(stream, addr, false).await;
                            drop(slot);
                        });
                    }
                }
            }
        });

        // Outbound dialers with exponential backoff (§4.5)
        for peer_addr in static_peers {
            manager.spawn_dialer(peer_addr);
        }

        // Relay loop (§4.6, §8.6)
        let mgr = manager.clone();
        let mut sd = shutdown_rx;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = wait_shutdown(&mut sd) => break,
                    ev = chain_events.recv() => match ev {
                        Ok(ev) => mgr.handle_chain_event(ev),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "Relay loop lagged behind chain events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    ev = mempool_events.recv() => match ev {
                        Ok(ev) => mgr.handle_mempool_event(ev),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "Relay loop lagged behind mempool events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });

        Ok(manager)
    }

    /// Bound listen address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.lock().peers.len()
    }

    /// The highest tip height a connected peer has reported, if any peer is connected.
    pub fn best_peer_height(&self) -> Option<u64> {
        self.lock().peers.values().map(|p| p.best_height).max()
    }

    /// Adds an outbound peer with its own reconnecting dialer.
    pub fn add_outbound_peer(self: &Arc<Self>, addr: impl Into<PeerAddr>) {
        self.spawn_dialer(addr.into());
    }

    /// Pauses (drops every connection, refuses new ones) or resumes networking.
    /// Used to partition nodes in tests.
    pub fn set_network_paused(&self, paused: bool) {
        let mut guard = self.lock();
        guard.paused = paused;
        if paused {
            guard.peers.clear();
        }
    }

    fn lock(&self) -> MutexGuard<'_, NetShared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn spawn_dialer(self: &Arc<Self>, peer: PeerAddr) {
        let mgr = self.clone();
        let mut sd = self.shutdown_rx.clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            let mut warned = false;
            while !is_shutdown(&sd) {
                // Back off unless the peer kept us: one that refuses the connection (e.g.
                // at its per-IP limit) or drops it at once gets no 1 Hz redial.
                match mgr.dial(&peer, &mut warned).await {
                    Dial::Stable => backoff_secs = 1,
                    Dial::Skipped => {}
                    Dial::Incompatible => {
                        warn!(%peer, "Peer is on another chain or protocol version; retrying rarely");
                        backoff_secs = INCOMPATIBLE_BACKOFF_SECS;
                    }
                    Dial::Myself => {
                        info!(%peer, "Not dialing this peer: it is this node");
                        return;
                    }
                    Dial::Failed => backoff_secs = (backoff_secs * 2).clamp(1, 30),
                }
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    () = wait_shutdown(&mut sd) => break,
                }
            }
        });
    }

    /// One attempt to reach `peer`: resolves it, then tries its addresses in turn and
    /// runs the first connection that is established to its end.
    async fn dial(&self, peer: &PeerAddr, warned: &mut bool) -> Dial {
        let addrs: Vec<SocketAddr> = match peer {
            PeerAddr::Socket(addr) => vec![*addr],
            PeerAddr::Host(host) => match tokio::net::lookup_host(host.as_str()).await {
                Ok(addrs) => addrs.collect(),
                Err(e) => {
                    if !std::mem::replace(warned, true) {
                        warn!(%peer, error = %e, "Cannot resolve peer; retrying with backoff");
                    }
                    return Dial::Failed;
                }
            },
        };
        let (mut myself, mut skipped) = (0, false);
        for &addr in &addrs {
            if addr == self.local_addr {
                myself += 1;
                continue;
            }
            if self.has_peer_addr(addr) || self.lock().paused {
                skipped = true;
                continue;
            }
            if self.is_banned(addr) {
                continue;
            }
            let Ok(Ok(stream)) =
                tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await
            else {
                continue;
            };
            // Our own address and listen port: a bootnode listed among its own peers.
            if let (Ok(local), Ok(remote)) = (stream.local_addr(), stream.peer_addr())
                && remote == SocketAddr::new(local.ip(), self.local_addr.port())
            {
                myself += 1;
                continue;
            }
            *warned = false;
            match self.run_connection(stream, addr, true).await {
                ConnEnd::Myself => myself += 1,
                ConnEnd::Incompatible => return Dial::Incompatible,
                ConnEnd::Ended(lasted) if lasted >= STABLE_CONNECTION => return Dial::Stable,
                ConnEnd::Ended(_) | ConnEnd::Refused => return Dial::Failed,
            }
        }
        if myself > 0 && myself == addrs.len() {
            Dial::Myself
        } else if skipped {
            Dial::Skipped
        } else {
            Dial::Failed
        }
    }

    fn is_banned(&self, addr: SocketAddr) -> bool {
        let mut guard = self.lock();
        let now = Instant::now();
        guard.bans.retain(|_, until| now < *until);
        guard.bans.contains_key(&Source::of(addr))
    }

    fn is_banned_or_paused(&self, addr: SocketAddr) -> bool {
        self.lock().paused || self.is_banned(addr)
    }

    /// Bans a source and drops every connection from it (their loops notice within one
    /// poll tick), and everything it sent that still awaits verification
    /// (`ChainCmd::CancelSource`; the chain actor has usually done so already).
    fn ban(&self, source: Source, duration: Duration, reason: &str) {
        warn!(?source, secs = duration.as_secs(), reason, "Banning peer");
        {
            let mut guard = self.lock();
            // A shorter ban (e.g. for a stray frame) never shortens a running one.
            let until = Instant::now() + duration;
            let entry = guard.bans.entry(source).or_insert(until);
            *entry = (*entry).max(until);
            guard.peers.retain(|_, p| Source::of(p.addr) != source);
        }
        let chain_tx = self.chain_tx.clone();
        tokio::spawn(async move {
            let _ = chain_tx
                .send(ChainCmd::CancelSource {
                    source,
                    ban: duration,
                })
                .await;
        });
    }

    fn has_peer_addr(&self, addr: SocketAddr) -> bool {
        self.lock().peers.values().any(|p| p.addr == addr)
    }

    fn peer_best_height(&self, peer_id: PeerId) -> u64 {
        self.lock().peers.get(&peer_id).map_or(0, |p| p.best_height)
    }

    /// Lowers the best height a peer claimed to `height` (`--mine-when-synced` waits for
    /// this node to reach the best claimed height).
    fn lower_best_height(&self, peer_id: PeerId, height: u64) {
        if let Some(p) = self.lock().peers.get_mut(&peer_id) {
            p.best_height = p.best_height.min(height);
        }
    }

    fn broadcast(&self, msg: &WireMessage, exclude: Option<PeerId>) {
        let guard = self.lock();
        for (&pid, handle) in &guard.peers {
            if Some(pid) != exclude {
                // A full queue drops gossip frames for that peer (§4.5).
                if handle.tx.try_send(msg.clone()).is_err() {
                    debug!(peer = pid, "Peer queue full; dropping gossip frame");
                }
            }
        }
    }

    fn handle_chain_event(&self, ev: ChainEvent) {
        match ev {
            ChainEvent::BlockAccepted { block, origin } => {
                let exclude = match origin {
                    Origin::Peer(pid, _) => Some(pid),
                    _ => None,
                };
                let tip_height = self.status_rx.borrow().tip.height;
                let height = block.header.context.height;
                if let Ok(relay_id) = chain::block_relay_id(&block) {
                    self.lock().seen_blocks.mark_seen(relay_id);
                }
                if height + RELAY_DEPTH >= tip_height {
                    let msg = WireMessage::GossipBlock(Box::new((*block).clone()));
                    self.broadcast(&msg, exclude);
                }
            }
            ChainEvent::BlockRejected {
                block_id,
                origin: Origin::Peer(pid, source),
                reason,
            } => {
                warn!(peer = pid, id = ?block_id, %reason, "Peer sent an invalid block");
                self.ban(source, INVALID_BAN, &reason);
            }
            // Own shares go to every peer; verified gossip to every peer but its sender.
            ChainEvent::ShareAccepted { share, origin } => {
                let exclude = match origin {
                    Origin::Peer(pid, _) => Some(pid),
                    _ => None,
                };
                if let Ok(relay_id) = chain::share_relay_id(&share) {
                    self.lock().seen_shares.mark_seen(relay_id);
                }
                self.broadcast(
                    &WireMessage::GossipShare(Box::new((*share).clone())),
                    exclude,
                );
            }
            ChainEvent::ShareRejected {
                origin: Origin::Peer(pid, source),
                reason,
            } => {
                warn!(peer = pid, %reason, "Peer sent an invalid share");
                self.ban(source, INVALID_BAN, &reason);
            }
            _ => {}
        }
    }

    fn handle_mempool_event(&self, ev: MempoolEvent) {
        let MempoolEvent::Accepted { tx, origin } = ev;
        let exclude = match origin {
            Origin::Peer(pid, _) => Some(pid),
            _ => None,
        };
        if let Ok(relay_id) = chain::job_relay_id(&tx) {
            self.lock().seen_jobs.mark_seen(relay_id);
        }
        self.broadcast(&WireMessage::GossipJob(tx), exclude);
    }

    /// Runs one connection to its end.
    async fn run_connection(&self, stream: TcpStream, addr: SocketAddr, outbound: bool) -> ConnEnd {
        let source = Source::of(addr);
        let _ = stream.set_nodelay(true);
        let codec = LengthDelimitedCodec::builder()
            .length_field_type::<u32>()
            .big_endian()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        let mut framed = Framed::new(stream, codec);

        // Send our Handshake immediately (§4.5 step 2).
        let our_hs = {
            let snap = self.status_rx.borrow();
            WireMessage::Handshake {
                version: WIRE_VERSION,
                chain_id: self.chain_id,
                genesis_id: snap.genesis_id,
                node_nonce: self.node_nonce,
                best_height: snap.tip.height,
                best_block_id: snap.tip.id,
            }
        };
        let Ok(hs_bytes) = codec::encode(&our_hs) else {
            return ConnEnd::Refused;
        };
        if framed.send(Bytes::from(hs_bytes)).await.is_err() {
            return ConnEnd::Refused;
        }

        // Wait at most HANDSHAKE_TIMEOUT for the peer's Handshake (§4.5 step 3).
        // A timeout or closed socket is not misbehaviour; a wrong first frame is.
        let (peer_best_height, peer_best_id) = match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            framed.next(),
        )
        .await
        {
            Ok(Some(Ok(bytes))) => match codec::decode::<WireMessage>(&bytes) {
                Ok(WireMessage::Handshake {
                    version,
                    chain_id,
                    genesis_id,
                    node_nonce,
                    best_height,
                    best_block_id,
                }) => {
                    if node_nonce == self.node_nonce {
                        debug!(%addr, "Connected to this node itself");
                        return ConnEnd::Myself;
                    }
                    let ours = self.status_rx.borrow().genesis_id;
                    if version != WIRE_VERSION || chain_id != self.chain_id || genesis_id != ours {
                        info!(%addr, version, chain_id, %genesis_id, "Peer is on another chain, genesis or protocol version");
                        return ConnEnd::Incompatible;
                    }
                    (best_height, best_block_id)
                }
                // A Handshake of another protocol version need not decode (its fields
                // differ): it is recognised by its variant tag and version.
                _ if handshake_version(&bytes).is_some_and(|v| v != WIRE_VERSION) => {
                    info!(%addr, version = ?handshake_version(&bytes), "Peer speaks another protocol version");
                    return ConnEnd::Incompatible;
                }
                _ => {
                    self.ban(source, PROTOCOL_BAN, "first frame is not a valid Handshake");
                    return ConnEnd::Refused;
                }
            },
            Ok(Some(Err(e))) if e.kind() == std::io::ErrorKind::InvalidData => {
                self.ban(source, PROTOCOL_BAN, "oversize frame");
                return ConnEnd::Refused;
            }
            _ => {
                debug!(%addr, "No handshake from peer");
                return ConnEnd::Refused;
            }
        };

        // Register the peer, deduplicating outbound connections by address (§4.5 step 4).
        let peer_id = self.next_peer_id.fetch_add(1, Ordering::Relaxed);
        let (write_tx, mut write_rx) = mpsc::channel::<WireMessage>(256);
        {
            let mut guard = self.lock();
            if guard.paused
                || (outbound && guard.peers.values().any(|p| p.outbound && p.addr == addr))
            {
                return ConnEnd::Refused;
            }
            guard.peers.insert(
                peer_id,
                PeerHandle {
                    addr,
                    outbound,
                    tx: write_tx.clone(),
                    best_height: peer_best_height,
                    invalid_jobs: 0,
                    useless_shares: 0,
                    useless_window: Instant::now(),
                },
            );
        }
        info!(peer = peer_id, %addr, outbound, peer_best_height, "Connected peer");
        let connected_at = Instant::now();

        let (mut sink, mut frames) = framed.split();
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                let Ok(bytes) = codec::encode(&msg) else {
                    break;
                };
                if sink.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
        });

        let mut sync = SyncState::default();
        if peer_best_height > 0 && !self.has_block(peer_best_id).await {
            self.start_sync(&write_tx, &mut sync, peer_best_height);
        }

        let mut poll = tokio::time::interval(Duration::from_millis(100));
        let mut shutdown = self.shutdown_rx.clone();
        let mut req_window = Instant::now();
        let mut req_count = 0u32;
        let reason = loop {
            let deadline = sync.deadline;
            tokio::select! {
                () = wait_shutdown(&mut shutdown) => break Disconnect::Closed,
                frame = frames.next() => {
                    // Banned meanwhile (its source sent something invalid): read no more.
                    if !self.lock().peers.contains_key(&peer_id) {
                        break Disconnect::Closed;
                    }
                    let bytes = match frame {
                        None => break Disconnect::Closed,
                        Some(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                            break Disconnect::Ban(PROTOCOL_BAN, "oversize frame".into());
                        }
                        // Resets and other I/O errors are not misbehaviour.
                        Some(Err(_)) => break Disconnect::Closed,
                        Some(Ok(bytes)) => bytes,
                    };
                    let Ok(msg) = codec::decode::<WireMessage>(&bytes) else {
                        break Disconnect::Ban(PROTOCOL_BAN, "undecodable frame".into());
                    };
                    if req_window.elapsed() >= Duration::from_secs(1) {
                        req_window = Instant::now();
                        req_count = 0;
                    }
                    if matches!(msg, WireMessage::SyncRequest { .. }) {
                        req_count += 1;
                        if req_count > 10 {
                            continue; // rate limit (§4.8): drop the excess requests
                        }
                    }
                    if let Err(d) = self.handle_peer_message(peer_id, source, msg, &write_tx, &mut sync).await {
                        break d;
                    }
                }
                () = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    debug!(peer = peer_id, "Sync request timed out");
                    // A peer that does not answer is not ahead of us, whatever it claimed.
                    // (Waiting for our own verification of its last batch is not its fault.)
                    if sync.outstanding {
                        let tip = self.status_rx.borrow().tip.height;
                        self.lower_best_height(peer_id, tip);
                    }
                    sync = SyncState::default();
                }
                _ = poll.tick() => {
                    // Paused, or banned meanwhile: a block or share rejected after its
                    // asynchronous check removes the peer (ChainEvent::*Rejected).
                    {
                        let guard = self.lock();
                        if guard.paused || !guard.peers.contains_key(&peer_id) {
                            break Disconnect::Closed;
                        }
                    }
                    if let Some((await_id, last_h)) = sync.awaiting
                        && self.has_block(await_id).await
                    {
                        sync.awaiting = None;
                        sync.from = last_h + 1;
                        self.send_sync_request(&write_tx, &mut sync);
                    }
                    if let Some((at, hint)) = sync.retry
                        && TokioInstant::now() >= at
                        && !sync.outstanding
                    {
                        sync.retry = None;
                        self.start_sync(&write_tx, &mut sync, hint);
                    }
                }
            }
        };

        write_task.abort();
        self.lock().peers.remove(&peer_id);
        match reason {
            Disconnect::Closed => debug!(peer = peer_id, %addr, "Disconnected peer"),
            Disconnect::Ban(duration, why) => self.ban(source, duration, &why),
        }
        ConnEnd::Ended(connected_at.elapsed())
    }

    async fn handle_peer_message(
        &self,
        peer_id: PeerId,
        source: Source,
        msg: WireMessage,
        write_tx: &mpsc::Sender<WireMessage>,
        sync: &mut SyncState,
    ) -> Result<(), Disconnect> {
        let origin = Origin::Peer(peer_id, source);
        match msg {
            WireMessage::Handshake { .. } => {
                Err(Disconnect::Ban(PROTOCOL_BAN, "second Handshake".into()))
            }
            WireMessage::GossipBlock(block) => {
                let height = block.header.context.height;
                let Ok(relay_id) = chain::block_relay_id(&block) else {
                    return Ok(());
                };
                if self.lock().seen_blocks.contains(&relay_id) {
                    return Ok(());
                }
                let ack = self.import_block(Arc::new(*block), origin).await;
                // A gossiped block raises the height the peer claims to at most one past
                // our tip: the height of an orphan (even one queued behind a pending
                // parent) is unchecked, and such a header costs nothing to make.
                if matches!(
                    ack,
                    ImportAck::Queued | ImportAck::Duplicate | ImportAck::Deferred
                ) {
                    let next = self.status_rx.borrow().tip.height + 1;
                    if let Some(p) = self.lock().peers.get_mut(&peer_id) {
                        p.best_height = p.best_height.max(height.min(next));
                    }
                }
                match ack {
                    ImportAck::Orphan if height == 1 => {
                        warn!(peer = peer_id, "genesis mismatch at height 1");
                        Err(Disconnect::Closed)
                    }
                    ImportAck::Orphan => {
                        if !sync.outstanding && sync.awaiting.is_none() {
                            self.start_sync(write_tx, sync, height);
                        }
                        Ok(())
                    }
                    ImportAck::Invalid(reason) => {
                        self.lock().seen_blocks.mark_seen(relay_id);
                        Err(Disconnect::Ban(INVALID_BAN, reason))
                    }
                    // Nobody relays a block twice: fetch it from this peer in a moment.
                    ImportAck::Busy => {
                        if !sync.outstanding && sync.awaiting.is_none() && sync.retry.is_none() {
                            sync.retry = Some((TokioInstant::now() + BUSY_RETRY, height));
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            WireMessage::GossipJob(tx) => {
                let Ok(relay_id) = chain::job_relay_id(&tx) else {
                    return Ok(());
                };
                if !self.lock().seen_jobs.mark_seen(relay_id) {
                    return Ok(());
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                let cmd = MempoolCmd::Insert {
                    tx,
                    origin,
                    reply: reply_tx,
                };
                if self.mempool_tx.send(cmd).await.is_err() {
                    return Ok(());
                }
                if let Ok(Err(MempoolReject::Invalid { message, .. })) = reply_rx.await {
                    let mut guard = self.lock();
                    if let Some(p) = guard.peers.get_mut(&peer_id) {
                        p.invalid_jobs += 1;
                        if p.invalid_jobs >= MAX_INVALID_JOBS {
                            return Err(Disconnect::Ban(INVALID_BAN, message));
                        }
                    }
                }
                Ok(())
            }
            WireMessage::GossipShare(share) => {
                // Ignored and duplicate shares cost us checks but are free to make (no
                // signature or work is needed before they are dropped): budget them.
                // Past the budget the peer is throttled, not banned: an honest peer
                // that is syncing, or relaying to us while we catch up, sends many.
                if self.useless_share_budget_spent(peer_id) {
                    return Ok(());
                }
                let Ok(relay_id) = chain::share_relay_id(&share) else {
                    return Ok(());
                };
                if self.lock().seen_shares.contains(&relay_id) {
                    return Ok(());
                }
                let ack = self.import_share(share, origin).await;
                // Only settled outcomes are remembered: an ignored share (e.g. its parent
                // had not arrived yet) may be useful when relayed again later, and copies
                // of a pending one must still reach the chain actor in case its check is
                // cancelled. Accepted shares are recorded on ChainEvent::ShareAccepted.
                if matches!(ack, ShareAck::Duplicate | ShareAck::Invalid(_)) {
                    self.lock().seen_shares.mark_seen(relay_id);
                }
                if matches!(ack, ShareAck::Ignored(_) | ShareAck::Duplicate)
                    && let Some(p) = self.lock().peers.get_mut(&peer_id)
                {
                    p.useless_shares = p.useless_shares.saturating_add(1);
                }
                // Invalid shares get the peer banned here or, when the work check fails
                // later, through ChainEvent::ShareRejected.
                match ack {
                    ShareAck::Invalid(reason) => Err(Disconnect::Ban(INVALID_BAN, reason)),
                    _ => Ok(()),
                }
            }
            WireMessage::SyncRequest { from_height } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let cmd = ChainCmd::GetCanonicalRange {
                    from_height,
                    reply: reply_tx,
                };
                let blocks = if self.chain_tx.send(cmd).await.is_ok() {
                    reply_rx.await.unwrap_or_default()
                } else {
                    Vec::new()
                };
                let blocks = blocks.into_iter().map(|b| (*b).clone()).collect();
                // SyncResponse frames are never dropped (§4.5).
                let _ = write_tx.send(WireMessage::SyncResponse { blocks }).await;
                Ok(())
            }
            WireMessage::SyncResponse { blocks } => {
                self.handle_sync_response(origin, blocks, write_tx, sync)
                    .await
            }
        }
    }

    async fn handle_sync_response(
        &self,
        origin: Origin,
        blocks: Vec<Block>,
        write_tx: &mpsc::Sender<WireMessage>,
        sync: &mut SyncState,
    ) -> Result<(), Disconnect> {
        let Origin::Peer(peer_id, _) = origin else {
            return Ok(());
        };
        if !sync.outstanding {
            // Late answer to a request that already timed out: ignore it.
            debug!(peer = peer_id, "Ignoring unexpected SyncResponse");
            return Ok(());
        }
        sync.outstanding = false;
        sync.deadline = None;
        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else {
            // It has nothing from `from` on, whatever height it claimed.
            self.lower_best_height(peer_id, sync.from.saturating_sub(1));
            return Ok(());
        };
        let (first_height, last_id, last_height) = (
            first.header.context.height,
            last.header.block_id,
            last.header.context.height,
        );
        // An honest answer starts at the requested height; anything else shows nothing
        // about the height the peer claimed.
        if first_height != sync.from {
            self.lower_best_height(peer_id, sync.from.saturating_sub(1));
            return Ok(());
        }
        let encoded_len = codec::encode(&blocks).map_or(0, |b| b.len());
        let full = blocks.len() == MAX_SYNC_BLOCKS || encoded_len >= MAX_SYNC_BYTES;

        let mut first_ack = None;
        let mut nothing_new = true;
        for blk in blocks {
            let height = blk.header.context.height;
            let ack = self.import_block(Arc::new(blk), origin).await;
            match ack {
                ImportAck::Invalid(reason) => return Err(Disconnect::Ban(INVALID_BAN, reason)),
                // The rest would only be parked behind a block that is not pending: ask
                // again from here in a moment.
                ImportAck::Busy => {
                    sync.awaiting = None;
                    sync.retry = Some((TokioInstant::now() + BUSY_RETRY, height));
                    return Ok(());
                }
                _ => {}
            }
            nothing_new &= ack == ImportAck::Duplicate;
            first_ack.get_or_insert(ack);
        }
        // A short batch of blocks we already have: the peer has nothing beyond it.
        if nothing_new && !full {
            self.lower_best_height(peer_id, last_height);
        }

        if first_ack == Some(ImportAck::Orphan) {
            // Parent unknown and not pending: the fork point is lower (§4.7).
            if first_height == 1 {
                warn!(peer = peer_id, "genesis mismatch at height 1");
                return Err(Disconnect::Closed);
            }
            sync.lookback = sync.lookback.saturating_mul(2);
            if sync.lookback > MAX_REORG_DEPTH {
                warn!(peer = peer_id, "no common ancestor within reorg depth");
                return Err(Disconnect::Closed);
            }
            sync.from = sync.from.saturating_sub(sync.lookback).max(1);
            self.send_sync_request(write_tx, sync);
            return Ok(());
        }

        if full || self.peer_best_height(peer_id) > last_height {
            if self.has_block(last_id).await {
                sync.from = last_height + 1;
                self.send_sync_request(write_tx, sync);
            } else {
                // Verification is sequential: wait for the batch to be applied (§4.7).
                sync.awaiting = Some((last_id, last_height));
                sync.deadline = Some(TokioInstant::now() + SYNC_TIMEOUT);
            }
        }
        Ok(())
    }

    fn start_sync(
        &self,
        write_tx: &mpsc::Sender<WireMessage>,
        sync: &mut SyncState,
        hint_height: u64,
    ) {
        let our_best = self.status_rx.borrow().tip.height;
        sync.lookback = SYNC_LOOKBACK;
        sync.awaiting = None;
        sync.from = hint_height
            .min(our_best + 1)
            .saturating_sub(SYNC_LOOKBACK)
            .max(1);
        self.send_sync_request(write_tx, sync);
    }

    fn send_sync_request(&self, write_tx: &mpsc::Sender<WireMessage>, sync: &mut SyncState) {
        sync.outstanding = true;
        sync.deadline = Some(TokioInstant::now() + SYNC_TIMEOUT);
        let _ = write_tx.try_send(WireMessage::SyncRequest {
            from_height: sync.from,
        });
    }

    async fn has_block(&self, id: Digest) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .chain_tx
            .send(ChainCmd::HasBlock { id, reply: tx })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Whether the peer has sent its quota of useless shares this minute (or is gone).
    fn useless_share_budget_spent(&self, peer_id: PeerId) -> bool {
        let mut guard = self.lock();
        let Some(p) = guard.peers.get_mut(&peer_id) else {
            return true;
        };
        if p.useless_window.elapsed() >= Duration::from_secs(60) {
            p.useless_window = Instant::now();
            p.useless_shares = 0;
        }
        p.useless_shares >= MAX_USELESS_SHARES_PER_MIN
    }

    async fn import_share(&self, share: Box<ShareProof>, origin: Origin) -> ShareAck {
        let (tx, rx) = oneshot::channel();
        let cmd = ChainCmd::ImportShare {
            share,
            origin,
            ack: Some(tx),
        };
        if self.chain_tx.send(cmd).await.is_err() {
            return ShareAck::Duplicate;
        }
        rx.await.unwrap_or(ShareAck::Duplicate)
    }

    async fn import_block(&self, block: Arc<Block>, origin: Origin) -> ImportAck {
        let (tx, rx) = oneshot::channel();
        let cmd = ChainCmd::ImportBlock {
            block,
            origin,
            ack: Some(tx),
        };
        if self.chain_tx.send(cmd).await.is_err() {
            return ImportAck::Duplicate;
        }
        rx.await.unwrap_or(ImportAck::Duplicate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_of_any_version_are_recognised_by_their_prefix() {
        // A v3 Handshake: version, chain_id, best_height, best_block_id (88 bytes).
        let mut v3 = vec![0u8; 4];
        v3.extend_from_slice(&3u32.to_le_bytes());
        v3.extend_from_slice(&0x504C_4154u64.to_le_bytes());
        v3.extend_from_slice(&7u64.to_le_bytes());
        v3.extend_from_slice(&[9; 64]);
        assert!(
            codec::decode::<WireMessage>(&v3).is_err(),
            "v3 does not decode as v4"
        );
        assert_eq!(handshake_version(&v3), Some(3));

        let v4 = codec::encode(&WireMessage::Handshake {
            version: WIRE_VERSION,
            chain_id: 1,
            genesis_id: Digest::ZERO,
            node_nonce: 2,
            best_height: 3,
            best_block_id: Digest::ZERO,
        })
        .unwrap();
        assert_eq!(handshake_version(&v4), Some(WIRE_VERSION));
        let other = codec::encode(&WireMessage::SyncRequest { from_height: 1 }).unwrap();
        assert_eq!(handshake_version(&other), None);
        assert_eq!(handshake_version(&[0, 0, 0]), None);
    }
}
