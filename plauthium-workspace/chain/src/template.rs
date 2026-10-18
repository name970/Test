//! Block template building (`chain::template`, §7.4).

use std::sync::Arc;
use wire::{
    Block, BlockBody, BlockHeader, CORE_PROTOCOL_VERSION, Digest, FwContext, GENERATOR_VERSION,
    GenesisConfig, JobTransaction, PLACEHOLDER_SUITE_ID, ShareProof, Target, codec,
};
use work::FreshWorkIds;

use crate::core_adapter::{
    compute_body_root, compute_difficulty_context, compute_job_ids, compute_miner_policy_digest,
    compute_payout_policy_digest, compute_program_and_air_ids, fresh_work_ids_to_digests,
};
use crate::difficulty::{ChainView, expected_targets, share_weight};
use crate::stf::{NodeState, StfError, VerifiedShare, apply_body};

/// Mining template published to the `ConsensusEngine` (§7.4, §8.5).
#[derive(Clone, Debug)]
pub struct Template {
    /// Increases with every published template.
    pub generation: u64,
    /// Context of the next block, with `attempt_nonce = 0`.
    pub context: FwContext,
    /// Body committed by `context.body_core_root`.
    pub body: Arc<BlockBody>,
    /// `Θ_B` of the next height.
    pub seal_target: Target,
    /// `Θ_S` of the next height.
    pub share_target: Target,
    /// Beneficiary of everything mined on this template.
    pub miner_owner: u64,
}

impl Template {
    /// Returns a copy of `context` with `attempt_nonce` set to `nonce`.
    pub fn context_with_nonce(&self, nonce: u64) -> FwContext {
        let mut ctx = self.context.clone();
        ctx.attempt_nonce = nonce;
        ctx
    }

    /// Seals this template into a `Block` using the given `nonce` and `FreshWorkIds`.
    pub fn seal(&self, nonce: u64, ids: &FreshWorkIds) -> Block {
        let d = fresh_work_ids_to_digests(ids);
        Block {
            header: BlockHeader {
                context: self.context_with_nonce(nonce),
                output_root: d.output_root,
                work_id: d.work_id,
                block_id: d.block_id,
                share_ticket: d.share_ticket,
                seal_ticket: d.seal_ticket,
                seal_target: self.seal_target,
                share_target: self.share_target,
                miner_owner: self.miner_owner,
            },
            body: (*self.body).clone(),
        }
    }

    /// Packages an attempt on this template into a `ShareProof` using `nonce` and `FreshWorkIds`.
    pub fn share(&self, nonce: u64, ids: &FreshWorkIds) -> ShareProof {
        let d = fresh_work_ids_to_digests(ids);
        ShareProof {
            context: self.context_with_nonce(nonce),
            owner: self.miner_owner,
            output_root: d.output_root,
            work_id: d.work_id,
            share_ticket: d.share_ticket,
            share_target: self.share_target,
        }
    }

    /// True when `other` would make the miners evaluate exactly the same contexts.
    pub fn same_work_as(&self, other: &Template) -> bool {
        self.context == other.context
            && self.seal_target == other.seal_target
            && self.share_target == other.share_target
            && self.miner_owner == other.miner_owner
    }
}

/// Own shares that satisfy the §6.7 inclusion rules for a block at height `h` on `chain`,
/// sorted by `work_id`, unique, at most `max_shares_per_block`.
fn includable_shares(
    genesis: &GenesisConfig,
    genesis_id: Digest,
    chain: &impl ChainView,
    tip_state: &NodeState,
    own_shares: &[ShareProof],
    h: u64,
) -> Vec<ShareProof> {
    let (program_id, air_id) = compute_program_and_air_ids();
    let mut shares: Vec<ShareProof> = own_shares
        .iter()
        .filter(|sh| {
            let s = sh.context.height;
            if s < 1 || s >= h || s < h.saturating_sub(genesis.share_inclusion_window) {
                return false;
            }
            if tip_state.shares.contains_key(&sh.work_id) {
                return false;
            }
            let (exp_seal, exp_share) = chain.targets_at(s);
            let c = &sh.context;
            sh.owner >= 1
                && c.chain_id == genesis.chain_id
                && c.genesis_id == genesis_id
                && c.protocol_version == CORE_PROTOCOL_VERSION
                && c.crypto_suite_id == PLACEHOLDER_SUITE_ID
                && c.program_id == program_id
                && c.air_id == air_id
                && c.generator_version == GENERATOR_VERSION
                && c.parent_canonical_id == chain.block_id_at(s - 1)
                && c.stable_anchor_id == chain.block_id_at(s.saturating_sub(genesis.anchor_depth))
                && c.miner_policy_digest == compute_miner_policy_digest(sh.owner)
                && c.payout_policy_digest == compute_payout_policy_digest(sh.owner)
                && c.parent_derived_difficulty_context
                    == compute_difficulty_context(exp_seal, exp_share)
                && sh.share_target == exp_share
        })
        .cloned()
        .collect();
    shares.sort_by_key(|s| s.work_id);
    shares.dedup_by_key(|s| s.work_id);
    shares.truncate(genesis.max_shares_per_block as usize);
    shares
}

/// The `VerifiedShare` view of a share proof.
pub fn verified_share(sh: &ShareProof) -> VerifiedShare {
    VerifiedShare {
        work_id: sh.work_id,
        owner: sh.owner,
        weight: share_weight(sh.share_target),
        height: sh.context.height,
    }
}

/// Runs `apply_body` and assembles the body with its derived fields.
fn derive_body(
    tip_state: &NodeState,
    genesis: &GenesisConfig,
    h: u64,
    jobs: Vec<JobTransaction>,
    shares: &[ShareProof],
) -> Result<BlockBody, StfError> {
    let verified: Vec<VerifiedShare> = shares.iter().map(verified_share).collect();
    let (_, derived) = apply_body(tip_state, genesis, h, &jobs, &verified)?;
    Ok(BlockBody {
        jobs,
        shares: shares.to_vec(),
        leases: derived.leases,
        receipts: derived.receipts,
        fee_transitions: derived.fee_transitions,
        ledger_digest: derived.ledger_digest,
    })
}

/// Builds a new mining `Template` on top of the tip at `tip_height` of `chain` (§7.4).
///
/// Jobs are taken in FIFO order (no fee priority, WP 6.16) as long as the body stays
/// valid; unfundable jobs are skipped and stay in the mempool. Errors only if even an
/// empty body cannot be derived, which would indicate a corrupted tip state.
#[allow(clippy::too_many_arguments)]
pub fn build_template(
    generation: u64,
    genesis: &GenesisConfig,
    genesis_id: Digest,
    chain: &impl ChainView,
    tip_height: u64,
    tip_state: &NodeState,
    mempool_fifo: &[JobTransaction],
    own_shares: &[ShareProof],
    miner_owner: u64,
    now_bucket: u64,
) -> Result<Template, StfError> {
    let h = tip_height + 1;
    let mut shares = includable_shares(genesis, genesis_id, chain, tip_state, own_shares, h);
    let verified: Vec<VerifiedShare> = shares.iter().map(verified_share).collect();
    if apply_body(tip_state, genesis, h, &[], &verified).is_err() {
        shares.clear();
    }

    // Greedily add jobs in FIFO order; the body itself stays sorted by tx_id (§7.4).
    let mut selected: Vec<(Digest, JobTransaction)> = Vec::new();
    for tx in mempool_fifo {
        if selected.len() as u64 >= genesis.max_jobs_per_block {
            break;
        }
        let Ok((tx_id, _)) = compute_job_ids(tx) else {
            continue;
        };
        let Err(pos) = selected.binary_search_by_key(&tx_id, |(id, _)| *id) else {
            continue;
        };
        let mut candidate = selected.clone();
        candidate.insert(pos, (tx_id, tx.clone()));
        let jobs: Vec<JobTransaction> = candidate.iter().map(|(_, j)| j.clone()).collect();
        let verified: Vec<VerifiedShare> = shares.iter().map(verified_share).collect();
        if apply_body(tip_state, genesis, h, &jobs, &verified).is_ok() {
            selected = candidate;
        }
    }

    let mut jobs: Vec<JobTransaction> = selected.into_iter().map(|(_, j)| j).collect();
    let mut body = derive_body(tip_state, genesis, h, jobs.clone(), &shares)?;
    let size_limit = genesis.max_block_bytes.saturating_sub(4096) as usize;
    while !jobs.is_empty() && codec::encode(&body).map_or(usize::MAX, |b| b.len()) > size_limit {
        jobs.pop();
        body = derive_body(tip_state, genesis, h, jobs.clone(), &shares)?;
    }

    let (seal_target, share_target) = expected_targets(h, chain, genesis);
    let body_core_root = compute_body_root(&body).map_err(StfError::SchedulerError)?;
    let (program_id, air_id) = compute_program_and_air_ids();
    let context = FwContext {
        chain_id: genesis.chain_id,
        genesis_id,
        protocol_version: CORE_PROTOCOL_VERSION,
        crypto_suite_id: PLACEHOLDER_SUITE_ID,
        program_id,
        air_id,
        generator_version: GENERATOR_VERSION,
        parent_canonical_id: chain.block_id_at(tip_height),
        stable_anchor_id: chain.block_id_at(h.saturating_sub(genesis.anchor_depth)),
        height: h,
        miner_policy_digest: compute_miner_policy_digest(miner_owner),
        payout_policy_digest: compute_payout_policy_digest(miner_owner),
        attempt_nonce: 0,
        body_core_root,
        parent_derived_difficulty_context: compute_difficulty_context(seal_target, share_target),
        time_bucket: chain.time_bucket_at(tip_height).max(now_bucket),
    };

    Ok(Template {
        generation,
        context,
        body: Arc::new(body),
        seal_target,
        share_target,
        miner_owner,
    })
}
