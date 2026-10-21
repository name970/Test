//! Core adapter (§3.3, §5.4, §5.8, §5.9, §6.1, §6.2, §13).
//!
//! The only module that converts between wire types and the frozen core crates.

use core::fmt;
use ledger::{EmissionRate, Ledger, LedgerParams};
use primitives::DomainSeparatedHash;
use scivm::{OzakiIIProfile, RoundingMode};
use wire::{
    BlockBody, BlockHeader, DOT_LEN, Digest, FwContext, GenesisConfig, JobPayload, JobTransaction,
    MAX_JOB_BYTES, MIN_TARGET, ShareProof, Target, codec,
};
use work::{FreshWorkContext, FreshWorkIds, TILE_DIM};

// ---------------------------------------------------------------------------
// Domain labels (§3.3)
// ---------------------------------------------------------------------------

/// Label of `genesis_id`.
pub const LABEL_GENESIS: &str = "PLAU/TESTNET/GENESIS/v1";
/// Label of the `program_id` and `air_id` tags.
pub const LABEL_TAG: &str = "PLAU/TESTNET/TAG/v1";
/// Label of `miner_policy_digest`.
pub const LABEL_MINER: &str = "PLAU/TESTNET/MINER/v1";
/// Label of `payout_policy_digest`.
pub const LABEL_PAYOUT: &str = "PLAU/TESTNET/PAYOUT/v1";
/// Label of `parent_derived_difficulty_context`.
pub const LABEL_DIFFICULTY: &str = "PLAU/TESTNET/DIFFICULTY/v1";
/// Label of `body_core_root`.
pub const LABEL_BODY: &str = "PLAU/TESTNET/BODY/v1";
/// Label of `tx_id`.
pub const LABEL_JOB: &str = "PLAU/TESTNET/JOB/v1";
/// Label of the RPC client tag.
pub const LABEL_CLIENT: &str = "PLAU/TESTNET/CLIENT/v1";
/// Label of `ledger_digest`.
pub const LABEL_LEDGER: &str = "PLAU/TESTNET/LEDGER/v1";
/// Label of `result_digest`.
pub const LABEL_RESULT: &str = "PLAU/TESTNET/RESULT/v1";

/// Tag name hashed into `program_id`.
pub const PROGRAM_TAG_NAME: &str = "program/fresh-work-int8-512";
/// Tag name hashed into `air_id`.
pub const AIR_TAG_NAME: &str = "air/int8-tile";

const _: () = assert!(DOT_LEN == TILE_DIM);
const _: () = assert!(wire::DIGEST_LEN == primitives::DIGEST_LEN);

/// Converts a core `DomainSeparatedHash` into a wire `Digest`.
pub fn hash_to_digest(h: &DomainSeparatedHash) -> Digest {
    Digest(h.to_bytes())
}

/// Converts a wire `Digest` into a core `DomainSeparatedHash`.
pub fn digest_to_hash(d: &Digest) -> DomainSeparatedHash {
    DomainSeparatedHash::from_bytes(d.0)
}

/// Computes a domain-separated digest using `primitives::DomainSeparatedHash::new`.
pub fn domain_hash(label: &str, parts: &[&[u8]]) -> Digest {
    hash_to_digest(&DomainSeparatedHash::new(label, parts))
}

/// Computes `genesis_id = H(PLAU/TESTNET/GENESIS/v1, codec(GenesisConfig))`.
pub fn compute_genesis_id(config: &GenesisConfig) -> Result<Digest, String> {
    let bytes = codec::encode(config).map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_GENESIS, &[&bytes]))
}

/// Computes `(program_id, air_id)` from §3.3.
pub fn compute_program_and_air_ids() -> (Digest, Digest) {
    let program_id = domain_hash(LABEL_TAG, &[PROGRAM_TAG_NAME.as_bytes()]);
    let air_id = domain_hash(LABEL_TAG, &[AIR_TAG_NAME.as_bytes()]);
    (program_id, air_id)
}

/// Computes `miner_policy_digest = H(PLAU/TESTNET/MINER/v1, owner_be)`.
pub fn compute_miner_policy_digest(owner: u64) -> Digest {
    domain_hash(LABEL_MINER, &[&owner.to_be_bytes()])
}

/// Computes `payout_policy_digest = H(PLAU/TESTNET/PAYOUT/v1, owner_be)`.
pub fn compute_payout_policy_digest(owner: u64) -> Digest {
    domain_hash(LABEL_PAYOUT, &[&owner.to_be_bytes()])
}

/// Computes `parent_derived_difficulty_context = H(PLAU/TESTNET/DIFFICULTY/v1, seal_target, share_target)`.
pub fn compute_difficulty_context(seal_target: Target, share_target: Target) -> Digest {
    domain_hash(
        LABEL_DIFFICULTY,
        &[&seal_target.0.to_be_bytes(), &share_target.0.to_be_bytes()],
    )
}

/// Computes `body_core_root = H(PLAU/TESTNET/BODY/v1, codec(BlockBody))`.
pub fn compute_body_root(body: &BlockBody) -> Result<Digest, String> {
    let bytes = codec::encode(body).map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_BODY, &[&bytes]))
}

/// `(tx_id, job_id)` from the encoded transaction bytes.
fn job_ids_from_encoding(encoded: &[u8]) -> (Digest, u64) {
    let tx_id = domain_hash(LABEL_JOB, &[encoded]);
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&tx_id.0[..8]);
    (tx_id, u64::from_be_bytes(first8))
}

/// Computes `(tx_id, job_id)` for a `JobTransaction` (§5.4).
pub fn compute_job_ids(tx: &JobTransaction) -> Result<(Digest, u64), String> {
    let bytes = codec::encode(tx).map_err(|e| e.to_string())?;
    Ok(job_ids_from_encoding(&bytes))
}

/// Computes `JobTransaction.client` from a client label of at most 64 bytes (§3.3, §9.5).
pub fn compute_client_digest(client_text: &str) -> Result<Digest, String> {
    if client_text.len() > 64 {
        return Err("client text exceeds 64 bytes".to_string());
    }
    Ok(domain_hash(LABEL_CLIENT, &[client_text.as_bytes()]))
}

/// Computes `result_digest = H(PLAU/TESTNET/RESULT/v1, job_id, result)`.
pub fn compute_result_digest(job_id: u64, result: i128) -> Digest {
    domain_hash(
        LABEL_RESULT,
        &[&job_id.to_be_bytes(), &result.to_be_bytes()],
    )
}

/// Extracts the most-significant 128 bits of a digest (§6.1).
pub fn top128(d: &Digest) -> u128 {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&d.0[..16]);
    u128::from_be_bytes(buf)
}

// ---------------------------------------------------------------------------
// Genesis validation (§3.2, §3.4)
// ---------------------------------------------------------------------------

/// Validates a `GenesisConfig` and returns its `LedgerParams`, the genesis `Ledger`
/// and `genesis_id`.
pub fn validate_genesis(config: &GenesisConfig) -> Result<(LedgerParams, Ledger, Digest), String> {
    if config.share_ratio < 1 {
        return Err("share_ratio must be >= 1".to_string());
    }
    if config.initial_seal_target.0 < MIN_TARGET {
        return Err("initial_seal_target must be >= MIN_TARGET".to_string());
    }
    if config.time_bucket_secs < 1 {
        return Err("time_bucket_secs must be >= 1".to_string());
    }
    if config.target_block_secs < config.time_bucket_secs
        || !config
            .target_block_secs
            .is_multiple_of(config.time_bucket_secs)
    {
        return Err(
            "target_block_secs must be >= time_bucket_secs and a multiple of time_bucket_secs"
                .to_string(),
        );
    }
    if config.blocks_per_epoch < 1 {
        return Err("blocks_per_epoch must be >= 1".to_string());
    }
    if config.anchor_depth < 1 {
        return Err("anchor_depth must be >= 1".to_string());
    }
    if config.settle_delay < 1 {
        return Err("settle_delay must be >= 1".to_string());
    }
    if config.share_maturity_depth < 1 || config.share_maturity_depth >= config.snapshot_window {
        return Err("must satisfy 1 <= share_maturity_depth < snapshot_window".to_string());
    }
    if config.share_inclusion_window < 1 {
        return Err("share_inclusion_window must be >= 1".to_string());
    }
    if let wire::DifficultyMode::Windowed { window } = config.difficulty
        && window < 2
    {
        return Err("retarget window must be >= 2 when Windowed".to_string());
    }

    let emission_rate = EmissionRate::new(config.emission_numerator, config.emission_denominator)
        .map_err(|e| e.to_string())?;
    let params = LedgerParams {
        genesis_height: 0,
        blocks_per_epoch: config.blocks_per_epoch,
        maturation_delay: config.maturation_delay,
        emission_rate,
    };
    let ledger = Ledger::genesis(params).map_err(|e| e.to_string())?;
    let genesis_id = compute_genesis_id(config)?;
    Ok((params, ledger, genesis_id))
}

// ---------------------------------------------------------------------------
// SciVM Ozaki-II profile & stateless job validation (§5.4)
// ---------------------------------------------------------------------------

/// Returns the normative `TESTNET_PROFILE`, the literal of `poc/src/main.rs` lines 133–140 (§5.4).
pub fn testnet_ozaki_profile() -> OzakiIIProfile {
    OzakiIIProfile {
        version: 1,
        moduli: vec![255, 254, 253, 251, 247, 241, 239, 233],
        inner_dim: TILE_DIM as u64,
        bound_x: 1 << 23,
        bound_y: 1 << 23,
        rounding: RoundingMode::NearestTiesToEven,
    }
}

/// Error returned during stateless job validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobValidationError {
    /// A fee that must be positive is zero (names the field).
    ZeroFee(&'static str),
    /// `deposit` or the total charge overflows `u64`.
    AmountOverflow,
    /// The encoded job is larger than `MAX_JOB_BYTES`.
    TooLarge(usize),
    /// The payload is malformed or rejected by the SciVM profile.
    InvalidPayload(String),
}

impl fmt::Display for JobValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobValidationError::ZeroFee(field) => write!(f, "{field} must be > 0"),
            JobValidationError::AmountOverflow => write!(f, "fee/deposit addition overflowed u64"),
            JobValidationError::TooLarge(len) => {
                write!(
                    f,
                    "encoded job size {len} exceeds MAX_JOB_BYTES ({MAX_JOB_BYTES})"
                )
            }
            JobValidationError::InvalidPayload(msg) => write!(f, "invalid payload: {msg}"),
        }
    }
}

impl std::error::Error for JobValidationError {}

/// Validates a `JobTransaction` statelessly (§5.4) and returns
/// `(tx_id, job_id, deposit, exact_dot)`.
pub fn validate_job_stateless(
    tx: &JobTransaction,
) -> Result<(Digest, u64, u64, i128), JobValidationError> {
    if tx.queue_fee == 0 {
        return Err(JobValidationError::ZeroFee("queue_fee"));
    }
    if tx.base_fee == 0 {
        return Err(JobValidationError::ZeroFee("base_fee"));
    }
    if tx.v_exec == 0 {
        return Err(JobValidationError::ZeroFee("v_exec"));
    }
    let deposit = tx
        .checked_deposit()
        .ok_or(JobValidationError::AmountOverflow)?;
    tx.checked_total_charge()
        .ok_or(JobValidationError::AmountOverflow)?;

    let encoded = codec::encode(tx)
        .map_err(|e| JobValidationError::InvalidPayload(format!("codec failed: {e}")))?;
    if encoded.len() > MAX_JOB_BYTES {
        return Err(JobValidationError::TooLarge(encoded.len()));
    }

    let dot = match &tx.payload {
        JobPayload::ExactDot { xs, ys } => {
            if xs.len() != DOT_LEN || ys.len() != DOT_LEN {
                return Err(JobValidationError::InvalidPayload(format!(
                    "ExactDot requires {DOT_LEN} operands per vector, got ({}, {})",
                    xs.len(),
                    ys.len()
                )));
            }
            if xs
                .iter()
                .chain(ys)
                .any(|v| !v.is_finite() || v.fract() != 0.0)
            {
                return Err(JobValidationError::InvalidPayload(
                    "every operand must be finite with zero fractional part".to_string(),
                ));
            }
            testnet_ozaki_profile()
                .exact_dot(xs, ys)
                .map_err(|e| JobValidationError::InvalidPayload(e.to_string()))?
        }
    };

    let (tx_id, job_id) = job_ids_from_encoding(&encoded);
    Ok((tx_id, job_id, deposit, dot))
}

// ---------------------------------------------------------------------------
// Fresh Work conversions and Stage A verification (§5.9, §6.2)
// ---------------------------------------------------------------------------

/// Converts a wire `FwContext` to `work::FreshWorkContext`; integer fields that do not fit
/// the core widths make the context invalid.
pub fn fw_context_to_core(ctx: &FwContext) -> Result<FreshWorkContext, WorkError> {
    Ok(FreshWorkContext {
        chain_id: ctx.chain_id,
        genesis_id: digest_to_hash(&ctx.genesis_id),
        protocol_version: ctx
            .protocol_version
            .try_into()
            .map_err(|_| WorkError::ContextFieldOverflow("protocol_version"))?,
        crypto_suite_id: ctx
            .crypto_suite_id
            .try_into()
            .map_err(|_| WorkError::ContextFieldOverflow("crypto_suite_id"))?,
        program_id: digest_to_hash(&ctx.program_id),
        air_id: digest_to_hash(&ctx.air_id),
        generator_version: ctx
            .generator_version
            .try_into()
            .map_err(|_| WorkError::ContextFieldOverflow("generator_version"))?,
        parent_canonical_id: digest_to_hash(&ctx.parent_canonical_id),
        stable_anchor_id: digest_to_hash(&ctx.stable_anchor_id),
        height: ctx.height,
        miner_policy_digest: digest_to_hash(&ctx.miner_policy_digest),
        payout_policy_digest: digest_to_hash(&ctx.payout_policy_digest),
        attempt_nonce: ctx.attempt_nonce,
        body_core_root: digest_to_hash(&ctx.body_core_root),
        parent_derived_difficulty_context: digest_to_hash(&ctx.parent_derived_difficulty_context),
        time_bucket: ctx.time_bucket,
    })
}

/// Fresh Work outputs as wire digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkDigests {
    /// `outputRoot`.
    pub output_root: Digest,
    /// `WorkID`, which is also the ShareID.
    pub work_id: Digest,
    /// `BlockID`.
    pub block_id: Digest,
    /// `θ_S`.
    pub share_ticket: Digest,
    /// `θ_B`.
    pub seal_ticket: Digest,
}

/// Converts `work::FreshWorkIds` into wire digests.
pub fn fresh_work_ids_to_digests(ids: &FreshWorkIds) -> WorkDigests {
    WorkDigests {
        output_root: hash_to_digest(&ids.output_root),
        work_id: hash_to_digest(&ids.work_id),
        block_id: hash_to_digest(&ids.block_id),
        share_ticket: hash_to_digest(&ids.share_ticket),
        seal_ticket: hash_to_digest(&ids.seal_ticket),
    }
}

/// Evaluates one Fresh Work attempt: instance, 512^3 product and identifiers (§5.2).
pub fn evaluate_fresh_work(ctx: &FwContext) -> Result<WorkDigests, WorkError> {
    let core_ctx = fw_context_to_core(ctx)?;
    let inst = core_ctx.instance();
    let z = inst.x.product(&inst.y);
    Ok(fresh_work_ids_to_digests(&FreshWorkIds::derive(
        &core_ctx, &z,
    )))
}

/// Lottery mode for Stage A verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lottery {
    /// Header seal: checks `block_id` and `θ_B < Θ_B`.
    Seal,
    /// Share proof: checks `θ_S < Θ_S`.
    Share,
}

/// Claimed Fresh Work outputs to check in Stage A (§6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedWork {
    /// Claimed `outputRoot`.
    pub output_root: Digest,
    /// Claimed `WorkID`.
    pub work_id: Digest,
    /// Claimed `θ_S`.
    pub share_ticket: Digest,
    /// Claimed `(block_id, θ_B)`, present for a header seal.
    pub seal_fields: Option<(Digest, Digest)>,
}

impl ClaimedWork {
    /// Extracts claimed outputs from a `BlockHeader`.
    pub fn from_header(h: &BlockHeader) -> Self {
        Self {
            output_root: h.output_root,
            work_id: h.work_id,
            share_ticket: h.share_ticket,
            seal_fields: Some((h.block_id, h.seal_ticket)),
        }
    }

    /// Extracts claimed outputs from a `ShareProof`.
    pub fn from_share(s: &ShareProof) -> Self {
        Self {
            output_root: s.output_root,
            work_id: s.work_id,
            share_ticket: s.share_ticket,
            seal_fields: None,
        }
    }
}

/// Stage A Fresh Work verification error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkError {
    /// A context integer does not fit the core field width (names the field).
    ContextFieldOverflow(&'static str),
    /// Recomputed `outputRoot` differs from the claim.
    OutputRootMismatch,
    /// Recomputed `WorkID` differs from the claim.
    WorkIdMismatch,
    /// Recomputed `BlockID` differs from the claim (or a seal claim is missing).
    BlockIdMismatch,
    /// Recomputed `θ_S` differs from the claim.
    ShareTicketMismatch,
    /// Recomputed `θ_B` differs from the claim.
    SealTicketMismatch,
    /// The lottery value is not below its target.
    TargetNotMet,
    /// The verification task failed to run (panicked or was cancelled).
    Internal(String),
}

impl fmt::Display for WorkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkError::ContextFieldOverflow(field) => {
                write!(f, "context field {field} overflowed core integer type")
            }
            WorkError::OutputRootMismatch => write!(f, "output_root mismatch"),
            WorkError::WorkIdMismatch => write!(f, "work_id mismatch"),
            WorkError::BlockIdMismatch => write!(f, "block_id mismatch"),
            WorkError::ShareTicketMismatch => write!(f, "share_ticket mismatch"),
            WorkError::SealTicketMismatch => write!(f, "seal_ticket mismatch"),
            WorkError::TargetNotMet => write!(f, "lottery ticket did not meet target"),
            WorkError::Internal(msg) => write!(f, "verification task failed: {msg}"),
        }
    }
}

impl std::error::Error for WorkError {}

/// Recomputes the 512x512 INT8 tile product for `ctx` and verifies the claimed outputs
/// and the lottery target (§6.2).
pub fn verify_fresh_work(
    ctx: &FwContext,
    claimed: &ClaimedWork,
    target: Target,
    lottery: Lottery,
) -> Result<(), WorkError> {
    let ids = evaluate_fresh_work(ctx)?;
    if ids.output_root != claimed.output_root {
        return Err(WorkError::OutputRootMismatch);
    }
    if ids.work_id != claimed.work_id {
        return Err(WorkError::WorkIdMismatch);
    }
    if ids.share_ticket != claimed.share_ticket {
        return Err(WorkError::ShareTicketMismatch);
    }

    let ticket = match lottery {
        Lottery::Seal => {
            let Some((claimed_block_id, claimed_seal_ticket)) = claimed.seal_fields else {
                return Err(WorkError::BlockIdMismatch);
            };
            if ids.block_id != claimed_block_id {
                return Err(WorkError::BlockIdMismatch);
            }
            if ids.seal_ticket != claimed_seal_ticket {
                return Err(WorkError::SealTicketMismatch);
            }
            ids.seal_ticket
        }
        Lottery::Share => ids.share_ticket,
    };
    if top128(&ticket) >= target.0 {
        return Err(WorkError::TargetNotMet);
    }
    Ok(())
}
