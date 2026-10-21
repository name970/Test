//! ChainState actor (`node::chain_actor`, §8.2, §8.3).
//!
//! The only writer of chain state. It owns the `BlockTree`, runs the import pipeline
//! (§6.8), persists accepted blocks, publishes templates and status snapshots, and
//! never awaits another actor (§8.2).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chain::{
    BlockMeta, BlockSummary, BlockTree, ClaimedWork, Lottery, OwnerKeypair, ShareCheck,
    SignedShare, Template, TreeError, WorkError, block_relay_id, build_template, owner_key_id,
    share_relay_id, verify_fresh_work,
};
use ledger::StateCategory;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use wire::{
    Block, Digest, GenesisConfig, MAX_FUTURE_SECS, MAX_SYNC_BLOCKS, MAX_SYNC_BYTES, ORPHAN_LIMIT,
    ShareProof, codec,
};

use crate::mempool::MempoolView;
use crate::share_pool::{SHARE_POOL_CAPACITY, SharePool};
use crate::store::BlockLogWriter;

/// Maximum gossiped shares awaiting their work check (one tile recomputation each),
/// parked or running. Parked shares count too, so releasing them never exceeds it.
const SHARE_CHECK_LIMIT: usize = 64;
/// Per peer: shares parked or awaiting their work check. Honest gossip stays far below.
const SHARE_PEER_LIMIT: usize = 8;

/// A pending share's slot: (owner key id, work_id). Keyed by owner too because only
/// the work check ties a work_id to its owner: a validly signed claim to another
/// owner's work_id must not hold that owner's slot.
type ShareSlot = (Digest, Digest);

/// Identifier of a connected peer.
pub type PeerId = u64;

/// Origin of a block or transaction (§8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Mined by this node.
    Local,
    /// Submitted through the RPC.
    Rpc,
    /// Received from a peer.
    Peer(PeerId),
    /// Replayed from `blocks.log`.
    Disk,
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
}

/// Immediate acknowledgment of `ChainCmd::ImportShare`, sent before the work check runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareAck {
    /// Accepted for verification.
    Queued,
    /// Waiting for its parent block, which is being verified. The share is dropped
    /// without penalty if that block turns out invalid, so it may be relayed again.
    Parked,
    /// Already pooled, registered, in flight, parked or rejected before.
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
        /// The share's (owner key id, work_id).
        slot: (Digest, Digest),
        /// The share's relay id.
        relay_id: Digest,
        /// Where it came from.
        origin: Origin,
        /// The verified share, or why it was rejected.
        result: Result<Box<SignedShare>, String>,
    },
    /// Internal: Stage A finished for a block.
    WorkVerified {
        /// The verified block.
        block: Arc<Block>,
        /// Where it came from.
        origin: Origin,
        /// Verification outcome.
        result: Result<(), WorkError>,
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
}

struct ParkedBlock {
    block: Arc<Block>,
    relay_id: Digest,
    origin: Origin,
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
    share_checks: HashMap<ShareSlot, Origin>,
    parked_shares: VecDeque<ParkedShare>, // oldest first
    invalid_shares: LruIdCache,           // relay ids
    // Blocks awaiting validation are deduplicated by relay id (`chain::block_relay_id`),
    // not by block_id: a copy with a forged signature or body shares the honest block_id
    // and must not shadow, or get cached as invalid in place of, the genuine block.
    in_flight: HashMap<Digest, Digest>, // relay_id -> block_id
    parked_by_parent: HashMap<Digest, Vec<ParkedBlock>>,
    parked_order: VecDeque<(Digest, Digest, Digest)>, // (relay_id, block_id, parent_id), oldest first
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
        let status = build_status_snapshot(&tree, &miner, start_instant, 0, 0);
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
                slot,
                relay_id,
                origin,
                result,
            } => {
                self.share_checks.remove(&slot);
                match result {
                    Ok(signed) => return self.admit_share(*signed, origin),
                    Err(reason) => self.reject_share(relay_id, origin, reason),
                }
            }
            ChainCmd::WorkVerified {
                block,
                origin,
                result,
            } => self.handle_work_verified(block, origin, result),
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

    /// Whether a block with this block_id awaits validation (a parent check).
    fn is_pending_block(&self, block_id: &Digest) -> bool {
        self.in_flight.values().any(|b| b == block_id)
            || self.parked_order.iter().any(|(_, b, _)| b == block_id)
            || self
                .deferred
                .iter()
                .any(|d| d.block.header.block_id == *block_id)
    }

    /// Whether these exact block bytes await validation.
    fn is_pending_relay(&self, relay_id: &Digest) -> bool {
        self.in_flight.contains_key(relay_id)
            || self.parked_order.iter().any(|(r, _, _)| r == relay_id)
            || self.deferred.iter().any(|d| d.relay_id == *relay_id)
    }

    fn import_block(&mut self, block: Arc<Block>, origin: Origin) -> ImportAck {
        let block_id = block.header.block_id;
        let relay_id = match block_relay_id(&block) {
            Ok(id) => id,
            Err(e) => return ImportAck::Invalid(format!("unencodable header: {e}")),
        };
        if self.tree.has_block(&block_id)
            || self.is_pending_relay(&relay_id)
            || self.invalid_lru.contains(&relay_id)
        {
            return ImportAck::Duplicate;
        }

        match self.tree.pre_validate(&block, Some(unix_now_secs())) {
            Ok(()) => {
                if matches!(origin, Origin::Local | Origin::Disk) {
                    self.finalize_block(block, origin);
                } else {
                    self.in_flight.insert(relay_id, block_id);
                    self.spawn_stage_a(block, origin);
                }
                ImportAck::Queued
            }
            Err(TreeError::Duplicate) => ImportAck::Duplicate,
            Err(TreeError::Orphan { parent }) => {
                let parent_pending = self.is_pending_block(&parent);
                self.park(parent, block, relay_id, origin);
                if parent_pending {
                    ImportAck::Queued
                } else {
                    ImportAck::Orphan
                }
            }
            Err(TreeError::Deferred { .. }) => {
                self.deferred.push(DeferredEntry {
                    block,
                    relay_id,
                    origin,
                    expires_at: Instant::now() + Duration::from_secs(600),
                });
                ImportAck::Deferred
            }
            Err(TreeError::TooDeep) => ImportAck::TooDeep,
            Err(TreeError::Invalid(reason)) => {
                self.reject(block_id, relay_id, origin, reason.clone());
                ImportAck::Invalid(reason)
            }
        }
    }

    /// Caches the rejected block's relay id (never its block_id, which an invalid copy
    /// of an honest block can share) and reports the rejection.
    fn reject(&mut self, block_id: Digest, relay_id: Digest, origin: Origin, reason: String) {
        debug!(id = ?block_id, ?origin, %reason, "Rejected block");
        self.invalid_lru.insert(relay_id);
        let _ = self.event_tx.send(ChainEvent::BlockRejected {
            block_id,
            origin,
            reason,
        });
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
            ShareCheck::UnknownParent(parent) if self.in_flight.values().any(|b| *b == parent) => {
                Some(parent)
            }
            ShareCheck::UnknownParent(_) => return ShareAck::Ignored("share parent is unknown"),
            ShareCheck::Ignore(why) => return ShareAck::Ignored(why),
            ShareCheck::Invalid(reason) => {
                self.reject_share(relay_id, origin, reason.clone());
                return ShareAck::Invalid(reason);
            }
        };
        if self.share_checks.len() + self.parked_shares.len() >= SHARE_CHECK_LIMIT
            || self.peer_share_load(origin) >= SHARE_PEER_LIMIT
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

    /// Shares from `origin` that are parked or awaiting their work check.
    fn peer_share_load(&self, origin: Origin) -> usize {
        self.share_checks.values().filter(|o| **o == origin).count()
            + self
                .parked_shares
                .iter()
                .filter(|p| p.origin == origin)
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
        self.share_checks.insert(slot, origin);
        let sem = self.share_verify_sem.clone();
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let _permit = sem.acquire_owned().await.map_err(|e| e.to_string())?;
                tokio::task::spawn_blocking(move || {
                    let proof = signed.proof();
                    verify_fresh_work(
                        &proof.context,
                        &ClaimedWork::from_share(proof),
                        proof.share_target,
                        Lottery::Share,
                    )
                    .map_err(|e| format!("share VerifyFW failed: {e}"))?;
                    Ok(signed)
                })
                .await
                .map_err(|e| e.to_string())?
            }
            .await;
            let _ = tx
                .send(ChainCmd::ShareVerified {
                    slot,
                    relay_id,
                    origin,
                    result,
                })
                .await;
        });
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

    fn reject_share(&mut self, relay_id: Digest, origin: Origin, reason: String) {
        debug!(?origin, %reason, "Rejected share");
        self.invalid_shares.insert(relay_id);
        let _ = self
            .event_tx
            .send(ChainEvent::ShareRejected { origin, reason });
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
            if self.share_pool.contains(&p.slot.1) || self.share_checks.contains_key(&p.slot) {
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

    fn spawn_stage_a(&self, block: Arc<Block>, origin: Origin) {
        let sem = self.verify_sem.clone();
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            let result = verify_block_parallel(&block, &sem).await;
            let _ = tx
                .send(ChainCmd::WorkVerified {
                    block,
                    origin,
                    result,
                })
                .await;
        });
    }

    fn handle_work_verified(
        &mut self,
        block: Arc<Block>,
        origin: Origin,
        result: Result<(), WorkError>,
    ) {
        let block_id = block.header.block_id;
        let Ok(relay_id) = block_relay_id(&block) else {
            return;
        };
        self.in_flight.remove(&relay_id);
        match result {
            Ok(()) => self.finalize_block(block, origin),
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
    }

    /// Drops, without penalty, the shares parked for `block_id` unless another copy
    /// of that block is still in Stage A. A block id is unauthenticated until Stage A
    /// passes, so shares parked behind a block that fails it may name a fabricated one.
    fn drop_parked_shares_unless_pending(&mut self, block_id: Digest) {
        if !self.in_flight.values().any(|b| *b == block_id) {
            self.parked_shares.retain(|p| p.parent != block_id);
        }
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
                    let _ = self.import_block(child.block, child.origin);
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

    fn park(&mut self, parent: Digest, block: Arc<Block>, relay_id: Digest, origin: Origin) {
        while self.parked_order.len() >= ORPHAN_LIMIT {
            let Some((old_relay, _, old_parent)) = self.parked_order.pop_front() else {
                break;
            };
            if let Some(list) = self.parked_by_parent.get_mut(&old_parent) {
                list.retain(|e| e.relay_id != old_relay);
                if list.is_empty() {
                    self.parked_by_parent.remove(&old_parent);
                }
            }
        }
        self.parked_order
            .push_back((relay_id, block.header.block_id, parent));
        self.parked_by_parent
            .entry(parent)
            .or_default()
            .push(ParkedBlock {
                block,
                relay_id,
                origin,
            });
    }

    fn take_parked_children(&mut self, parent: Digest) -> Vec<ParkedBlock> {
        let children = self.parked_by_parent.remove(&parent).unwrap_or_default();
        if !children.is_empty() {
            self.parked_order.retain(|(_, _, p)| *p != parent);
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
            let _ = self.import_block(entry.block, entry.origin);
        }
    }

    fn publish_status(&self) {
        let snap = build_status_snapshot(
            &self.tree,
            &self.miner,
            self.start_instant,
            self.parked_order.len(),
            self.share_pool.len(),
        );
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
    // its tiles (tens of ms each) is recomputed.
    {
        let _permit = sem
            .acquire()
            .await
            .map_err(|e| WorkError::Internal(e.to_string()))?;
        let signed = block.clone();
        tokio::task::spawn_blocking(move || BlockTree::verify_signatures(&signed))
            .await
            .map_err(|e| WorkError::Internal(e.to_string()))??;
    }

    let mut jobs = vec![(
        block.header.context.clone(),
        ClaimedWork::from_header(&block.header),
        block.header.seal_target,
        Lottery::Seal,
    )];
    jobs.extend(block.body.shares.iter().map(|sh| {
        (
            sh.context.clone(),
            ClaimedWork::from_share(sh),
            sh.share_target,
            Lottery::Share,
        )
    }));

    let handles: Vec<_> = jobs
        .into_iter()
        .map(|(ctx, claimed, target, lottery)| {
            let sem = sem.clone();
            tokio::spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| WorkError::Internal(e.to_string()))?;
                tokio::task::spawn_blocking(move || {
                    verify_fresh_work(&ctx, &claimed, target, lottery)
                })
                .await
                .map_err(|e| WorkError::Internal(e.to_string()))?
            })
        })
        .collect();

    for handle in handles {
        handle
            .await
            .map_err(|e| WorkError::Internal(e.to_string()))??;
    }
    Ok(())
}

fn build_status_snapshot(
    tree: &BlockTree,
    miner: &OwnerKeypair,
    start_instant: Instant,
    orphan_count: usize,
    share_pool: usize,
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
        orphan_count,
        share_pool,
    }
}
