//! P2P network manager: length-delimited TCP framing, handshake, gossip relay, sync
//! driver and misbehaviour bans (`node::net`, §4, §8.6).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant as TokioInstant;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, info, warn};
use wire::{
    Block, Digest, HANDSHAKE_TIMEOUT, MAX_FRAME_BYTES, MAX_REORG_DEPTH, MAX_SYNC_BLOCKS,
    MAX_SYNC_BYTES, SYNC_LOOKBACK, SYNC_TIMEOUT, WIRE_VERSION, WireMessage, codec,
};

use crate::chain_actor::{ChainCmd, ChainEvent, ImportAck, Origin, PeerId, StatusSnapshot};
use crate::mempool::{MempoolCmd, MempoolEvent, MempoolReject};

/// Ban for protocol violations (bad frames, unexpected messages), §4.8.
const PROTOCOL_BAN: Duration = Duration::from_secs(60);
/// Ban for invalid blocks or repeated invalid jobs, §4.8.
const INVALID_BAN: Duration = Duration::from_secs(600);
/// Invalid jobs tolerated per connection before a ban.
const MAX_INVALID_JOBS: u32 = 10;
/// Accepted blocks more than this far below the tip are not relayed: peers that
/// need them fetch them with `SyncRequest`.
const RELAY_DEPTH: u64 = 2 * SYNC_LOOKBACK;

/// What a ban applies to. Loopback peers are banned by full socket address, so one
/// misbehaving local node cannot lock every other local node out of a localhost testnet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BanKey {
    Ip(IpAddr),
    Addr(SocketAddr),
}

fn ban_key(addr: SocketAddr) -> BanKey {
    if addr.ip().is_loopback() {
        BanKey::Addr(addr)
    } else {
        BanKey::Ip(addr.ip())
    }
}

struct PeerHandle {
    addr: SocketAddr,
    outbound: bool,
    tx: mpsc::Sender<WireMessage>,
    best_height: u64,
    invalid_jobs: u32,
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
    bans: HashMap<BanKey, Instant>,
    /// Relay ids (`chain::block_relay_id`), not block ids: a copy with a forged
    /// signature has the honest `block_id` and must not shadow the genuine block.
    seen_blocks: LruSeen<Digest>,
    /// Relay ids (`chain::job_relay_id`), not tx ids, for the same reason.
    seen_jobs: LruSeen<Digest>,
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
    next_peer_id: AtomicU64,
    shared: Mutex<NetShared>,
    chain_tx: mpsc::Sender<ChainCmd>,
    mempool_tx: mpsc::Sender<MempoolCmd>,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    shutdown_rx: watch::Receiver<bool>,
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
        static_peers: Vec<SocketAddr>,
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
        let manager = Arc::new(Self {
            local_addr,
            chain_id,
            next_peer_id: AtomicU64::new(1),
            shared: Mutex::new(NetShared {
                peers: HashMap::new(),
                bans: HashMap::new(),
                seen_blocks: LruSeen::new(8_192),
                seen_jobs: LruSeen::new(16_384),
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
                        let conn = mgr.clone();
                        tokio::spawn(async move { conn.run_connection(stream, addr, false).await });
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

    /// Adds an outbound peer with its own reconnecting dialer.
    pub fn add_outbound_peer(self: &Arc<Self>, addr: SocketAddr) {
        self.spawn_dialer(addr);
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

    fn spawn_dialer(self: &Arc<Self>, peer_addr: SocketAddr) {
        if peer_addr == self.local_addr {
            return;
        }
        let mgr = self.clone();
        let mut sd = self.shutdown_rx.clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            while !is_shutdown(&sd) {
                if !mgr.has_peer_addr(peer_addr) && !mgr.is_banned_or_paused(peer_addr) {
                    match TcpStream::connect(peer_addr).await {
                        Ok(stream) => {
                            backoff_secs = 1;
                            mgr.run_connection(stream, peer_addr, true).await;
                        }
                        Err(_) => backoff_secs = (backoff_secs * 2).min(30),
                    }
                }
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    () = wait_shutdown(&mut sd) => break,
                }
            }
        });
    }

    fn is_banned_or_paused(&self, addr: SocketAddr) -> bool {
        let mut guard = self.lock();
        if guard.paused {
            return true;
        }
        let now = Instant::now();
        guard.bans.retain(|_, until| now < *until);
        guard.bans.contains_key(&ban_key(addr)) || guard.bans.contains_key(&BanKey::Addr(addr))
    }

    fn ban(&self, addr: SocketAddr, duration: Duration, reason: &str) {
        warn!(%addr, secs = duration.as_secs(), reason, "Banning peer");
        self.lock()
            .bans
            .insert(ban_key(addr), Instant::now() + duration);
    }

    fn has_peer_addr(&self, addr: SocketAddr) -> bool {
        self.lock().peers.values().any(|p| p.addr == addr)
    }

    fn peer_best_height(&self, peer_id: PeerId) -> u64 {
        self.lock().peers.get(&peer_id).map_or(0, |p| p.best_height)
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
                    Origin::Peer(pid) => Some(pid),
                    _ => None,
                };
                let tip_height = self.status_rx.borrow().tip.height;
                let height = block.header.context.height;
                if let Ok(relay_id) = chain::block_relay_id(&block.header) {
                    self.lock().seen_blocks.mark_seen(relay_id);
                }
                if height + RELAY_DEPTH >= tip_height {
                    let msg = WireMessage::GossipBlock(Box::new((*block).clone()));
                    self.broadcast(&msg, exclude);
                }
            }
            ChainEvent::BlockRejected {
                block_id,
                origin: Origin::Peer(pid),
                reason,
            } => {
                let addr = self.lock().peers.remove(&pid).map(|p| p.addr);
                if let Some(addr) = addr {
                    warn!(peer = pid, id = ?block_id, %reason, "Peer sent an invalid block");
                    self.ban(addr, INVALID_BAN, &reason);
                }
            }
            _ => {}
        }
    }

    fn handle_mempool_event(&self, ev: MempoolEvent) {
        let MempoolEvent::Accepted { tx, origin } = ev;
        let exclude = match origin {
            Origin::Peer(pid) => Some(pid),
            _ => None,
        };
        if let Ok(relay_id) = chain::job_relay_id(&tx) {
            self.lock().seen_jobs.mark_seen(relay_id);
        }
        self.broadcast(&WireMessage::GossipJob(tx), exclude);
    }

    async fn run_connection(&self, stream: TcpStream, addr: SocketAddr, outbound: bool) {
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
                best_height: snap.tip.height,
                best_block_id: snap.tip.id,
            }
        };
        let Ok(hs_bytes) = codec::encode(&our_hs) else {
            return;
        };
        if framed.send(Bytes::from(hs_bytes)).await.is_err() {
            return;
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
                    best_height,
                    best_block_id,
                }) => {
                    if version != WIRE_VERSION || chain_id != self.chain_id {
                        debug!(%addr, version, chain_id, "Handshake version or chain_id mismatch");
                        return;
                    }
                    (best_height, best_block_id)
                }
                _ => {
                    self.ban(addr, PROTOCOL_BAN, "first frame is not a valid Handshake");
                    return;
                }
            },
            Ok(Some(Err(e))) if e.kind() == std::io::ErrorKind::InvalidData => {
                self.ban(addr, PROTOCOL_BAN, "oversize frame");
                return;
            }
            _ => {
                debug!(%addr, "No handshake from peer");
                return;
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
                return;
            }
            guard.peers.insert(
                peer_id,
                PeerHandle {
                    addr,
                    outbound,
                    tx: write_tx.clone(),
                    best_height: peer_best_height,
                    invalid_jobs: 0,
                },
            );
        }
        info!(peer = peer_id, %addr, outbound, peer_best_height, "Connected peer");

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
                    if let Err(d) = self.handle_peer_message(peer_id, msg, &write_tx, &mut sync).await {
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
                    sync = SyncState::default();
                }
                _ = poll.tick() => {
                    if self.lock().paused {
                        break Disconnect::Closed;
                    }
                    if let Some((await_id, last_h)) = sync.awaiting
                        && self.has_block(await_id).await
                    {
                        sync.awaiting = None;
                        sync.from = last_h + 1;
                        self.send_sync_request(&write_tx, &mut sync);
                    }
                }
            }
        };

        write_task.abort();
        self.lock().peers.remove(&peer_id);
        match reason {
            Disconnect::Closed => debug!(peer = peer_id, %addr, "Disconnected peer"),
            Disconnect::Ban(duration, why) => self.ban(addr, duration, &why),
        }
    }

    async fn handle_peer_message(
        &self,
        peer_id: PeerId,
        msg: WireMessage,
        write_tx: &mpsc::Sender<WireMessage>,
        sync: &mut SyncState,
    ) -> Result<(), Disconnect> {
        match msg {
            WireMessage::Handshake { .. } => {
                Err(Disconnect::Ban(PROTOCOL_BAN, "second Handshake".into()))
            }
            WireMessage::GossipBlock(block) => {
                let height = block.header.context.height;
                let Ok(relay_id) = chain::block_relay_id(&block.header) else {
                    return Ok(());
                };
                {
                    let mut guard = self.lock();
                    if let Some(p) = guard.peers.get_mut(&peer_id) {
                        p.best_height = p.best_height.max(height);
                    }
                    if !guard.seen_blocks.mark_seen(relay_id) {
                        return Ok(());
                    }
                }
                match self.import_block(Arc::new(*block), peer_id).await {
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
                    ImportAck::Invalid(reason) => Err(Disconnect::Ban(INVALID_BAN, reason)),
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
                    origin: Origin::Peer(peer_id),
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
                self.handle_sync_response(peer_id, blocks, write_tx, sync)
                    .await
            }
        }
    }

    async fn handle_sync_response(
        &self,
        peer_id: PeerId,
        blocks: Vec<Block>,
        write_tx: &mpsc::Sender<WireMessage>,
        sync: &mut SyncState,
    ) -> Result<(), Disconnect> {
        if !sync.outstanding {
            // Late answer to a request that already timed out: ignore it.
            debug!(peer = peer_id, "Ignoring unexpected SyncResponse");
            return Ok(());
        }
        sync.outstanding = false;
        sync.deadline = None;
        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else {
            return Ok(());
        };
        let (first_height, last_id, last_height) = (
            first.header.context.height,
            last.header.block_id,
            last.header.context.height,
        );
        let encoded_len = codec::encode(&blocks).map_or(0, |b| b.len());
        let full = blocks.len() == MAX_SYNC_BLOCKS || encoded_len >= MAX_SYNC_BYTES;

        let mut first_ack = None;
        for blk in blocks {
            let ack = self.import_block(Arc::new(blk), peer_id).await;
            if let ImportAck::Invalid(reason) = ack {
                return Err(Disconnect::Ban(INVALID_BAN, reason));
            }
            first_ack.get_or_insert(ack);
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

    async fn import_block(&self, block: Arc<Block>, peer_id: PeerId) -> ImportAck {
        let (tx, rx) = oneshot::channel();
        let cmd = ChainCmd::ImportBlock {
            block,
            origin: Origin::Peer(peer_id),
            ack: Some(tx),
        };
        if self.chain_tx.send(cmd).await.is_err() {
            return ImportAck::Duplicate;
        }
        rx.await.unwrap_or(ImportAck::Duplicate)
    }
}
