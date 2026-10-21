//! ChainState actor (`node::chain_actor`, §8.2, §8.3).
//!
//! The only writer of chain state. It owns the `BlockTree`, runs the import pipeline
//! (§6.8), persists accepted blocks, publishes templates and status snapshots, and
//! never awaits another actor (§8.2).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chain::{
    BlockMeta, BlockSummary, BlockTree, ClaimedWork, Lottery, OwnerKeypair, ShareCheck,
    SignedShare, Template, TreeError, WorkError, block_relay_id, build_template, owner_key_id,
    share_relay_id, verify_fresh_work,
};
use ledger::StateCategory;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use wire::{
    Block, Digest, FwContext, GenesisConfig, MAX_FUTURE_SECS, MAX_SYNC_BLOCKS, MAX_SYNC_BYTES,
    ORPHAN_LIMIT, ShareProof, Target, codec,
};

use crate::mempool::MempoolView;
use crate::share_pool::{SHARE_POOL_CAPACITY, SharePool};
use crate::store::BlockLogWriter;

/// Maximum gossiped shares awaiting their work check (one tile recomputation each),
/// parked or running. Parked shares count too, so releasing them never exceeds it.
const SHARE_CHECK_LIMIT: usize = 64;
/// Per source: shares parked or awaiting their work check. Honest gossip stays far below.
const SHARE_SOURCE_LIMIT: usize = 8;

/// Blocks from one source (see [`Source`]) in Stage A at once. Honest peers relay
/// verified blocks one at a time and a sync batch is verified block after block
/// (children wait, parked), so this only bounds how much one address can make us
/// recompute before its first invalid block gets it banned.
const STAGE_A_SOURCE_LIMIT: usize = 2;
/// Blocks in Stage A at once, from all sources.
const STAGE_A_LIMIT: usize = 16;
/// Per source: blocks that passed Stage B and wait for a Stage A slot.
const STAGE_A_QUEUE_SOURCE_LIMIT: usize = 8;
/// Blocks waiting for a Stage A slot, from all sources.
const STAGE_A_QUEUE_LIMIT: usize = 64;
/// Per source: blocks kept for a retry because their time_bucket is in the future.
const DEFERRED_SOURCE_LIMIT: usize = 8;
/// Blocks kept for a retry because their time_bucket is in the future, in total.
const DEFERRED_LIMIT: usize = 64;
/// Per source: blocks parked until their parent arrives (of `ORPHAN_LIMIT` in total).
/// A sync batch parks all but its first block, so a source may hold a full batch.
const ORPHAN_SOURCE_LIMIT: usize = MAX_SYNC_BLOCKS;
/// A block further ahead than `MAX_FUTURE_SECS` plus this is not kept for a retry.
const DEFERRED_SLACK_SECS: u64 = 60;
/// Other sources remembered per pending block: if the source that delivered it first
/// is banned, the block stays pending on behalf of one of them.
const CO_SENDERS: usize = 3;
/// Ban for a peer that sent an invalid block or share (§4.8). Net bans the source for
/// this long, and the chain actor admits nothing new from it meanwhile.
pub const INVALID_BAN: Duration = Duration::from_secs(600);

/// A pending share's slot: (owner key id, work_id). Keyed by owner too because only
/// the work check ties a work_id to its owner: a validly signed claim to another
/// owner's work_id must not hold that owner's slot.
type ShareSlot = (Digest, Digest);

/// Identifier of a connected peer.
pub type PeerId = u64;

/// Where a peer connects from. Bans, and the limits on what may await verification,
/// apply per source: an IP address (for IPv6 its /64 prefix, which one host commonly
/// holds whole), or the full socket address for loopback peers, so that the nodes of a
/// local testnet stay apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A remote IP address (IPv6: the /64 prefix).
    Ip(IpAddr),
    /// A loopback socket address.
    Addr(SocketAddr),
}

impl Source {
    /// The source of a connection from `addr`.
    pub fn of(addr: SocketAddr) -> Self {
        let ip = match addr.ip() {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        match ip {
            ip if ip.is_loopback() => Self::Addr(addr),
            IpAddr::V6(v6) => {
                let [a, b, c, d, ..] = v6.segments();
                Self::Ip(IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0)))
            }
            v4 => Self::Ip(v4),
        }
    }
}

/// Origin of a block or transaction (§8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Mined by this node.
    Local,
    /// Submitted through the RPC.
    Rpc,
    /// Received from a peer, connected from a source.
    Peer(PeerId, Source),
    /// Replayed from `blocks.log`.
    Disk,
}

impl Origin {
    /// The source of a peer origin.
    pub fn source(&self) -> Option<Source> {
        match self {
            Origin::Peer(_, source) => Some(*source),
            _ => None,
        }
    }
}

/// Immediate acknowledgment of `ChainCmd::ImportBlock`, sent before Stage A runs (§8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportAck {
    /// Accepted for processing (Stage A pending, or waiting for a pending parent).
    Queued,
    /// Already known, in flight, parked or rejected before.
    Duplicate,
    /// The parent is unknown and not pending: the sender should sync.
    Orphan,
    /// `time_bucket` too far in the future; retried later.
    Deferred,
    /// Violates a consensus rule.
    Invalid(String),
    /// Forks deeper than `MAX_REORG_DEPTH`.
    TooDeep,
    /// Refused for now: too many blocks from this source (or in total) await
    /// verification. Not remembered, so a copy can be imported later.
    Busy,
    /// Its time_bucket is too far in the future even to keep for a retry. Not remembered.
    TooEarly,
}

/// Immediate acknowledgment of `ChainCmd::ImportShare`, sent before the work check runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareAck {
    /// Accepted for verification.
    Queued,
    /// Waiting for its parent block, which is being verified. The share is dropped
    /// without penalty if that block turns out invalid, so it may be relayed again.
    Parked,
    /// These exact bytes already await verification (another peer relayed them first).
    Pending,
    /// Already pooled, registered or rejected, or the share is pending under other bytes.
    Duplicate,
    /// Of no use here (stale, unknown parent, ...); the sender is not penalized.
    Ignored(&'static str),
    /// No honest node relays this share; the sender is penalized.
    Invalid(String),
}

/// Commands accepted by `ChainActor` (§8.2).
pub enum ChainCmd {
    /// Imports a block from a peer, the miner or disk.
    ImportBlock {
        /// The block.
        block: Arc<Block>,
        /// Where it came from.
        origin: Origin,
        /// Immediate acknowledgment.
        ack: Option<oneshot::Sender<ImportAck>>,
    },
    /// A block sealed by the local miner.
    SubmitLocalBlock(Box<Block>),
    /// A share found by the local miner: pooled and gossiped to every peer.
    SubmitLocalShare(Box<ShareProof>),
    /// A share gossiped by a peer.
    ImportShare {
        /// The share.
        share: Box<ShareProof>,
        /// Where it came from.
        origin: Origin,
        /// Immediate acknowledgment.
        ack: Option<oneshot::Sender<ShareAck>>,
    },
    /// Internal: the work check of a gossiped share finished.
    ShareVerified {
        /// Identifies this check among the slot's checks.
        ticket: u64,
        /// The share's (owner key id, work_id).
        slot: (Digest, Digest),
        /// The share's relay id.
        relay_id: Digest,
        /// Where it came from.
        origin: Origin,
        /// The verified share, or why it was not.
        result: Result<Box<SignedShare>, CheckFailure>,
    },
    /// Internal: Stage A finished for a block.
    WorkVerified {
        /// Identifies this Stage A task among the block's.
        ticket: u64,
        /// The verified block.
        block: Arc<Block>,
        /// Where it came from.
        origin: Origin,
        /// Verification outcome.
        result: Result<(), WorkError>,
    },
    /// Drops what a banned source sent that still awaits verification: its Stage A
    /// tasks and share work checks are aborted and its queued, parked and deferred
    /// blocks and parked shares dropped, except blocks another source delivered too.
    /// Nothing new is admitted from it until the ban ends.
    CancelSource {
        /// The banned source.
        source: Source,
        /// How long it is banned.
        ban: Duration,
    },
    /// Canonical blocks from `from_height` within the `SyncResponse` limits (§4.4).
    GetCanonicalRange {
        /// First height (`>= 1`).
        from_height: u64,
        /// Blocks, ascending.
        reply: oneshot::Sender<Vec<Arc<Block>>>,
    },
    /// Canonical block summaries below `before`, newest first (§9.4).
    GetBlocks {
        /// Exclusive upper height bound (default: tip + 1).
        before: Option<u64>,
        /// Maximum summaries (capped at 100).
        limit: usize,
        /// Summaries.
        reply: oneshot::Sender<Vec<BlockSummary>>,
    },
    /// Whether a block is in the tree.
    HasBlock {
        /// Block id.
        id: Digest,
        /// Answer.
        reply: oneshot::Sender<bool>,
    },
    /// The subset of `tx_ids` that the canonical chain already includes.
    FilterIncluded {
        /// Candidates.
        tx_ids: Vec<Digest>,
        /// Included ones.
        reply: oneshot::Sender<Vec<Digest>>,
    },
    /// Flushes and fsyncs `blocks.log`.
    FlushAndClose {
        /// Sent once the log is synced.
        reply: oneshot::Sender<()>,
    },
}

/// Events broadcast by `ChainActor` (§8.2).
#[derive(Clone, Debug)]
pub enum ChainEvent {
    /// A block was inserted (any branch); peers relay it.
    BlockAccepted {
        /// The block.
        block: Arc<Block>,
        /// Where it came from.
        origin: Origin,
    },
    /// A block was rejected as invalid; the origin peer is penalized.
    BlockRejected {
        /// The block.
        block_id: Digest,
        /// Where it came from.
        origin: Origin,
        /// Why.
        reason: String,
    },
    /// A share entered the pool (own or verified gossip); peers relay it.
    ShareAccepted {
        /// The share.
        share: Arc<ShareProof>,
        /// Where it came from.
        origin: Origin,
    },
    /// A gossiped share was rejected as invalid; the origin peer is penalized.
    ShareRejected {
        /// Where it came from.
        origin: Origin,
        /// Why.
        reason: String,
    },
    /// The canonical tip changed.
    TipChanged {
        /// New tip (boxed: it carries the 64-byte miner key id).
        tip: Box<BlockMeta>,
        /// Blocks that joined the canonical chain, ascending.
        connected: Vec<Arc<Block>>,
        /// Blocks that left it, old tip first.
        disconnected: Vec<Arc<Block>>,
    },
}

/// Status snapshot published via `watch` for `/api/status` and `Handshake` (§8.3, §9.3).
#[derive(Clone, Debug)]
pub struct StatusSnapshot {
    /// `genesis.name`.
    pub chain_name: String,
    /// `genesis.chain_id`.
    pub chain_id: u64,
    /// `genesis_id`.
    pub genesis_id: Digest,
    /// `genesis.blocks_per_epoch`.
    pub blocks_per_epoch: u64,
    /// `genesis.target_block_secs`.
    pub target_block_secs: u64,
    /// `genesis.max_jobs_per_block`.
    pub max_jobs_per_block: u64,
    /// Key id of this node's ML-DSA-87 miner key.
    pub miner_key_id: Digest,
    /// The 64-bit `OwnerId` derived from `miner_key_id`.
    pub owner_id: u64,
    /// When the actor started.
    pub start_instant: Instant,
    /// Tip metadata.
    pub tip: BlockMeta,
    /// Ledger epoch at the tip.
    pub epoch: u64,
    /// The 8 category balances in `StateCategory::ALL` order.
    pub balances: [u64; StateCategory::COUNT],
    /// Their sum.
    pub total: u128,
    /// Founder tranches still locked at the tip height.
    pub founder_locked: u64,
    /// Segregated rounding dust.
    pub segregated_dust: u64,
    /// Lots in `R_a`.
    pub fee_lots: usize,
    /// Fees still locked in `R_l`.
    pub locked_fees: usize,
    /// Booking to be paid at the next epoch boundary.
    pub pending_payout: u64,
    /// Admitted, unsettled jobs.
    pub open_jobs: usize,
    /// Parked blocks waiting for their parent.
    pub orphan_count: usize,
    /// Verified shares waiting for a block (own and gossiped).
    pub share_pool: usize,
    /// Blocks in Stage A.
    pub verifying_blocks: usize,
    /// Blocks that passed Stage B and wait for a Stage A slot.
    pub queued_blocks: usize,
    /// Blocks kept for a retry because their time_bucket is ahead of the local clock.
    pub deferred_blocks: usize,
    /// Blocks that entered Stage A since the node started.
    pub stage_a_started: u64,
}

/// Counters of the import pipeline shown in the status snapshot.
#[derive(Clone, Copy, Debug, Default)]
struct PipelineCounts {
    orphans: usize,
    share_pool: usize,
    verifying_blocks: usize,
    queued_blocks: usize,
    deferred_blocks: usize,
    stage_a_started: u64,
}

/// How a block reaches the import pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// It just arrived.
    Fresh,
    /// It was parked until its parent arrived. The orphan limit bounded it, so the Stage A
    /// queue limits do not refuse it, but it counts against the deferred limits.
    Parked,
    /// It was deferred and is due now; it was already counted against every limit.
    Retry,
}

/// A block in Stage A. `origin` is the source it is verified for, `also` the other
/// sources that delivered the same bytes (see [`CO_SENDERS`]).
struct StageA {
    ticket: u64,
    block_id: Digest,
    origin: Origin,
    also: Vec<Origin>,
    task: AbortHandle,
}

/// A block that passed Stage B and waits for a Stage A slot.
struct QueuedBlock {
    block: Arc<Block>,
    relay_id: Digest,
    origin: Origin,
    also: Vec<Origin>,
}

/// Why a share's work check did not produce a verified share.
#[derive(Debug)]
pub enum CheckFailure {
    /// The share is invalid; its sender is penalized.
    Invalid(String),
    /// The check could not run (a task panicked or was cancelled); no one is penalized.
    Internal(String),
}

/// A gossiped share's work check in flight.
struct ShareCheckTask {
    ticket: u64,
    origin: Origin,
    relay_id: Digest,
    task: AbortHandle,
}

/// Where a parked block sits: in `parked_by_parent[parent]`, sent by `source`.
struct ParkedRef {
    relay_id: Digest,
    block_id: Digest,
    parent: Digest,
    source: Option<Source>,
}

struct ParkedBlock {
    block: Arc<Block>,
    relay_id: Digest,
    origin: Origin,
    also: Vec<Origin>,
}

/// A gossiped share whose signature verified, waiting for its parent's Stage A.
struct ParkedShare {
    parent: Digest,
    slot: ShareSlot,
    signed: Box<SignedShare>,
    relay_id: Digest,
    origin: Origin,
}

struct DeferredEntry {
    block: Arc<Block>,
    relay_id: Digest,
    origin: Origin,
    also: Vec<Origin>,
    expires_at: Instant,
}

struct LruIdCache {
    order: VecDeque<Digest>,
    set: HashSet<Digest>,
    capacity: usize,
}

impl LruIdCache {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    fn contains(&self, id: &Digest) -> bool {
        self.set.contains(id)
    }

    fn insert(&mut self, id: Digest) {
        if self.set.insert(id) {
            if self.order.len() >= self.capacity
                && let Some(old) = self.order.pop_front()
            {
                self.set.remove(&old);
            }
            self.order.push_back(id);
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Asynchronous ChainState actor (§8.3).
pub struct ChainActor {
    tree: BlockTree,
    miner: Arc<OwnerKeypair>,
    log_writer: BlockLogWriter,
    self_tx: mpsc::Sender<ChainCmd>,
    cmd_rx: mpsc::Receiver<ChainCmd>,
    event_tx: broadcast::Sender<ChainEvent>,
    template_tx: watch::Sender<Option<Arc<Template>>>,
    status_tx: watch::Sender<Arc<StatusSnapshot>>,
    mempool_view: watch::Receiver<Arc<MempoolView>>,
    verify_sem: Arc<Semaphore>,
    // Shares get their own, smaller verification budget, so a block's Stage A never
    // queues behind share gossip.
    share_verify_sem: Arc<Semaphore>,
    start_instant: Instant,
    template_gen: u64,
    share_pool: SharePool,
    // Work checks in flight by slot. Only a share whose owner signature verified can
    // take a slot, so a forged copy cannot shadow the genuine share, and re-signed
    // copies of a pending share (ML-DSA signing is randomized) are dropped before
    // costing a verification.
    share_checks: HashMap<ShareSlot, ShareCheckTask>,
    parked_shares: VecDeque<ParkedShare>, // oldest first
    invalid_shares: LruIdCache,           // relay ids
    // Blocks awaiting validation are deduplicated by relay id (`chain::block_relay_id`),
    // not by block_id: a copy with a forged signature or body shares the honest block_id
    // and must not shadow, or get cached as invalid in place of, the genuine block.
    // Each source has at most STAGE_A_SOURCE_LIMIT blocks in Stage A; the rest wait in
    // stage_a_queue, oldest first, and are started as slots free up.
    in_flight: HashMap<Digest, StageA>, // by relay_id
    stage_a_queue: VecDeque<QueuedBlock>,
    stage_a_started: u64,
    next_ticket: u64,
    // Sources whose work was cancelled (banned), until their ban ends.
    banned: HashMap<Source, Instant>,
    parked_by_parent: HashMap<Digest, Vec<ParkedBlock>>,
    parked_order: VecDeque<ParkedRef>, // oldest first
    deferred: Vec<DeferredEntry>,
    invalid_lru: LruIdCache, // relay ids
}

impl ChainActor {
    /// Builds the actor at genesis and returns it with the status receiver.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        genesis: GenesisConfig,
        miner: Arc<OwnerKeypair>,
        log_writer: BlockLogWriter,
        self_tx: mpsc::Sender<ChainCmd>,
        cmd_rx: mpsc::Receiver<ChainCmd>,
        event_tx: broadcast::Sender<ChainEvent>,
        template_tx: watch::Sender<Option<Arc<Template>>>,
        mempool_view: watch::Receiver<Arc<MempoolView>>,
        verify_sem: Arc<Semaphore>,
    ) -> Result<(Self, watch::Receiver<Arc<StatusSnapshot>>), String> {
        let tree = BlockTree::new(genesis)?;
        let share_verify_sem =
            Arc::new(Semaphore::new((verify_sem.available_permits() / 2).max(1)));
        let start_instant = Instant::now();
        let status = build_status_snapshot(&tree, &miner, start_instant, PipelineCounts::default());
        let (status_tx, status_rx) = watch::channel(Arc::new(status));
        let mut actor = Self {
            tree,
            miner,
            log_writer,
            self_tx,
            cmd_rx,
            event_tx,
            template_tx,
            status_tx,
            mempool_view,
            verify_sem,
            share_verify_sem,
            start_instant,
            template_gen: 0,
            share_pool: SharePool::new(SHARE_POOL_CAPACITY),
            share_checks: HashMap::new(),
            parked_shares: VecDeque::new(),
            invalid_shares: LruIdCache::new(4_096),
            in_flight: HashMap::new(),
            stage_a_queue: VecDeque::new(),
            stage_a_started: 0,
            next_ticket: 0,
            banned: HashMap::new(),
            parked_by_parent: HashMap::new(),
            parked_order: VecDeque::new(),
            deferred: Vec::new(),
            invalid_lru: LruIdCache::new(4_096),
        };
        actor.rebuild_and_publish_template();
        Ok((actor, status_rx))
    }

    /// Replays persisted blocks at startup (`Origin::Disk`, §8.7). Stage A is skipped, so
    /// tiles are not recomputed, but `apply_verified_block` still verifies the miner and
    /// share signatures before any owner enters the state.
    pub fn replay_disk_blocks(&mut self, blocks: Vec<Block>) {
        let total = blocks.len();
        let mut applied = 0usize;
        for blk in blocks {
            let block = Arc::new(blk);
            let result = self
                .tree
                .pre_validate(&block, None)
                .and_then(|()| self.tree.apply_verified_block(block.clone()));
            match result {
                Ok(_) => applied += 1,
                Err(e) => {
                    warn!(id = ?block.header.block_id, error = %e, "Skipping block during disk replay")
                }
            }
        }
        if total > 0 {
            info!(
                applied,
                total,
                height = self.tree.tip_meta().height,
                "Replayed blocks.log"
            );
        }
        self.publish_status();
        self.rebuild_and_publish_template();
    }

    /// Runs the actor until `shutdown` turns true; flushes `blocks.log` on exit.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut next_tick = TokioInstant::now() + Duration::from_secs(5);
        let mut debounce: Option<TokioInstant> = None;
        loop {
            let wake = debounce.map_or(next_tick, |d| d.min(next_tick));
            tokio::select! {
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    if self.handle_cmd(cmd) && debounce.is_none() {
                        debounce = Some(TokioInstant::now() + Duration::from_millis(500));
                    }
                }
                Ok(()) = self.mempool_view.changed() => {
                    if debounce.is_none() {
                        debounce = Some(TokioInstant::now() + Duration::from_millis(500));
                    }
                }
                () = tokio::time::sleep_until(wake) => {
                    let now = TokioInstant::now();
                    if debounce.is_some_and(|d| now >= d) {
                        debounce = None;
                        self.rebuild_and_publish_template();
                    }
                    if now >= next_tick {
                        next_tick = now + Duration::from_secs(5);
                        self.retry_deferred();
                        self.rebuild_and_publish_template();
                    }
                }
            }
        }
        if let Err(e) = self.log_writer.flush_and_sync() {
            warn!(error = %e, "Failed to sync blocks.log on shutdown");
        }
        debug!("ChainState actor stopped");
    }

    /// Handles one command; returns true when a debounced template rebuild is due.
    fn handle_cmd(&mut self, cmd: ChainCmd) -> bool {
        match cmd {
            ChainCmd::ImportBlock { block, origin, ack } => {
                let res = self.import_block(block, origin);
                if let Some(tx) = ack {
                    let _ = tx.send(res);
                }
            }
            ChainCmd::SubmitLocalBlock(block) => {
                let ack = self.import_block(Arc::new(*block), Origin::Local);
                if !matches!(ack, ImportAck::Queued) {
                    debug!(?ack, "Local block not accepted");
                }
            }
            ChainCmd::SubmitLocalShare(share) => {
                // Our own template's work, Freivalds-checked by the miner: only the
                // signature check that every pooled share carries.
                match SignedShare::verify(*share) {
                    Ok(signed) => return self.admit_share(signed, Origin::Local),
                    Err(e) => warn!(error = %e, "Local share has an invalid signature"),
                }
            }
            ChainCmd::ImportShare { share, origin, ack } => {
                let res = self.import_share(share, origin);
                if let Some(tx) = ack {
                    let _ = tx.send(res);
                }
            }
            ChainCmd::ShareVerified {
                ticket,
                slot,
                relay_id,
                origin,
                result,
            } => {
                if self
                    .share_checks
                    .get(&slot)
                    .is_some_and(|c| c.ticket == ticket)
                {
                    self.share_checks.remove(&slot);
                }
                match result {
                    Ok(signed) => return self.admit_share(*signed, origin),
                    Err(CheckFailure::Invalid(reason)) => {
                        self.reject_share(relay_id, origin, reason)
                    }
                    Err(CheckFailure::Internal(e)) => {
                        warn!(error = %e, "Share work check could not run; share dropped")
                    }
                }
            }
            ChainCmd::WorkVerified {
                ticket,
                block,
                origin,
                result,
            } => self.handle_work_verified(ticket, block, origin, result),
            ChainCmd::CancelSource { source, ban } => self.cancel_source(source, ban),
            ChainCmd::GetCanonicalRange { from_height, reply } => {
                let _ = reply.send(self.canonical_range(from_height));
            }
            ChainCmd::GetBlocks {
                before,
                limit,
                reply,
            } => {
                let _ = reply.send(self.tree.canonical_summaries(before, limit.min(100)));
            }
            ChainCmd::HasBlock { id, reply } => {
                let _ = reply.send(self.tree.has_block(&id));
            }
            ChainCmd::FilterIncluded { tx_ids, reply } => {
                let included = tx_ids
                    .into_iter()
                    .filter(|id| self.tree.is_tx_on_canonical(id))
                    .collect();
                let _ = reply.send(included);
            }
            ChainCmd::FlushAndClose { reply } => {
                if let Err(e) = self.log_writer.flush_and_sync() {
                    warn!(error = %e, "Failed to sync blocks.log");
                }
                let _ = reply.send(());
            }
        }
        false
    }

    fn canonical_range(&self, from_height: u64) -> Vec<Arc<Block>> {
        let mut blocks = Vec::new();
        if from_height == 0 {
            return blocks;
        }
        let mut total_bytes = 0usize;
        let tip_h = self.tree.tip_meta().height;
        for h in from_height..=tip_h {
            if blocks.len() >= MAX_SYNC_BLOCKS {
                break;
            }
            let Some(blk) = self.tree.canonical_block_at(h) else {
                break;
            };
            let size = codec::encode(&*blk).map_or(0, |b| b.len());
            if !blocks.is_empty() && total_bytes.saturating_add(size) > MAX_SYNC_BYTES {
                break;
            }
            total_bytes = total_bytes.saturating_add(size);
            blocks.push(blk);
        }
        blocks
    }

    /// Whether a block with this block_id passed Stage B and is in, or waits for, Stage A.
    fn is_in_stage_a(&self, block_id: &Digest) -> bool {
        self.in_flight.values().any(|s| s.block_id == *block_id)
            || self
                .stage_a_queue
                .iter()
                .any(|q| q.block.header.block_id == *block_id)
    }

    /// Whether a block with this block_id awaits validation (a parent check).
    fn is_pending_block(&self, block_id: &Digest) -> bool {
        self.is_in_stage_a(block_id)
            || self.parked_order.iter().any(|r| r.block_id == *block_id)
            || self
                .deferred
                .iter()
                .any(|d| d.block.header.block_id == *block_id)
    }

    /// Whether these exact block bytes await validation.
    fn is_pending_relay(&self, relay_id: &Digest) -> bool {
        self.in_flight.contains_key(relay_id)
            || self.stage_a_queue.iter().any(|q| q.relay_id == *relay_id)
            || self.parked_order.iter().any(|r| r.relay_id == *relay_id)
            || self.deferred.iter().any(|d| d.relay_id == *relay_id)
    }

    /// Imports a block that just arrived (from a peer, the miner or disk).
    fn import_block(&mut self, block: Arc<Block>, origin: Origin) -> ImportAck {
        self.admit_block(block, origin, Vec::new(), Admission::Fresh)
    }

    /// The import pipeline (§6.8). `also` lists other sources that delivered the same
    /// bytes; `how` says whether this node held the block already (see [`Admission`]).
    fn admit_block(
        &mut self,
        block: Arc<Block>,
        origin: Origin,
        also: Vec<Origin>,
        how: Admission,
    ) -> ImportAck {
        let block_id = block.header.block_id;
        let relay_id = match block_relay_id(&block) {
            Ok(id) => id,
            Err(e) => return ImportAck::Invalid(format!("unencodable header: {e}")),
        };
        // Until net closes its connections, a banned source may still deliver blocks;
        // and its blocks released by a parent in the same turn it was banned are dropped.
        if self.is_banned(origin) {
            return ImportAck::Busy;
        }
        if self.is_pending_relay(&relay_id) {
            self.note_co_sender(&relay_id, origin);
            return ImportAck::Duplicate;
        }
        if self.tree.has_block(&block_id) || self.invalid_lru.contains(&relay_id) {
            return ImportAck::Duplicate;
        }

        match self.tree.pre_validate(&block, Some(unix_now_secs())) {
            Ok(()) => {
                if matches!(origin, Origin::Local | Origin::Disk) {
                    self.finalize_block(block, origin);
                    ImportAck::Queued
                } else {
                    let entry = QueuedBlock {
                        block,
                        relay_id,
                        origin,
                        also,
                    };
                    self.queue_stage_a(entry, how == Admission::Fresh)
                }
            }
            Err(TreeError::Duplicate) => ImportAck::Duplicate,
            Err(TreeError::Orphan { parent }) => {
                let parent_pending = self.is_pending_block(&parent);
                let entry = ParkedBlock {
                    block,
                    relay_id,
                    origin,
                    also,
                };
                self.park(parent, entry);
                self.publish_status();
                if parent_pending {
                    ImportAck::Queued
                } else {
                    ImportAck::Orphan
                }
            }
            Err(TreeError::Deferred { time_bucket }) => {
                let entry = DeferredEntry {
                    block,
                    relay_id,
                    origin,
                    also,
                    expires_at: Instant::now() + Duration::from_secs(600),
                };
                self.defer(entry, time_bucket, how != Admission::Retry)
            }
            Err(TreeError::TooDeep) => ImportAck::TooDeep,
            Err(TreeError::Invalid(reason)) => {
                self.reject(block_id, relay_id, origin, reason.clone());
                ImportAck::Invalid(reason)
            }
        }
    }

    /// Whether `origin` is a peer whose source is banned.
    fn is_banned(&self, origin: Origin) -> bool {
        origin
            .source()
            .and_then(|s| self.banned.get(&s))
            .is_some_and(|until| *until > Instant::now())
    }

    /// Records that `origin` delivered bytes already pending for another source, so the
    /// block stays pending if that source is banned.
    fn note_co_sender(&mut self, relay_id: &Digest, origin: Origin) {
        let Some(source) = origin.source() else {
            return;
        };
        let add = |owner: Origin, also: &mut Vec<Origin>| {
            if owner.source() != Some(source)
                && also.len() < CO_SENDERS
                && !also.iter().any(|o| o.source() == Some(source))
            {
                also.push(origin);
            }
        };
        if let Some(s) = self.in_flight.get_mut(relay_id) {
            add(s.origin, &mut s.also);
        } else if let Some(q) = self
            .stage_a_queue
            .iter_mut()
            .find(|q| q.relay_id == *relay_id)
        {
            add(q.origin, &mut q.also);
        } else if let Some(d) = self.deferred.iter_mut().find(|d| d.relay_id == *relay_id) {
            add(d.origin, &mut d.also);
        } else if let Some(c) = self
            .parked_by_parent
            .values_mut()
            .flat_map(|list| list.iter_mut())
            .find(|c| c.relay_id == *relay_id)
        {
            add(c.origin, &mut c.also);
        }
    }

    /// Keeps a block whose time_bucket is ahead of our clock for a retry, unless it is
    /// too far ahead or, when `limited`, too many are kept already.
    fn defer(&mut self, entry: DeferredEntry, time_bucket: u64, limited: bool) -> ImportAck {
        let genesis = self.tree.genesis();
        let latest = (unix_now_secs() / genesis.time_bucket_secs)
            .saturating_add((MAX_FUTURE_SECS + DEFERRED_SLACK_SECS) / genesis.time_bucket_secs);
        if time_bucket > latest {
            return ImportAck::TooEarly;
        }
        if limited {
            let source = entry.origin.source();
            let from_source = self
                .deferred
                .iter()
                .filter(|d| d.origin.source() == source)
                .count();
            if self.deferred.len() >= DEFERRED_LIMIT || from_source >= DEFERRED_SOURCE_LIMIT {
                return ImportAck::Busy;
            }
        }
        self.deferred.push(entry);
        ImportAck::Deferred
    }

    /// Caches the rejected block's relay id (never its block_id, which an invalid copy
    /// of an honest block can share) and reports the rejection. A peer that sent an
    /// invalid block gets its source banned, so everything else from that source that
    /// awaits verification is dropped at once, before any more of it can start.
    fn reject(&mut self, block_id: Digest, relay_id: Digest, origin: Origin, reason: String) {
        debug!(id = ?block_id, ?origin, %reason, "Rejected block");
        self.invalid_lru.insert(relay_id);
        let _ = self.event_tx.send(ChainEvent::BlockRejected {
            block_id,
            origin,
            reason,
        });
        if let Some(source) = origin.source() {
            self.cancel_source(source, INVALID_BAN);
        }
    }

    /// Admits a gossiped share for verification, cheapest checks first: dedupe, the
    /// no-work checks, per-peer and global limits, then the owner signature here on the
    /// actor (well under a millisecond). The tile recomputation follows off the actor
    /// (`ChainCmd::ShareVerified`), or once the parent block passes Stage A if the share
    /// arrived while it was being verified.
    fn import_share(&mut self, share: Box<ShareProof>, origin: Origin) -> ShareAck {
        let relay_id = match share_relay_id(&share) {
            Ok(id) => id,
            Err(e) => return ShareAck::Invalid(format!("unencodable share: {e}")),
        };
        if self.is_banned(origin) {
            return ShareAck::Ignored("the sender is banned");
        }
        if self.share_checks.values().any(|c| c.relay_id == relay_id)
            || self.parked_shares.iter().any(|p| p.relay_id == relay_id)
        {
            return ShareAck::Pending;
        }
        // The pool holds work-checked shares only, so a work_id there is settled.
        let slot = (owner_key_id(&share.owner_key), share.work_id);
        if self.share_pool.contains(&share.work_id)
            || self.share_checks.contains_key(&slot)
            || self.parked_shares.iter().any(|p| p.slot == slot)
            || self.invalid_shares.contains(&relay_id)
        {
            return ShareAck::Duplicate;
        }
        let parent = match self.tree.check_gossip_share(&share) {
            ShareCheck::Verify => None,
            // Shares follow their parent closely: wait if it is in Stage A, so it has
            // passed Stage B. A parked orphan block's id is unauthenticated and does not
            // qualify.
            ShareCheck::UnknownParent(parent) if self.is_in_stage_a(&parent) => Some(parent),
            ShareCheck::UnknownParent(_) => return ShareAck::Ignored("share parent is unknown"),
            ShareCheck::Ignore(why) => return ShareAck::Ignored(why),
            ShareCheck::Invalid(reason) => {
                self.reject_share(relay_id, origin, reason.clone());
                return ShareAck::Invalid(reason);
            }
        };
        if self.share_checks.len() + self.parked_shares.len() >= SHARE_CHECK_LIMIT
            || self.source_share_load(origin) >= SHARE_SOURCE_LIMIT
        {
            return ShareAck::Ignored("too many shares awaiting verification");
        }
        let signed = match SignedShare::verify(*share) {
            Ok(signed) => Box::new(signed),
            Err(e) => {
                let reason = format!("share signature: {e}");
                self.reject_share(relay_id, origin, reason.clone());
                return ShareAck::Invalid(reason);
            }
        };
        match parent {
            None => {
                self.spawn_share_work_check(slot, signed, relay_id, origin);
                ShareAck::Queued
            }
            Some(parent) => {
                self.parked_shares.push_back(ParkedShare {
                    parent,
                    slot,
                    signed,
                    relay_id,
                    origin,
                });
                ShareAck::Parked
            }
        }
    }

    /// Shares from `origin`'s source that are parked or awaiting their work check.
    fn source_share_load(&self, origin: Origin) -> usize {
        let source = origin.source();
        self.share_checks
            .values()
            .filter(|c| c.origin.source() == source)
            .count()
            + self
                .parked_shares
                .iter()
                .filter(|p| p.origin.source() == source)
                .count()
    }

    /// Takes the share's slot and recomputes its tile off the actor.
    fn spawn_share_work_check(
        &mut self,
        slot: ShareSlot,
        signed: Box<SignedShare>,
        relay_id: Digest,
        origin: Origin,
    ) {
        let ticket = self.take_ticket();
        let sem = self.share_verify_sem.clone();
        let tx = self.self_tx.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| CheckFailure::Internal(e.to_string()))?;
                tokio::task::spawn_blocking(move || {
                    // Held until the tile is done, even if this check is cancelled.
                    let _permit = permit;
                    let proof = signed.proof();
                    verify_fresh_work(
                        &proof.context,
                        &ClaimedWork::from_share(proof),
                        proof.share_target,
                        Lottery::Share,
                    )
                    .map_err(|e| CheckFailure::Invalid(format!("share VerifyFW failed: {e}")))?;
                    Ok(signed)
                })
                .await
                .map_err(|e| CheckFailure::Internal(e.to_string()))?
            }
            .await;
            let _ = tx
                .send(ChainCmd::ShareVerified {
                    ticket,
                    slot,
                    relay_id,
                    origin,
                    result,
                })
                .await;
        });
        self.share_checks.insert(
            slot,
            ShareCheckTask {
                ticket,
                origin,
                relay_id,
                task: task.abort_handle(),
            },
        );
    }

    /// Pools a verified share that is still useful and announces it for relay.
    /// Returns true when the pool changed (a template rebuild is due).
    fn admit_share(&mut self, signed: SignedShare, origin: Origin) -> bool {
        let proof = signed.proof();
        let min_height = (self.tree.tip_meta().height + 1)
            .saturating_sub(self.tree.genesis().share_inclusion_window);
        if proof.context.height < min_height
            || self.tree.tip_state().shares.contains_key(&proof.work_id)
        {
            return false;
        }
        let share = Arc::new(proof.clone());
        if !self.share_pool.insert(signed) {
            return false;
        }
        let _ = self
            .event_tx
            .send(ChainEvent::ShareAccepted { share, origin });
        self.publish_status();
        true
    }

    /// Like [`Self::reject`], for shares.
    fn reject_share(&mut self, relay_id: Digest, origin: Origin, reason: String) {
        debug!(?origin, %reason, "Rejected share");
        self.invalid_shares.insert(relay_id);
        let _ = self
            .event_tx
            .send(ChainEvent::ShareRejected { origin, reason });
        if let Some(source) = origin.source() {
            self.cancel_source(source, INVALID_BAN);
        }
    }

    /// Resumes the shares parked for `parent`, now that it is in the tree: the no-work
    /// checks run again (now on the parent's branch), then the work check.
    fn release_parked_shares(&mut self, parent: Digest) {
        let (ready, waiting): (VecDeque<_>, VecDeque<_>) = self
            .parked_shares
            .drain(..)
            .partition(|p| p.parent == parent);
        self.parked_shares = waiting;
        for p in ready {
            if self.share_pool.contains(&p.slot.1)
                || self.share_checks.contains_key(&p.slot)
                || self.is_banned(p.origin)
            {
                continue;
            }
            match self.tree.check_gossip_share(p.signed.proof()) {
                ShareCheck::Verify => {
                    self.spawn_share_work_check(p.slot, p.signed, p.relay_id, p.origin)
                }
                ShareCheck::Invalid(reason) => self.reject_share(p.relay_id, p.origin, reason),
                ShareCheck::UnknownParent(_) | ShareCheck::Ignore(_) => {}
            }
        }
    }

    /// After a tip change: return the shares of abandoned blocks to the pool, then drop
    /// what is outside the inclusion window or registered on the new canonical chain.
    fn refresh_share_pool(&mut self, tip_height: u64, disconnected: &[Arc<Block>]) {
        for share in disconnected.iter().flat_map(|blk| &blk.body.shares) {
            // These passed Stage A inside their block; only the signature is rechecked.
            if let Ok(signed) = SignedShare::verify(share.clone()) {
                self.share_pool.insert(signed);
            }
        }
        let min_height =
            (tip_height + 1).saturating_sub(self.tree.genesis().share_inclusion_window);
        let tip_state = self.tree.tip_state();
        self.share_pool
            .prune(min_height, |work_id| tip_state.shares.contains_key(work_id));
    }

    fn take_ticket(&mut self) -> u64 {
        self.next_ticket += 1;
        self.next_ticket
    }

    /// Starts Stage A for a peer's block that passed Stage B, or queues it while its
    /// source (or everyone) has the most blocks in Stage A. A block that just arrived is
    /// refused when its source has the most blocks queued, or when the queue is full and
    /// no source holds a larger share of it.
    fn queue_stage_a(&mut self, entry: QueuedBlock, fresh: bool) -> ImportAck {
        if self.has_stage_a_slot(entry.origin) {
            self.start_stage_a(entry);
            return ImportAck::Queued;
        }
        if fresh {
            let source = entry.origin.source();
            let queued = self
                .stage_a_queue
                .iter()
                .filter(|q| q.origin.source() == source)
                .count();
            if queued >= STAGE_A_QUEUE_SOURCE_LIMIT
                || (self.stage_a_queue.len() >= STAGE_A_QUEUE_LIMIT
                    && !self.evict_queued_for(queued))
            {
                debug!(id = ?entry.block.header.block_id, origin = ?entry.origin, "Too many blocks awaiting verification");
                return ImportAck::Busy;
            }
        }
        self.stage_a_queue.push_back(entry);
        self.publish_status();
        ImportAck::Queued
    }

    /// Makes room in the full queue for a block whose source has `queued` blocks in it,
    /// by dropping the newest block of the source holding the most, if that is clearly
    /// more. Returns whether room was made.
    fn evict_queued_for(&mut self, queued: usize) -> bool {
        let mut counts: HashMap<Option<Source>, usize> = HashMap::new();
        for q in &self.stage_a_queue {
            *counts.entry(q.origin.source()).or_default() += 1;
        }
        let Some((&heaviest, &most)) = counts.iter().max_by_key(|&(_, n)| *n) else {
            return false;
        };
        if most <= queued + 1 {
            return false;
        }
        let Some(pos) = self
            .stage_a_queue
            .iter()
            .rposition(|q| q.origin.source() == heaviest)
        else {
            return false;
        };
        if let Some(evicted) = self.stage_a_queue.remove(pos) {
            debug!(id = ?evicted.block.header.block_id, origin = ?evicted.origin, "Evicted a queued block for another source's");
            self.drop_parked_shares_unless_pending(evicted.block.header.block_id);
        }
        true
    }

    fn has_stage_a_slot(&self, origin: Origin) -> bool {
        let source = origin.source();
        self.in_flight.len() < STAGE_A_LIMIT
            && self
                .in_flight
                .values()
                .filter(|s| s.origin.source() == source)
                .count()
                < STAGE_A_SOURCE_LIMIT
    }

    fn start_stage_a(&mut self, entry: QueuedBlock) {
        let ticket = self.take_ticket();
        let QueuedBlock {
            block,
            relay_id,
            origin,
            also,
        } = entry;
        let block_id = block.header.block_id;
        let sem = self.verify_sem.clone();
        let tx = self.self_tx.clone();
        let task = tokio::spawn(async move {
            let result = verify_block_parallel(&block, &sem).await;
            let _ = tx
                .send(ChainCmd::WorkVerified {
                    ticket,
                    block,
                    origin,
                    result,
                })
                .await;
        });
        self.in_flight.insert(
            relay_id,
            StageA {
                ticket,
                block_id,
                origin,
                also,
                task: task.abort_handle(),
            },
        );
        self.stage_a_started += 1;
        self.publish_status();
    }

    /// Starts queued blocks, oldest first, while the limits allow.
    fn start_queued_stage_a(&mut self) {
        let mut i = 0;
        while i < self.stage_a_queue.len() && self.in_flight.len() < STAGE_A_LIMIT {
            if !self.has_stage_a_slot(self.stage_a_queue[i].origin) {
                i += 1;
                continue;
            }
            let Some(q) = self.stage_a_queue.remove(i) else {
                break;
            };
            // Another copy may have been applied meanwhile.
            if !self.tree.has_block(&q.block.header.block_id) {
                self.start_stage_a(q);
            }
        }
    }

    fn handle_work_verified(
        &mut self,
        ticket: u64,
        block: Arc<Block>,
        origin: Origin,
        result: Result<(), WorkError>,
    ) {
        let block_id = block.header.block_id;
        let Ok(relay_id) = block_relay_id(&block) else {
            return;
        };
        // The block may be verified for another source by now (its first sender was
        // banned). A result that arrives after its task was cancelled (it was already
        // on its way) is still used, but must not end another copy's Stage A.
        let origin = match self.in_flight.get(&relay_id) {
            Some(s) if s.ticket == ticket => self
                .in_flight
                .remove(&relay_id)
                .map_or(origin, |s| s.origin),
            _ => origin,
        };
        match result {
            Ok(()) => {
                self.finalize_block(block, origin);
                if self.tree.has_block(&block_id)
                    && let Some(other) = self.in_flight.remove(&relay_id)
                {
                    other.task.abort();
                }
            }
            // Not a verdict on the block: nobody is penalized and nothing is cached.
            Err(WorkError::Internal(e)) => {
                warn!(id = ?block_id, error = %e, "Stage A could not run; block dropped");
                self.drop_parked_shares_unless_pending(block_id);
            }
            // A Stage A failure shows these bytes are bad, not that no valid block has
            // this block_id (the claims checked here are not all bound by it), so
            // children parked under the block_id stay parked for the genuine parent.
            Err(err) => {
                self.reject(
                    block_id,
                    relay_id,
                    origin,
                    format!("Stage A VerifyFW failed: {err}"),
                );
                self.drop_parked_shares_unless_pending(block_id);
            }
        }
        self.start_queued_stage_a();
        self.publish_status();
    }

    /// Drops, without penalty, the shares parked for `block_id` unless another copy
    /// of that block is still in (or waiting for) Stage A. A block id is unauthenticated
    /// until Stage A passes, so shares parked behind a block that fails it may name a
    /// fabricated one.
    fn drop_parked_shares_unless_pending(&mut self, block_id: Digest) {
        if !self.is_in_stage_a(&block_id) {
            self.parked_shares.retain(|p| p.parent != block_id);
        }
    }

    /// Drops what `source` sent that still awaits verification: its Stage A tasks and
    /// share work checks are aborted (a tile already computing finishes, and its result
    /// is discarded), and its queued, parked and deferred blocks and parked shares are
    /// dropped. A block another source delivered too stays pending for that source,
    /// keeping its place (a running Stage A goes on). Nothing is condemned: an honest
    /// copy of what was dropped can still be imported.
    fn cancel_source(&mut self, source: Source, ban: Duration) {
        let now = Instant::now();
        self.banned.retain(|_, until| *until > now);
        let until = now + ban;
        let entry = self.banned.entry(source).or_insert(until);
        *entry = (*entry).max(until);
        let theirs = |o: &Origin| o.source() == Some(source);
        // Keeps an entry by moving it to a co-sender; false if none is left.
        let hand_over = |origin: &mut Origin, also: &mut Vec<Origin>| {
            also.retain(|o| !theirs(o));
            if theirs(origin) {
                if also.is_empty() {
                    return false;
                }
                *origin = also.remove(0);
            }
            true
        };
        let mut dropped = Vec::new();
        let mut dropped_ids = Vec::new();
        self.in_flight.retain(|relay_id, s| {
            let keep = hand_over(&mut s.origin, &mut s.also);
            if !keep {
                s.task.abort();
                dropped.push(*relay_id);
                dropped_ids.push(s.block_id);
            }
            keep
        });
        self.stage_a_queue.retain_mut(|q| {
            let keep = hand_over(&mut q.origin, &mut q.also);
            if !keep {
                dropped.push(q.relay_id);
                dropped_ids.push(q.block.header.block_id);
            }
            keep
        });
        self.deferred.retain_mut(|d| {
            let keep = hand_over(&mut d.origin, &mut d.also);
            if !keep {
                dropped.push(d.relay_id);
            }
            keep
        });
        let parked_before = dropped.len();
        for list in self.parked_by_parent.values_mut() {
            list.retain_mut(|c| {
                let keep = hand_over(&mut c.origin, &mut c.also);
                if !keep {
                    dropped.push(c.relay_id);
                }
                keep
            });
        }
        if dropped.len() > parked_before {
            let gone: HashSet<Digest> = dropped[parked_before..].iter().copied().collect();
            self.parked_by_parent.retain(|_, list| !list.is_empty());
            self.parked_order.retain(|r| !gone.contains(&r.relay_id));
        }
        // Parked blocks handed over now count for their new source.
        let senders: HashMap<Digest, Option<Source>> = self
            .parked_by_parent
            .values()
            .flatten()
            .map(|c| (c.relay_id, c.origin.source()))
            .collect();
        for r in &mut self.parked_order {
            if let Some(source) = senders.get(&r.relay_id) {
                r.source = *source;
            }
        }
        let shares_before = self.share_checks.len() + self.parked_shares.len();
        self.share_checks.retain(|_, c| {
            if theirs(&c.origin) {
                c.task.abort();
                return false;
            }
            true
        });
        self.parked_shares.retain(|p| !theirs(&p.origin));
        let dropped_shares = shares_before - self.share_checks.len() - self.parked_shares.len();
        for block_id in dropped_ids {
            self.drop_parked_shares_unless_pending(block_id);
        }
        if dropped.is_empty() && dropped_shares == 0 {
            return;
        }
        info!(
            ?source,
            blocks = dropped.len(),
            shares = dropped_shares,
            "Dropped the pending verification work of a banned source"
        );
        self.start_queued_stage_a();
        self.publish_status();
    }

    fn finalize_block(&mut self, block: Arc<Block>, origin: Origin) {
        let block_id = block.header.block_id;
        match self.tree.apply_verified_block(block.clone()) {
            Ok(outcome) => {
                if origin != Origin::Disk
                    && let Err(e) = self.log_writer.append(&block)
                {
                    warn!(error = %e, "Failed appending block to blocks.log");
                }
                info!(
                    height = outcome.meta.height,
                    id = %outcome.meta.id.short_hex(16),
                    miner = %outcome.meta.miner_key_id.short_hex(16),
                    jobs = block.body.jobs.len(),
                    shares = block.body.shares.len(),
                    weight = outcome.meta.cumulative_weight,
                    tip = outcome.tip_changed,
                    "Accepted block"
                );
                let _ = self.event_tx.send(ChainEvent::BlockAccepted {
                    block: block.clone(),
                    origin,
                });
                if outcome.tip_changed {
                    self.refresh_share_pool(outcome.meta.height, &outcome.disconnected);
                    self.publish_status();
                    self.rebuild_and_publish_template();
                    let _ = self.event_tx.send(ChainEvent::TipChanged {
                        tip: Box::new(outcome.meta),
                        connected: outcome.connected,
                        disconnected: outcome.disconnected,
                    });
                }
                // Re-submit the parked blocks and shares whose parent is this block (§8.3).
                for child in self.take_parked_children(block_id) {
                    let _ =
                        self.admit_block(child.block, child.origin, child.also, Admission::Parked);
                }
                self.release_parked_shares(block_id);
            }
            // After Stage A the header's claims match its block_id, and the body is bound
            // by body_core_root, so this failure condemns every block with this block_id.
            Err(TreeError::Invalid(reason)) => {
                if let Ok(relay_id) = block_relay_id(&block) {
                    self.reject(block_id, relay_id, origin, reason);
                }
                self.drop_parked_children(block_id);
                // Shares mined on an invalid block can never be included.
                self.parked_shares.retain(|p| p.parent != block_id);
            }
            Err(other) => {
                debug!(id = ?block_id, error = %other, "Block not applied");
                self.drop_parked_shares_unless_pending(block_id);
            }
        }
    }

    /// Parks a block until its parent arrives. Orphans cost nothing to make, so when its
    /// source has parked its share the source's own oldest one makes room, and when the
    /// orphan set is full, the oldest one of the source holding the most does.
    fn park(&mut self, parent: Digest, entry: ParkedBlock) {
        let source = entry.origin.source();
        let mut counts: HashMap<Option<Source>, usize> = HashMap::new();
        for r in &self.parked_order {
            *counts.entry(r.source).or_default() += 1;
        }
        let own = counts.get(&source).copied().unwrap_or(0);
        if own >= ORPHAN_SOURCE_LIMIT {
            self.evict_parked(source);
        } else if self.parked_order.len() >= ORPHAN_LIMIT {
            let most = counts.values().copied().max().unwrap_or(0);
            let heaviest = if own == most {
                source
            } else {
                counts
                    .iter()
                    .find(|&(_, n)| *n == most)
                    .map_or(source, |(s, _)| *s)
            };
            self.evict_parked(heaviest);
        }
        self.parked_order.push_back(ParkedRef {
            relay_id: entry.relay_id,
            block_id: entry.block.header.block_id,
            parent,
            source,
        });
        self.parked_by_parent.entry(parent).or_default().push(entry);
    }

    /// Drops the oldest parked block of `source`.
    fn evict_parked(&mut self, source: Option<Source>) {
        let Some(pos) = self.parked_order.iter().position(|r| r.source == source) else {
            return;
        };
        let Some(old) = self.parked_order.remove(pos) else {
            return;
        };
        if let Some(list) = self.parked_by_parent.get_mut(&old.parent) {
            list.retain(|e| e.relay_id != old.relay_id);
            if list.is_empty() {
                self.parked_by_parent.remove(&old.parent);
            }
        }
    }

    fn take_parked_children(&mut self, parent: Digest) -> Vec<ParkedBlock> {
        let children = self.parked_by_parent.remove(&parent).unwrap_or_default();
        if !children.is_empty() {
            self.parked_order.retain(|r| r.parent != parent);
        }
        children
    }

    fn drop_parked_children(&mut self, parent: Digest) {
        for child in self.take_parked_children(parent) {
            self.invalid_lru.insert(child.relay_id);
            self.drop_parked_children(child.block.header.block_id);
        }
    }

    fn retry_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let now = Instant::now();
        let genesis = self.tree.genesis();
        let ready_bucket = (unix_now_secs() / genesis.time_bucket_secs)
            .saturating_add(MAX_FUTURE_SECS / genesis.time_bucket_secs);
        let (ready, waiting): (Vec<_>, Vec<_>) = self
            .deferred
            .drain(..)
            .filter(|e| now <= e.expires_at)
            .partition(|e| e.block.header.context.time_bucket <= ready_bucket);
        self.deferred = waiting;
        for entry in ready {
            let _ = self.admit_block(entry.block, entry.origin, entry.also, Admission::Retry);
        }
    }

    fn publish_status(&self) {
        let counts = PipelineCounts {
            orphans: self.parked_order.len(),
            share_pool: self.share_pool.len(),
            verifying_blocks: self.in_flight.len(),
            queued_blocks: self.stage_a_queue.len(),
            deferred_blocks: self.deferred.len(),
            stage_a_started: self.stage_a_started,
        };
        let snap = build_status_snapshot(&self.tree, &self.miner, self.start_instant, counts);
        let _ = self.status_tx.send(Arc::new(snap));
    }

    fn rebuild_and_publish_template(&mut self) {
        let tip_meta = self.tree.tip_meta().clone();
        let tip_state = self.tree.tip_state();
        let mempool_txs = self.mempool_view.borrow().txs.clone();
        let now_bucket = unix_now_secs() / self.tree.genesis().time_bucket_secs;
        let result = build_template(
            self.template_gen + 1,
            self.tree.genesis(),
            self.tree.genesis_id(),
            &self.tree,
            tip_meta.height,
            &tip_state,
            &mempool_txs,
            self.share_pool.as_slice(),
            &self.miner,
            now_bucket,
        );
        let tpl = match result {
            Ok(tpl) => tpl,
            Err(e) => {
                warn!(error = %e, "Template build failed; keeping the previous template");
                return;
            }
        };
        // Republishing identical work would make the miners repeat the same nonces.
        let unchanged = self
            .template_tx
            .borrow()
            .as_ref()
            .is_some_and(|cur| cur.same_work_as(&tpl));
        if !unchanged {
            self.template_gen += 1;
            let _ = self.template_tx.send(Some(Arc::new(tpl)));
        }
    }
}

async fn verify_block_parallel(block: &Arc<Block>, sem: &Arc<Semaphore>) -> Result<(), WorkError> {
    // Signatures first: about 0.5 ms each, so a forged block is rejected before any of
    // its tiles (tens of ms each) is recomputed. Each permit moves into its blocking
    // task: work that is already computing keeps its CPU slot until it finishes, even
    // if this Stage A is aborted.
    let permit = sem
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| WorkError::Internal(e.to_string()))?;
    let signed = block.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        BlockTree::verify_signatures(&signed)
    })
    .await
    .map_err(|e| WorkError::Internal(e.to_string()))??;

    // Then the header's tile alone: the seal is the work that is costly to fake, so a
    // block's share tiles are only recomputed once its seal is known to be genuine.
    let header_tile = (
        block.header.context.clone(),
        ClaimedWork::from_header(&block.header),
        block.header.seal_target,
        Lottery::Seal,
    );
    let share_tiles = block.body.shares.iter().map(|sh| {
        (
            sh.context.clone(),
            ClaimedWork::from_share(sh),
            sh.share_target,
            Lottery::Share,
        )
    });
    verify_tiles(std::iter::once(header_tile), sem).await?;
    verify_tiles(share_tiles, sem).await
}

/// Recomputes tiles in parallel, each under a permit of `sem`.
async fn verify_tiles(
    jobs: impl Iterator<Item = (FwContext, ClaimedWork, Target, Lottery)>,
    sem: &Arc<Semaphore>,
) -> Result<(), WorkError> {
    let mut tiles = JoinSet::new();
    for (ctx, claimed, target, lottery) in jobs {
        let sem = sem.clone();
        tiles.spawn(async move {
            let permit = sem
                .acquire_owned()
                .await
                .map_err(|e| WorkError::Internal(e.to_string()))?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                verify_fresh_work(&ctx, &claimed, target, lottery)
            })
            .await
            .map_err(|e| WorkError::Internal(e.to_string()))?
        });
    }
    // The first failure returns, and dropping `tiles` aborts the tiles still waiting
    // for a permit; aborting the Stage A task does the same.
    while let Some(joined) = tiles.join_next().await {
        joined.map_err(|e| WorkError::Internal(e.to_string()))??;
    }
    Ok(())
}

fn build_status_snapshot(
    tree: &BlockTree,
    miner: &OwnerKeypair,
    start_instant: Instant,
    counts: PipelineCounts,
) -> StatusSnapshot {
    let state = tree.tip_state();
    let ledger = &state.ledger;
    let genesis = tree.genesis();
    StatusSnapshot {
        chain_name: genesis.name.clone(),
        chain_id: genesis.chain_id,
        genesis_id: tree.genesis_id(),
        blocks_per_epoch: genesis.blocks_per_epoch,
        target_block_secs: genesis.target_block_secs,
        max_jobs_per_block: genesis.max_jobs_per_block,
        miner_key_id: miner.key_id(),
        owner_id: miner.owner_id(),
        start_instant,
        tip: tree.tip_meta().clone(),
        epoch: ledger.epoch(),
        balances: StateCategory::ALL.map(|c| ledger.balance(c).get()),
        total: ledger.total(),
        founder_locked: ledger.founder_vesting().locked_at(state.height).get(),
        segregated_dust: ledger.segregated_dust().get(),
        fee_lots: ledger.fee_lots().len(),
        locked_fees: ledger.locked_fees().len(),
        pending_payout: state.pending_payout,
        open_jobs: state.open_jobs.len(),
        orphan_count: counts.orphans,
        share_pool: counts.share_pool,
        verifying_blocks: counts.verifying_blocks,
        queued_blocks: counts.queued_blocks,
        deferred_blocks: counts.deferred_blocks,
        stage_a_started: counts.stage_a_started,
    }
}
