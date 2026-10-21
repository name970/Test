//! BlockTree, Stage B validation, fork choice, and block summaries (`chain::tree`, §6, §8.3).

use core::fmt;
use std::collections::HashMap;
use std::sync::Arc;

use ledger::StateCategory;
use wire::{
    Block, CORE_PROTOCOL_VERSION, CRYPTO_SUITE_ID, Digest, FeeTransition, GENERATOR_VERSION,
    GenesisConfig, MAX_FUTURE_SECS, MAX_REORG_DEPTH, ShareProof, Target, codec,
};

use crate::core_adapter::{
    ClaimedWork, Lottery, WorkError, compute_body_root, compute_difficulty_context,
    compute_job_ids, compute_miner_policy_digest, compute_payout_policy_digest,
    compute_program_and_air_ids, owner_key_id, top128, validate_genesis, verify_fresh_work,
    verify_header_miner, verify_job_client, verify_share_owner,
};
use crate::difficulty::{ChainView, expected_targets, seal_weight, share_target_for, share_weight};
use crate::stf::{NodeState, ShareRecord, VerifiedShare, apply_body};

/// Metadata stored per validated block in `BlockTree` (§8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    /// `block_id` (or `genesis_id` for the virtual height 0).
    pub id: Digest,
    /// Parent `block_id` (`Digest::ZERO` for genesis).
    pub parent: Digest,
    /// Height.
    pub height: u64,
    /// Context field 16.
    pub time_bucket: u64,
    /// `Θ_B` of this height.
    pub seal_target: Target,
    /// `Θ_S` of this height.
    pub share_target: Target,
    /// `work_of(seal_target)`.
    pub seal_weight: u128,
    /// Sum of seal weights from genesis.
    pub cumulative_weight: u128,
    /// Beneficiary's key id (`Digest::ZERO` for genesis).
    pub miner_key_id: Digest,
}

/// Summary of an admitted job inside a block for `/api/blocks` (§9.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSummary {
    /// Job identifier.
    pub job_id: u64,
    /// Transaction identifier.
    pub tx_id: Digest,
    /// Lease owner's key id.
    pub executor_owner: Digest,
    /// Escrow deposit.
    pub deposit: u64,
    /// Queue fee.
    pub queue_fee: u64,
    /// Base Fee.
    pub base_fee: u64,
}

/// Summary of a settlement receipt inside a block for `/api/blocks` (§9.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptSummary {
    /// Settled job.
    pub job_id: u64,
    /// Exact dot product.
    pub result: i128,
    /// Lease owner's key id.
    pub executor_owner: Digest,
    /// Height of admission.
    pub admitted_at: u64,
}

/// Summary of an included share inside a block for `/api/blocks` (§9.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareSummary {
    /// ShareID.
    pub work_id: Digest,
    /// Beneficiary's key id.
    pub owner: Digest,
    /// Height the share was mined at.
    pub height: u64,
    /// `w_s`.
    pub weight: u64,
}

/// Summary of a block and its post-state balances for `/api/blocks` (§9.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSummary {
    /// Height.
    pub height: u64,
    /// `block_id`.
    pub block_id: Digest,
    /// Parent `block_id`.
    pub parent_id: Digest,
    /// `stable_anchor_id`.
    pub anchor_id: Digest,
    /// Beneficiary's key id.
    pub miner_key_id: Digest,
    /// Context field 16.
    pub time_bucket: u64,
    /// Winning nonce.
    pub attempt_nonce: u64,
    /// Ledger epoch after the block.
    pub epoch: u64,
    /// `θ_B`.
    pub seal_ticket: Digest,
    /// Seal weight.
    pub seal_weight: u128,
    /// Cumulative chain weight.
    pub cumulative_weight: u128,
    /// Admitted jobs.
    pub jobs: Vec<JobSummary>,
    /// Settled jobs.
    pub receipts: Vec<ReceiptSummary>,
    /// Included share proofs.
    pub shares: Vec<ShareSummary>,
    /// Whether the header's own work registered as a share.
    pub header_share: bool,
    /// Ledger transitions in execution order.
    pub fee_transitions: Vec<FeeTransition>,
    /// The 8 category balances after the block, in `StateCategory::ALL` order.
    pub balances_after: [u64; StateCategory::COUNT],
    /// Sum of `balances_after` (always `M`).
    pub total_after: u128,
}

/// View over one branch of a `BlockTree`, from genesis up to a given block.
///
/// Heights at or below `fork_height` are on the canonical chain; the side branch above
/// it is stored explicitly, so building the view costs only the side-branch length.
pub struct AncestorChainView<'a> {
    tree: &'a BlockTree,
    fork_height: u64,
    branch: Vec<Digest>, // heights fork_height+1 ..= top, ascending
}

impl AncestorChainView<'_> {
    /// Highest height covered by the view.
    pub fn top_height(&self) -> u64 {
        self.fork_height + self.branch.len() as u64
    }

    fn meta_at(&self, h: u64) -> Option<&BlockMeta> {
        self.tree.metas.get(&self.block_id_at(h))
    }
}

impl ChainView for AncestorChainView<'_> {
    fn block_id_at(&self, h: u64) -> Digest {
        if h <= self.fork_height {
            self.tree.canonical_id_at(h).unwrap_or(Digest::ZERO)
        } else {
            self.branch
                .get((h - self.fork_height - 1) as usize)
                .copied()
                .unwrap_or(Digest::ZERO)
        }
    }

    fn time_bucket_at(&self, h: u64) -> u64 {
        self.meta_at(h)
            .map_or(self.tree.genesis.genesis_time_bucket, |m| m.time_bucket)
    }

    fn targets_at(&self, h: u64) -> (Target, Target) {
        self.meta_at(h).map_or_else(
            || self.tree.genesis_targets(),
            |m| (m.seal_target, m.share_target),
        )
    }
}

impl ChainView for BlockTree {
    fn block_id_at(&self, h: u64) -> Digest {
        self.canonical_id_at(h).unwrap_or(Digest::ZERO)
    }

    fn time_bucket_at(&self, h: u64) -> u64 {
        self.canonical_meta_at(h)
            .map_or(self.genesis.genesis_time_bucket, |m| m.time_bucket)
    }

    fn targets_at(&self, h: u64) -> (Target, Target) {
        self.canonical_meta_at(h).map_or_else(
            || self.genesis_targets(),
            |m| (m.seal_target, m.share_target),
        )
    }
}

/// Validation or tree error (§6.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The block is already known.
    Duplicate,
    /// The parent is not in the tree.
    Orphan {
        /// The missing parent.
        parent: Digest,
    },
    /// The fork point is deeper than `MAX_REORG_DEPTH` or its snapshot was pruned.
    TooDeep,
    /// The block's `time_bucket` is too far in the future; retry later.
    Deferred {
        /// The block's time bucket.
        time_bucket: u64,
    },
    /// The block violates a consensus rule.
    Invalid(String),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::Duplicate => write!(f, "duplicate block"),
            TreeError::Orphan { parent } => write!(f, "unknown parent {parent:?}"),
            TreeError::TooDeep => write!(f, "parent fork exceeds MAX_REORG_DEPTH or is pruned"),
            TreeError::Deferred { time_bucket } => {
                write!(f, "block time_bucket {time_bucket} is in the future")
            }
            TreeError::Invalid(reason) => write!(f, "invalid block: {reason}"),
        }
    }
}

impl std::error::Error for TreeError {}

fn invalid(reason: impl Into<String>) -> TreeError {
    TreeError::Invalid(reason.into())
}

/// Outcome of the checks a gossiped share passes before its work is verified
/// ([`BlockTree::check_gossip_share`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareCheck {
    /// Every check that needs no work passed: verify the signature and the work next.
    Verify,
    /// The share's parent block is not in the tree; it may still be on its way.
    UnknownParent(Digest),
    /// Possibly honest but of no use here (stale, already registered, ...): drop it
    /// without penalizing the sender.
    Ignore(&'static str),
    /// No honest node relays this share: penalize the sender.
    Invalid(String),
}

/// Outcome of inserting a verified block into `BlockTree`.
#[derive(Clone, Debug)]
pub struct InsertOutcome {
    /// Metadata of the inserted block.
    pub meta: BlockMeta,
    /// Whether the block became the new tip.
    pub tip_changed: bool,
    /// Blocks that joined the canonical chain, ascending.
    pub connected: Vec<Arc<Block>>,
    /// Blocks that left the canonical chain, old tip first.
    pub disconnected: Vec<Arc<Block>>,
}

/// Multi-branch block tree and canonical chain tracker (§6, §8.3).
pub struct BlockTree {
    genesis: GenesisConfig,
    genesis_id: Digest,
    metas: HashMap<Digest, BlockMeta>,
    blocks: HashMap<Digest, Arc<Block>>,
    states: HashMap<Digest, Arc<NodeState>>,
    summaries: HashMap<Digest, BlockSummary>,
    canonical: Vec<Digest>, // index == height
    tip: Digest,
}

impl BlockTree {
    /// Creates a new `BlockTree` initialized at the virtual height 0 (§3.4).
    pub fn new(genesis: GenesisConfig) -> Result<Self, String> {
        let (_params, ledger, genesis_id) = validate_genesis(&genesis)?;
        let seal = genesis.initial_seal_target;
        let genesis_meta = BlockMeta {
            id: genesis_id,
            parent: Digest::ZERO,
            height: 0,
            time_bucket: genesis.genesis_time_bucket,
            seal_target: seal,
            share_target: share_target_for(seal, genesis.share_ratio),
            seal_weight: 0,
            cumulative_weight: 0,
            miner_key_id: Digest::ZERO,
        };
        Ok(Self {
            genesis,
            genesis_id,
            metas: HashMap::from([(genesis_id, genesis_meta)]),
            blocks: HashMap::new(),
            states: HashMap::from([(genesis_id, Arc::new(NodeState::genesis(ledger)))]),
            summaries: HashMap::new(),
            canonical: vec![genesis_id],
            tip: genesis_id,
        })
    }

    /// The genesis configuration.
    pub fn genesis(&self) -> &GenesisConfig {
        &self.genesis
    }

    /// `genesis_id`, the `block_id` of height 0.
    pub fn genesis_id(&self) -> Digest {
        self.genesis_id
    }

    fn genesis_targets(&self) -> (Target, Target) {
        let seal = self.genesis.initial_seal_target;
        (seal, share_target_for(seal, self.genesis.share_ratio))
    }

    /// Current tip `block_id`.
    pub fn tip_id(&self) -> Digest {
        self.tip
    }

    /// Current tip metadata.
    pub fn tip_meta(&self) -> &BlockMeta {
        self.metas
            .get(&self.tip)
            .expect("invariant: the tip is always in the tree")
    }

    /// Current tip post-state.
    pub fn tip_state(&self) -> Arc<NodeState> {
        self.states
            .get(&self.tip)
            .cloned()
            .expect("invariant: the tip state is never pruned")
    }

    /// Whether the block is in the tree (any branch).
    pub fn has_block(&self, id: &Digest) -> bool {
        self.metas.contains_key(id)
    }

    /// A block by id (any branch).
    pub fn get_block(&self, id: &Digest) -> Option<Arc<Block>> {
        self.blocks.get(id).cloned()
    }

    /// Metadata by id (any branch).
    pub fn get_meta(&self, id: &Digest) -> Option<&BlockMeta> {
        self.metas.get(id)
    }

    /// Post-state by id, if not pruned.
    pub fn get_state(&self, id: &Digest) -> Option<Arc<NodeState>> {
        self.states.get(id).cloned()
    }

    /// Canonical `block_id` at `height`.
    pub fn canonical_id_at(&self, height: u64) -> Option<Digest> {
        self.canonical.get(usize::try_from(height).ok()?).copied()
    }

    fn canonical_meta_at(&self, height: u64) -> Option<&BlockMeta> {
        self.metas.get(&self.canonical_id_at(height)?)
    }

    /// Canonical block at `height` (`None` for height 0).
    pub fn canonical_block_at(&self, height: u64) -> Option<Arc<Block>> {
        self.blocks.get(&self.canonical_id_at(height)?).cloned()
    }

    /// Summaries of canonical blocks with height `< before` (default: all), newest first.
    pub fn canonical_summaries(&self, before: Option<u64>, limit: usize) -> Vec<BlockSummary> {
        let tip_h = self.tip_meta().height;
        let max_h = before.map_or(tip_h, |b| b.saturating_sub(1).min(tip_h));
        (1..=max_h)
            .rev()
            .filter_map(|h| self.summaries.get(&self.canonical_id_at(h)?).cloned())
            .take(limit)
            .collect()
    }

    /// Whether a not-yet-expired transaction is included on the canonical chain.
    ///
    /// The tip state remembers every used `tx_id` until its expiry height, after which
    /// the transaction could not be included again anyway.
    pub fn is_tx_on_canonical(&self, tx_id: &Digest) -> bool {
        self.tip_state().used_tx.contains_key(tx_id)
    }

    fn is_canonical(&self, meta: &BlockMeta) -> bool {
        self.canonical_id_at(meta.height) == Some(meta.id)
    }

    /// Builds an `AncestorChainView` of the branch ending at `top_id`.
    pub fn ancestor_view(&self, top_id: Digest) -> Option<AncestorChainView<'_>> {
        let mut branch = Vec::new();
        let mut meta = self.metas.get(&top_id)?;
        while !self.is_canonical(meta) {
            branch.push(meta.id);
            meta = self.metas.get(&meta.parent)?;
        }
        branch.reverse();
        Some(AncestorChainView {
            tree: self,
            fork_height: meta.height,
            branch,
        })
    }

    /// Runs §6.8 steps 1–6: size and ordering, duplicate, parent, depth, Stage B and time.
    ///
    /// `now_unix_secs = None` skips the local future-time check (disk replay, tests).
    pub fn pre_validate(&self, block: &Block, now_unix_secs: Option<u64>) -> Result<(), TreeError> {
        // Step 1: size and list caps, sorting and uniqueness
        let encoded = codec::encode(block).map_err(|e| invalid(e.to_string()))?;
        if encoded.len() as u64 > self.genesis.max_block_bytes {
            return Err(invalid(format!(
                "block size {} exceeds max_block_bytes {}",
                encoded.len(),
                self.genesis.max_block_bytes
            )));
        }
        let body = &block.body;
        if body.jobs.len() as u64 > self.genesis.max_jobs_per_block {
            return Err(invalid("too many jobs in block"));
        }
        if body.shares.len() as u64 > self.genesis.max_shares_per_block {
            return Err(invalid("too many shares in block"));
        }
        let mut prev_tx: Option<Digest> = None;
        for tx in &body.jobs {
            let (tx_id, _) =
                compute_job_ids(tx).map_err(|e| invalid(format!("job id failed: {e}")))?;
            if prev_tx.is_some_and(|p| p >= tx_id) {
                return Err(invalid("jobs must be strictly sorted by tx_id"));
            }
            prev_tx = Some(tx_id);
        }
        if body.shares.windows(2).any(|w| w[0].work_id >= w[1].work_id) {
            return Err(invalid("shares must be strictly sorted by work_id"));
        }

        // Step 2: duplicate
        if self.metas.contains_key(&block.header.block_id) {
            return Err(TreeError::Duplicate);
        }

        // Step 3: parent in tree
        let ctx = &block.header.context;
        let parent_id = ctx.parent_canonical_id;
        let Some(parent_meta) = self.metas.get(&parent_id) else {
            return Err(TreeError::Orphan { parent: parent_id });
        };

        // Step 4: parent snapshot missing or fork deeper than MAX_REORG_DEPTH
        let Some(parent_state) = self.states.get(&parent_id) else {
            return Err(TreeError::TooDeep);
        };
        if self.tip_meta().height.saturating_sub(parent_meta.height) > MAX_REORG_DEPTH {
            return Err(TreeError::TooDeep);
        }

        // Step 5: Stage B for the header (§6.3) and for each share (§6.3, §6.7)
        let h = parent_meta.height + 1;
        let (program_id, air_id) = compute_program_and_air_ids();
        if ctx.height != h {
            return Err(invalid(format!("expected height {h}, got {}", ctx.height)));
        }
        self.check_context_constants(ctx, program_id, air_id)?;
        let miner_key_id = owner_key_id(&block.header.miner_key);
        if ctx.miner_policy_digest != compute_miner_policy_digest(&miner_key_id)
            || ctx.payout_policy_digest != compute_payout_policy_digest(&miner_key_id)
        {
            return Err(invalid("policy digest does not match miner_key"));
        }
        if ctx.body_core_root != compute_body_root(body).map_err(invalid)? {
            return Err(invalid("body_core_root mismatch"));
        }
        if ctx.time_bucket < parent_meta.time_bucket {
            return Err(invalid("time_bucket regressed below parent"));
        }

        let view = self
            .ancestor_view(parent_id)
            .ok_or_else(|| invalid("missing ancestor chain"))?;
        if ctx.stable_anchor_id != view.block_id_at(h.saturating_sub(self.genesis.anchor_depth)) {
            return Err(invalid("stable_anchor_id mismatch"));
        }
        let (exp_seal, exp_share) = expected_targets(h, &view, &self.genesis);
        if block.header.seal_target != exp_seal {
            return Err(invalid("seal_target mismatch"));
        }
        if block.header.share_target != exp_share {
            return Err(invalid("share_target mismatch"));
        }
        if ctx.parent_derived_difficulty_context != compute_difficulty_context(exp_seal, exp_share)
        {
            return Err(invalid("parent_derived_difficulty_context mismatch"));
        }
        for sh in &body.shares {
            self.validate_share_stage_b(sh, h, &view, parent_state, program_id, air_id)?;
        }

        // Step 6: future time_bucket (§6.6), a local rule
        if let Some(now_secs) = now_unix_secs {
            let now_bucket = now_secs / self.genesis.time_bucket_secs;
            let max_lead = MAX_FUTURE_SECS / self.genesis.time_bucket_secs;
            if ctx.time_bucket > now_bucket.saturating_add(max_lead) {
                return Err(TreeError::Deferred {
                    time_bucket: ctx.time_bucket,
                });
            }
        }
        Ok(())
    }

    fn check_context_constants(
        &self,
        ctx: &wire::FwContext,
        program_id: Digest,
        air_id: Digest,
    ) -> Result<(), TreeError> {
        let checks = [
            (ctx.chain_id == self.genesis.chain_id, "chain_id"),
            (ctx.genesis_id == self.genesis_id, "genesis_id"),
            (
                ctx.protocol_version == CORE_PROTOCOL_VERSION,
                "protocol_version",
            ),
            (ctx.crypto_suite_id == CRYPTO_SUITE_ID, "crypto_suite_id"),
            (ctx.program_id == program_id, "program_id"),
            (ctx.air_id == air_id, "air_id"),
            (
                ctx.generator_version == GENERATOR_VERSION,
                "generator_version",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, field)) => Err(invalid(format!("{field} mismatch"))),
            None => Ok(()),
        }
    }

    fn validate_share_stage_b(
        &self,
        sh: &ShareProof,
        h: u64,
        view: &AncestorChainView<'_>,
        parent_state: &NodeState,
        program_id: Digest,
        air_id: Digest,
    ) -> Result<(), TreeError> {
        let s = sh.context.height;
        if s < 1 || s >= h || s < h.saturating_sub(self.genesis.share_inclusion_window) {
            return Err(invalid(format!(
                "share height {s} outside inclusion window at block height {h}"
            )));
        }
        if parent_state.shares.contains_key(&sh.work_id) {
            return Err(invalid("share work_id already registered"));
        }
        let c = &sh.context;
        self.check_context_constants(c, program_id, air_id)
            .map_err(|e| invalid(format!("share: {e}")))?;
        if c.parent_canonical_id != view.block_id_at(s - 1) {
            return Err(invalid("share parent_canonical_id mismatch"));
        }
        if c.stable_anchor_id != view.block_id_at(s.saturating_sub(self.genesis.anchor_depth)) {
            return Err(invalid("share stable_anchor_id mismatch"));
        }
        let owner_key_id = owner_key_id(&sh.owner_key);
        if c.miner_policy_digest != compute_miner_policy_digest(&owner_key_id)
            || c.payout_policy_digest != compute_payout_policy_digest(&owner_key_id)
        {
            return Err(invalid("share policy digest mismatch"));
        }
        let (exp_seal, exp_share) = view.targets_at(s);
        if sh.share_target != exp_share {
            return Err(invalid("share_target mismatch"));
        }
        if c.parent_derived_difficulty_context != compute_difficulty_context(exp_seal, exp_share) {
            return Err(invalid("share difficulty context mismatch"));
        }
        Ok(())
    }

    /// Checks a gossiped share against everything that needs no work recomputation,
    /// before it may enter the share pool.
    ///
    /// Context constants and the owner binding must hold on any chain. If the parent is
    /// known, the height, anchor and targets are checked on the parent's own branch, so
    /// an honest share from a side branch still passes. Shares outside the inclusion
    /// window of the canonical tip, or already registered on it, are ignored. Whether a
    /// pooled share fits a given template is decided again when the template is built.
    pub fn check_gossip_share(&self, sh: &ShareProof) -> ShareCheck {
        let (program_id, air_id) = compute_program_and_air_ids();
        let c = &sh.context;
        if let Err(e) = self.check_context_constants(c, program_id, air_id) {
            return ShareCheck::Invalid(format!("share: {e}"));
        }
        let owner_key_id = owner_key_id(&sh.owner_key);
        if c.miner_policy_digest != compute_miner_policy_digest(&owner_key_id)
            || c.payout_policy_digest != compute_payout_policy_digest(&owner_key_id)
        {
            return ShareCheck::Invalid("share policy digest mismatch".into());
        }
        let s = c.height;
        if s < 1 {
            return ShareCheck::Invalid("share height 0".into());
        }
        let next = self.tip_meta().height + 1;
        if s < next.saturating_sub(self.genesis.share_inclusion_window) {
            return ShareCheck::Ignore("share is outside the inclusion window");
        }
        if self.tip_state().shares.contains_key(&sh.work_id) {
            return ShareCheck::Ignore("share already registered");
        }
        let Some(parent) = self.metas.get(&c.parent_canonical_id) else {
            return ShareCheck::UnknownParent(c.parent_canonical_id);
        };
        if parent.height + 1 != s {
            return ShareCheck::Invalid(format!(
                "share height {s} does not follow its parent at {}",
                parent.height
            ));
        }
        let Some(view) = self.ancestor_view(parent.id) else {
            return ShareCheck::Ignore("share parent branch unavailable");
        };
        if c.stable_anchor_id != view.block_id_at(s.saturating_sub(self.genesis.anchor_depth)) {
            return ShareCheck::Invalid("share stable_anchor_id mismatch".into());
        }
        let (exp_seal, exp_share) = expected_targets(s, &view, &self.genesis);
        if sh.share_target != exp_share {
            return ShareCheck::Invalid("share_target mismatch".into());
        }
        if c.parent_derived_difficulty_context != compute_difficulty_context(exp_seal, exp_share) {
            return ShareCheck::Invalid("share difficulty context mismatch".into());
        }
        ShareCheck::Verify
    }

    /// Verifies the ML-DSA-87 signatures of the header, every job and every share.
    ///
    /// Cheap next to a tile recomputation (about 0.5 ms each), so Stage A runs it first.
    /// Jobs are checked against the header's `genesis_id`, which Stage B pins to ours.
    pub fn verify_signatures(block: &Block) -> Result<(), WorkError> {
        verify_header_miner(&block.header).map_err(|e| WorkError::Signature("header", e))?;
        let genesis_id = block.header.context.genesis_id;
        for tx in &block.body.jobs {
            verify_job_client(tx, &genesis_id).map_err(|e| WorkError::Signature("job", e))?;
        }
        for sh in &block.body.shares {
            verify_share_owner(sh).map_err(|e| WorkError::Signature("share", e))?;
        }
        Ok(())
    }

    /// Runs Stage A synchronously: every signature, then VerifyFW on the header and all
    /// shares (§6.2).
    pub fn verify_stage_a(block: &Block) -> Result<(), WorkError> {
        Self::verify_signatures(block)?;
        verify_fresh_work(
            &block.header.context,
            &ClaimedWork::from_header(&block.header),
            block.header.seal_target,
            Lottery::Seal,
        )?;
        for sh in &block.body.shares {
            verify_fresh_work(
                &sh.context,
                &ClaimedWork::from_share(sh),
                sh.share_target,
                Lottery::Share,
            )?;
        }
        Ok(())
    }

    /// Applies §6.8 steps 8–9: `apply_body`, header share registration, insertion and fork
    /// choice. The caller must have run `pre_validate` and Stage A (or trust the block).
    ///
    /// Owners enter the state only through verified signatures: the miner's and every
    /// share owner's ML-DSA-87 signature is checked here even for trusted blocks.
    pub fn apply_verified_block(&mut self, block: Arc<Block>) -> Result<InsertOutcome, TreeError> {
        let id = block.header.block_id;
        if self.metas.contains_key(&id) {
            return Err(TreeError::Duplicate);
        }
        let parent_id = block.header.context.parent_canonical_id;
        let parent_meta = self
            .metas
            .get(&parent_id)
            .ok_or(TreeError::Orphan { parent: parent_id })?
            .clone();
        let parent_state = self.states.get(&parent_id).ok_or(TreeError::TooDeep)?;

        // Step 8: apply_body and require the body's derived fields to match
        let h = parent_meta.height + 1;
        let miner = verify_header_miner(&block.header)
            .map_err(|e| invalid(format!("header signature: {e}")))?;
        let verified: Vec<VerifiedShare> = block
            .body
            .shares
            .iter()
            .map(VerifiedShare::verify)
            .collect::<Result<_, _>>()
            .map_err(|e| invalid(format!("share signature: {e}")))?;
        let (mut post_state, derived) =
            apply_body(parent_state, &self.genesis, h, &block.body.jobs, &verified)
                .map_err(|e| invalid(e.to_string()))?;
        if derived.leases != block.body.leases {
            return Err(invalid("derived leases mismatch"));
        }
        if derived.receipts != block.body.receipts {
            return Err(invalid("derived receipts mismatch"));
        }
        if derived.fee_transitions != block.body.fee_transitions {
            return Err(invalid("derived fee_transitions mismatch"));
        }
        if derived.ledger_digest != block.body.ledger_digest {
            return Err(invalid("derived ledger_digest mismatch"));
        }

        // Step 9: the header's own work is a share if θ_S < Θ_S (§6.7)
        let header_share = top128(&block.header.share_ticket) < block.header.share_target.0;
        if header_share {
            post_state
                .shares
                .entry(block.header.work_id)
                .or_insert(ShareRecord {
                    work_id: block.header.work_id,
                    owner: miner,
                    weight: share_weight(block.header.share_target),
                    height: h,
                    included_at: h,
                });
        }

        let sw = seal_weight(&block.header);
        let cw = parent_meta
            .cumulative_weight
            .checked_add(sw)
            .ok_or_else(|| invalid("cumulative_weight overflow"))?;
        let meta = BlockMeta {
            id,
            parent: parent_id,
            height: h,
            time_bucket: block.header.context.time_bucket,
            seal_target: block.header.seal_target,
            share_target: block.header.share_target,
            seal_weight: sw,
            cumulative_weight: cw,
            miner_key_id: miner.key_id(),
        };

        // Fork choice (§6.5): strictly heavier wins; a tie keeps the current tip.
        // The reorg path is computed before anything is inserted, so a too-deep
        // reorg leaves the tree untouched.
        let reorg = if cw > self.tip_meta().cumulative_weight {
            Some(self.reorg_path(parent_id)?)
        } else {
            None
        };

        let summary = build_summary(&block, &meta, &post_state, header_share)?;
        self.metas.insert(id, meta.clone());
        self.blocks.insert(id, block.clone());
        self.states.insert(id, Arc::new(post_state));
        self.summaries.insert(id, summary);

        let Some((fork_height, branch)) = reorg else {
            return Ok(InsertOutcome {
                meta,
                tip_changed: false,
                connected: Vec::new(),
                disconnected: Vec::new(),
            });
        };
        let keep = (fork_height + 1) as usize;
        let disconnected = self.canonical[keep..]
            .iter()
            .rev()
            .filter_map(|old| self.blocks.get(old).cloned())
            .collect();
        self.canonical.truncate(keep);
        self.canonical.extend(&branch);
        self.canonical.push(id);
        let connected = branch
            .iter()
            .chain(std::iter::once(&id))
            .filter_map(|new| self.blocks.get(new).cloned())
            .collect();
        self.tip = id;
        self.prune_old_snapshots();
        Ok(InsertOutcome {
            meta,
            tip_changed: true,
            connected,
            disconnected,
        })
    }

    /// Fork height and side branch (ascending, up to and including `parent_id`) of a new
    /// tip whose parent is `parent_id`.
    fn reorg_path(&self, parent_id: Digest) -> Result<(u64, Vec<Digest>), TreeError> {
        let view = self.ancestor_view(parent_id).ok_or(TreeError::TooDeep)?;
        if self.tip_meta().height.saturating_sub(view.fork_height) > MAX_REORG_DEPTH {
            return Err(TreeError::TooDeep);
        }
        Ok((view.fork_height, view.branch))
    }

    fn prune_old_snapshots(&mut self) {
        let Some(cutoff) = self.tip_meta().height.checked_sub(MAX_REORG_DEPTH) else {
            return;
        };
        let (metas, genesis_id) = (&self.metas, self.genesis_id);
        self.states
            .retain(|id, _| *id == genesis_id || metas.get(id).is_some_and(|m| m.height >= cutoff));
    }
}

fn build_summary(
    block: &Block,
    meta: &BlockMeta,
    post_state: &NodeState,
    header_share: bool,
) -> Result<BlockSummary, TreeError> {
    let mut jobs = Vec::with_capacity(block.body.jobs.len());
    for (i, tx) in block.body.jobs.iter().enumerate() {
        let (tx_id, job_id) = compute_job_ids(tx).map_err(invalid)?;
        let deposit = tx
            .checked_deposit()
            .ok_or_else(|| invalid("job deposit overflow"))?;
        jobs.push(JobSummary {
            job_id,
            tx_id,
            executor_owner: block.body.leases.get(i).map_or(Digest::ZERO, |l| l.owner),
            deposit,
            queue_fee: tx.queue_fee,
            base_fee: tx.base_fee,
        });
    }
    let receipts = block
        .body
        .receipts
        .iter()
        .map(|r| ReceiptSummary {
            job_id: r.job_id,
            result: r.result,
            executor_owner: r.executor_owner,
            admitted_at: r.admitted_at,
        })
        .collect();
    let shares = block
        .body
        .shares
        .iter()
        .map(|s| ShareSummary {
            work_id: s.work_id,
            owner: owner_key_id(&s.owner_key),
            height: s.context.height,
            weight: share_weight(s.share_target),
        })
        .collect();

    Ok(BlockSummary {
        height: meta.height,
        block_id: meta.id,
        parent_id: meta.parent,
        anchor_id: block.header.context.stable_anchor_id,
        miner_key_id: meta.miner_key_id,
        time_bucket: meta.time_bucket,
        attempt_nonce: block.header.context.attempt_nonce,
        epoch: post_state.ledger.epoch(),
        seal_ticket: block.header.seal_ticket,
        seal_weight: meta.seal_weight,
        cumulative_weight: meta.cumulative_weight,
        jobs,
        receipts,
        shares,
        header_share,
        fee_transitions: block.body.fee_transitions.clone(),
        balances_after: StateCategory::ALL.map(|c| post_state.ledger.balance(c).get()),
        total_after: post_state.ledger.total(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_adapter::fw_context_to_core;
    use crate::keys::{OwnerKeypair, unsigned};
    use crate::template::build_template;
    use primitives::TOTAL_SUPPLY;
    use wire::{DifficultyMode, JobPayload, JobTransaction};
    use work::FreshWorkIds;

    fn key(i: u8) -> Arc<OwnerKeypair> {
        Arc::new(OwnerKeypair::from_seed(&[i; 32]))
    }

    fn test_genesis() -> GenesisConfig {
        let mut g = GenesisConfig::default_testnet(1_700_000_000);
        g.initial_seal_target = Target(u128::MAX);
        g.share_ratio = 1;
        g.difficulty = DifficultyMode::Fixed;
        g.blocks_per_epoch = 4;
        g.settle_delay = 2;
        g.share_maturity_depth = 1;
        g.snapshot_window = 20;
        g.share_inclusion_window = 10;
        g
    }

    fn mine_one(
        tree: &BlockTree,
        parent_id: Digest,
        owner: &Arc<OwnerKeypair>,
        mempool: &[JobTransaction],
        nonce: u64,
        time_bucket: u64,
    ) -> Block {
        let anc = tree.ancestor_view(parent_id).unwrap();
        let parent_meta = tree.get_meta(&parent_id).unwrap();
        let parent_state = tree.get_state(&parent_id).unwrap();
        let tpl = build_template(
            1,
            tree.genesis(),
            tree.genesis_id(),
            &anc,
            parent_meta.height,
            &parent_state,
            mempool,
            &[],
            owner,
            time_bucket,
        )
        .unwrap();
        let ctx = fw_context_to_core(&tpl.context_with_nonce(nonce)).unwrap();
        let inst = ctx.instance();
        let z = inst.x.product(&inst.y);
        tpl.seal(nonce, &FreshWorkIds::derive(&ctx, &z)).unwrap()
    }

    fn sample_job(nonce: u64, expiry_height: u64, genesis_id: &Digest) -> JobTransaction {
        let client = key(200);
        let mut tx = JobTransaction {
            client_key: client.public_key().clone(),
            queue_fee: 100_000_000,
            base_fee: 1_000_000_000,
            v_exec: 8_000_000_000,
            v_da: 500_000_000,
            v_ref: 500_000_000,
            expiry_height,
            nonce,
            payload: JobPayload::ExactDot {
                xs: (1..=512).map(|i| i as f64).collect(),
                ys: (1..=512).rev().map(|i| i as f64).collect(),
            },
            signature: unsigned(),
        };
        client.sign_job(&mut tx, genesis_id).unwrap();
        tx
    }

    #[test]
    fn stf_job_lifecycle_and_tamper_rejection() {
        let g = test_genesis();
        let mut tree = BlockTree::new(g.clone()).unwrap();
        let tx = sample_job(1, 100, &tree.genesis_id());
        let miner = key(1);

        // Epoch 1 (boundary h = 4) books the header shares of blocks 1..=3; that booking
        // is paid into U at the epoch-2 boundary h = 8. Until then the founder tranches
        // lock all of U, so the template leaves the job out.
        let b1 = mine_one(
            &tree,
            tree.tip_id(),
            &miner,
            std::slice::from_ref(&tx),
            0,
            1_700_000_001,
        );
        assert!(b1.body.jobs.is_empty());
        tree.pre_validate(&b1, None).unwrap();
        BlockTree::verify_stage_a(&b1).unwrap();
        tree.apply_verified_block(Arc::new(b1)).unwrap();

        for h in 2..=7 {
            let b = mine_one(
                &tree,
                tree.tip_id(),
                &miner,
                std::slice::from_ref(&tx),
                h,
                1_700_000_000 + h,
            );
            assert!(b.body.jobs.is_empty(), "job must wait for spendable U");
            tree.pre_validate(&b, None).unwrap();
            let out = tree.apply_verified_block(Arc::new(b)).unwrap();
            assert!(out.tip_changed);
            assert_eq!(
                tree.tip_state().ledger.total(),
                u128::from(TOTAL_SUPPLY.get())
            );
        }

        // At h = 8 the epoch-1 booking is paid and a mature cohort exists: the job is admitted.
        let b8 = mine_one(
            &tree,
            tree.tip_id(),
            &miner,
            std::slice::from_ref(&tx),
            8,
            1_700_000_008,
        );
        assert_eq!(b8.body.jobs.len(), 1);
        assert_eq!(b8.body.leases.len(), 1);

        // Another key cannot claim the block: the work binds the miner's key id.
        let mut tampered = b8.clone();
        key(9).sign_header(&mut tampered.header).unwrap();
        assert!(tree.pre_validate(&tampered, None).is_err());

        tree.pre_validate(&b8, None).unwrap();
        BlockTree::verify_stage_a(&b8).unwrap();
        tree.apply_verified_block(Arc::new(b8)).unwrap();
        assert_eq!(tree.tip_state().open_jobs.len(), 1);

        // settle_delay = 2: the job settles at h = 10.
        for h in 9..=10 {
            let b = mine_one(&tree, tree.tip_id(), &miner, &[], h, 1_700_000_000 + h);
            assert_eq!(b.body.receipts.len(), usize::from(h == 10));
            tree.pre_validate(&b, None).unwrap();
            tree.apply_verified_block(Arc::new(b)).unwrap();
        }
        assert!(tree.tip_state().open_jobs.is_empty());
        assert_eq!(
            tree.tip_state().ledger.balance(StateCategory::Escrow).get(),
            0
        );
        let summaries = tree.canonical_summaries(None, 100);
        assert_eq!(summaries.len(), 10);
        assert!(
            summaries
                .iter()
                .all(|s| s.total_after == u128::from(TOTAL_SUPPLY.get()))
        );
    }

    #[test]
    fn fork_choice_heaviest_branch_and_tie_rule() {
        let g = test_genesis();
        let mut tree = BlockTree::new(g).unwrap();
        let genesis_id = tree.genesis_id();

        let (k1, k2) = (key(1), key(2));
        let a1 = mine_one(&tree, genesis_id, &k1, &[], 1, 1_700_000_001);
        let b1 = mine_one(&tree, genesis_id, &k2, &[], 2, 1_700_000_001);

        let out_a1 = tree.apply_verified_block(Arc::new(a1.clone())).unwrap();
        assert!(out_a1.tip_changed);
        assert_eq!(tree.tip_id(), a1.header.block_id);

        // A tie at height 1 keeps the current tip (a1).
        let out_b1 = tree.apply_verified_block(Arc::new(b1.clone())).unwrap();
        assert!(!out_b1.tip_changed);
        assert_eq!(tree.tip_id(), a1.header.block_id);

        // Extending branch B to height 2 makes it heavier: reorg.
        let b2 = mine_one(&tree, b1.header.block_id, &k2, &[], 3, 1_700_000_002);
        tree.pre_validate(&b2, None).unwrap();
        let out_b2 = tree.apply_verified_block(Arc::new(b2.clone())).unwrap();
        assert!(out_b2.tip_changed);
        assert_eq!(tree.tip_id(), b2.header.block_id);
        assert_eq!(out_b2.disconnected.len(), 1);
        assert_eq!(out_b2.disconnected[0].header.block_id, a1.header.block_id);
        let connected: Vec<Digest> = out_b2.connected.iter().map(|b| b.header.block_id).collect();
        assert_eq!(connected, vec![b1.header.block_id, b2.header.block_id]);
        assert_eq!(tree.canonical_id_at(1), Some(b1.header.block_id));
        assert_eq!(tree.canonical_id_at(2), Some(b2.header.block_id));

        // A block on the abandoned branch still validates against its own ancestry.
        let a2 = mine_one(&tree, a1.header.block_id, &k1, &[], 4, 1_700_000_002);
        tree.pre_validate(&a2, None).unwrap();
        let out_a2 = tree.apply_verified_block(Arc::new(a2)).unwrap();
        assert!(!out_a2.tip_changed, "equal weight keeps the current tip");
    }
}
