//! Core adapter (§3.3, §5.4, §5.8, §5.9, §6.1, §6.2, §13).
//!
//! The only module that converts between wire types and the frozen core crates. It is
//! also where owner identity crosses into the core: an ML-DSA-87 signature is verified
//! here before the owner's key id and its 64-bit `scheduler::OwnerId` reach `apply_body`.

use core::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use ledger::{EmissionRate, Ledger, LedgerParams};
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, MlDsa87, VerifyingKey};
use primitives::{DomainSeparatedHash, labels, pq_sizes};
use scivm::{OzakiIIProfile, RoundingMode};
use wire::{
    Block, BlockBody, BlockHeader, DOT_LEN, Digest, ENCODED_HEADER_LEN, ENCODED_SHARE_LEN,
    FwContext, GenesisConfig, JobPayload, JobTransaction, MAX_JOB_BYTES, MIN_TARGET,
    PUBLIC_KEY_LEN, PublicKey, SIGNATURE_LEN, ShareProof, Signature, Target, codec,
};

/// Bytes `validate_genesis` requires beyond the header and a full set of shares: room
/// for the body's framing (vector lengths, `ledger_digest`) and its derived fields.
pub const BLOCK_HEADROOM: u64 = 16_384;
use work::{FreshWorkContext, FreshWorkIds, TILE_DIM};

// ---------------------------------------------------------------------------
// Domain labels (§3.3)
// ---------------------------------------------------------------------------

/// Label of `genesis_id`.
pub const LABEL_GENESIS: &str = "PLAU/TESTNET/GENESIS/v1";
/// Label of the `program_id` and `air_id` tags.
pub const LABEL_TAG: &str = "PLAU/TESTNET/TAG/v1";
/// Label of `miner_policy_digest` (v2: hashes the owner key id).
pub const LABEL_MINER: &str = "PLAU/TESTNET/MINER/v2";
/// Label of `payout_policy_digest` (v2: hashes the owner key id).
pub const LABEL_PAYOUT: &str = "PLAU/TESTNET/PAYOUT/v2";
/// Label of `parent_derived_difficulty_context`.
pub const LABEL_DIFFICULTY: &str = "PLAU/TESTNET/DIFFICULTY/v1";
/// Label of `body_core_root`.
pub const LABEL_BODY: &str = "PLAU/TESTNET/BODY/v1";
/// Label of `tx_id` (v2: hashes the encoding without the signature).
pub const LABEL_JOB: &str = "PLAU/TESTNET/JOB/v2";
/// Label of an owner key id, `H(PLAU/TESTNET/OWNER/v1, public key)`.
pub const LABEL_OWNER: &str = "PLAU/TESTNET/OWNER/v1";
/// Label of the digest a client signs for a job.
pub const LABEL_SIG_JOB: &str = "PLAU/TESTNET/SIG/JOB/v1";
/// Label of the digest an owner signs for a share proof.
pub const LABEL_SIG_SHARE: &str = "PLAU/TESTNET/SIG/SHARE/v1";
/// Label of the digest a miner signs for a block header.
pub const LABEL_SIG_HEADER: &str = "PLAU/TESTNET/SIG/HEADER/v1";
/// FIPS 204 context string of every ML-DSA-87 signature (spec eqs. 2.3–2.5).
pub const SIGNATURE_CONTEXT: &[u8] = labels::PLAU_TX.as_bytes();
/// Label of a block's relay id (local, not consensus).
pub const LABEL_RELAY_BLOCK: &str = "PLAU/TESTNET/RELAY/BLOCK/v1";
/// Label of a job's relay id (local, not consensus).
pub const LABEL_RELAY_JOB: &str = "PLAU/TESTNET/RELAY/JOB/v1";
/// Label of a share proof's relay id (local, not consensus).
pub const LABEL_RELAY_SHARE: &str = "PLAU/TESTNET/RELAY/SHARE/v1";
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
const _: () = assert!(PUBLIC_KEY_LEN as u64 == pq_sizes::ML_DSA_87_PUBLIC_KEY);
const _: () = assert!(SIGNATURE_LEN as u64 == pq_sizes::ML_DSA_87_SIGNATURE);

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

/// Computes `miner_policy_digest = H(PLAU/TESTNET/MINER/v2, owner_key_id)`.
pub fn compute_miner_policy_digest(owner_key_id: &Digest) -> Digest {
    domain_hash(LABEL_MINER, &[&owner_key_id.0])
}

/// Computes `payout_policy_digest = H(PLAU/TESTNET/PAYOUT/v2, owner_key_id)`.
pub fn compute_payout_policy_digest(owner_key_id: &Digest) -> Digest {
    domain_hash(LABEL_PAYOUT, &[&owner_key_id.0])
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

/// Computes `(tx_id, job_id)` for a `JobTransaction` (§5.4). `tx_id` hashes the
/// encoding without the signature, so re-signing a job never changes its identity.
pub fn compute_job_ids(tx: &JobTransaction) -> Result<(Digest, u64), String> {
    let bytes = tx.unsigned_encoding().map_err(|e| e.to_string())?;
    Ok(job_ids_from_encoding(&bytes))
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
    // A full set of shares must always fit, or a template holding them (every node
    // picks the same gossiped shares) would be too large to be a valid block.
    let full_shares = config
        .max_shares_per_block
        .checked_mul(ENCODED_SHARE_LEN as u64)
        .and_then(|b| b.checked_add(ENCODED_HEADER_LEN as u64 + BLOCK_HEADROOM));
    if full_shares.is_none_or(|need| need > config.max_block_bytes) {
        return Err(format!(
            "max_block_bytes must hold the header, max_shares_per_block shares of {ENCODED_SHARE_LEN} bytes and {BLOCK_HEADROOM} bytes of headroom"
        ));
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
    // `tx_id` hashes the bytes without the trailing signature, as in `compute_job_ids`.
    let unsigned_len = encoded.len() - SIGNATURE_LEN;

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

    let (tx_id, job_id) = job_ids_from_encoding(&encoded[..unsigned_len]);
    Ok((tx_id, job_id, deposit, dot))
}

// ---------------------------------------------------------------------------
// Owner identity: key ids, signing digests and ML-DSA-87 verification
// ---------------------------------------------------------------------------

/// Computes an owner key id, `H(PLAU/TESTNET/OWNER/v1, public key)`.
pub fn owner_key_id(key: &PublicKey) -> Digest {
    domain_hash(LABEL_OWNER, &[&key.as_bytes()[..]])
}

/// Narrows a key id to the 64-bit `scheduler::OwnerId` (its first 8 bytes, big-endian).
///
/// The frozen scheduler uses this only for owner capacity bounds. Rewards, leases
/// and receipts carry the full 512-bit key id, so a colliding 64-bit value (about
/// 2^32 key generations) cannot redirect funds or attributions.
pub fn owner_id_from_key_id(key_id: &Digest) -> u64 {
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&key_id.0[..8]);
    u64::from_be_bytes(first8)
}

/// The digest a client signs: `H(PLAU/TESTNET/SIG/JOB/v1, genesis_id, tx_id)`.
///
/// `tx_id` covers every field but the signature; `genesis_id` stops a job signed for
/// one testnet from being replayed on another.
pub fn job_signing_digest(tx: &JobTransaction, genesis_id: &Digest) -> Result<Digest, String> {
    let (tx_id, _) = compute_job_ids(tx)?;
    Ok(domain_hash(LABEL_SIG_JOB, &[&genesis_id.0, &tx_id.0]))
}

/// The digest an owner signs: `H(PLAU/TESTNET/SIG/SHARE/v1, unsigned share encoding)`.
/// The context inside binds the chain and the genesis.
pub fn share_signing_digest(share: &ShareProof) -> Result<Digest, String> {
    let bytes = share.unsigned_encoding().map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_SIG_SHARE, &[&bytes]))
}

/// The digest a miner signs: `H(PLAU/TESTNET/SIG/HEADER/v1, unsigned header encoding)`.
pub fn header_signing_digest(header: &BlockHeader) -> Result<Digest, String> {
    let bytes = header.unsigned_encoding().map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_SIG_HEADER, &[&bytes]))
}

/// Signature verification error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureError {
    /// The signed object could not be encoded.
    Encoding(String),
    /// The signature bytes are not a valid ML-DSA-87 encoding (FIPS 204 `sigDecode`).
    Malformed,
    /// ML-DSA-87 verification failed.
    Invalid,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignatureError::Encoding(msg) => write!(f, "cannot encode the signed object: {msg}"),
            SignatureError::Malformed => write!(f, "malformed ML-DSA-87 signature"),
            SignatureError::Invalid => write!(f, "ML-DSA-87 signature verification failed"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Relay id of a block: a hash of its full encoding, header (signature included) and
/// body, so it names the exact block bytes.
///
/// `block_id` leaves out the signature (and other claimed fields), so anyone can make
/// an invalid copy that shares an honest block's `block_id`. Relay and rejection
/// caches must key on this id, or such a copy would shadow the genuine block. The body
/// is hashed too: `body_core_root` binds it only once checked, so a genuine header
/// with a forged body must not share the genuine block's relay id either.
pub fn block_relay_id(block: &Block) -> Result<Digest, String> {
    let bytes = codec::encode(block).map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_RELAY_BLOCK, &[&bytes]))
}

/// Relay id of a job: a hash of its full encoding, signature included. Relay caches
/// key on it for the reason given at [`block_relay_id`]; the mempool and the chain
/// keep deduplicating by `tx_id`.
pub fn job_relay_id(tx: &JobTransaction) -> Result<Digest, String> {
    let bytes = codec::encode(tx).map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_RELAY_JOB, &[&bytes]))
}

/// Relay id of a share proof: a hash of its full encoding, signature included. Relay
/// and rejection caches key on it for the reason given at [`block_relay_id`]; the
/// share pool keeps one share per `work_id`.
pub fn share_relay_id(share: &ShareProof) -> Result<Digest, String> {
    let bytes = codec::encode(share).map_err(|e| e.to_string())?;
    Ok(domain_hash(LABEL_RELAY_SHARE, &[&bytes]))
}

/// An owner whose ML-DSA-87 signature over a specific object has been verified.
///
/// Only [`verify_share_owner`], [`verify_header_miner`] and [`verify_job_client`]
/// create one, each from the signature on its own object, so a `VerifiedOwner` cannot
/// be minted from a signature over some unrelated digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedOwner {
    key_id: Digest,
    owner_id: u64,
}

impl VerifiedOwner {
    /// The owner key id, `H(PLAU/TESTNET/OWNER/v1, public key)`.
    pub fn key_id(&self) -> Digest {
        self.key_id
    }

    /// The 64-bit `scheduler::OwnerId` derived from the key id.
    pub fn owner_id(&self) -> u64 {
        self.owner_id
    }
}

/// Decoded verifying keys kept by [`verify_ml_dsa_87`], most recently used last.
const KEY_CACHE_CAPACITY: usize = 64;

type KeyCache = Mutex<Vec<(Digest, Arc<VerifyingKey<MlDsa87>>)>>;

/// The decoded verifying key for `key`, whose key id is `key_id`.
///
/// Decoding (FIPS 204 `pkDecode` plus `ExpandA`) costs about two thirds of a
/// verification, and a testnet has few distinct signers, so recently used keys are
/// cached. Entries are keyed by the SHA3-512 key id of the encoded key.
fn decoded_key(key_id: &Digest, key: &PublicKey) -> Arc<VerifyingKey<MlDsa87>> {
    static CACHE: OnceLock<KeyCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::with_capacity(KEY_CACHE_CAPACITY)));
    {
        let mut entries = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = entries.iter().position(|(id, _)| id == key_id) {
            let entry = entries.remove(pos);
            let decoded = entry.1.clone();
            entries.push(entry);
            return decoded;
        }
    }
    let encoded = EncodedVerifyingKey::<MlDsa87>::try_from(&key.as_bytes()[..])
        .expect("invariant: a PublicKey holds exactly 2,592 bytes");
    let decoded = Arc::new(VerifyingKey::<MlDsa87>::decode(&encoded));
    let mut entries = cache.lock().unwrap_or_else(|e| e.into_inner());
    if !entries.iter().any(|(id, _)| id == key_id) {
        if entries.len() >= KEY_CACHE_CAPACITY {
            entries.remove(0);
        }
        entries.push((*key_id, decoded.clone()));
    }
    decoded
}

/// Checks an ML-DSA-87 signature (FIPS 204 `ML-DSA.Verify` with context
/// [`SIGNATURE_CONTEXT`]) over a domain-separated `digest`.
///
/// This is the raw check. Code that needs an owner identity uses the object-specific
/// verifiers below, which are the only way to obtain a [`VerifiedOwner`].
pub fn verify_ml_dsa_87(
    key: &PublicKey,
    digest: &Digest,
    signature: &Signature,
) -> Result<(), SignatureError> {
    verify_owner_signature(key, digest, signature).map(|_| ())
}

/// Verifies `signature` and, only on success, returns the signer's key id and
/// derived `OwnerId`.
fn verify_owner_signature(
    key: &PublicKey,
    digest: &Digest,
    signature: &Signature,
) -> Result<VerifiedOwner, SignatureError> {
    let encoded_sig = EncodedSignature::<MlDsa87>::try_from(&signature.as_bytes()[..])
        .map_err(|_| SignatureError::Malformed)?;
    let sig =
        ml_dsa::Signature::<MlDsa87>::decode(&encoded_sig).ok_or(SignatureError::Malformed)?;
    let key_id = owner_key_id(key);
    if !decoded_key(&key_id, key).verify_with_context(&digest.0, SIGNATURE_CONTEXT, &sig) {
        return Err(SignatureError::Invalid);
    }
    Ok(VerifiedOwner {
        key_id,
        owner_id: owner_id_from_key_id(&key_id),
    })
}

/// Verifies a share proof's owner signature.
pub fn verify_share_owner(share: &ShareProof) -> Result<VerifiedOwner, SignatureError> {
    let digest = share_signing_digest(share).map_err(SignatureError::Encoding)?;
    verify_owner_signature(&share.owner_key, &digest, &share.signature)
}

/// Verifies a block header's miner signature.
pub fn verify_header_miner(header: &BlockHeader) -> Result<VerifiedOwner, SignatureError> {
    let digest = header_signing_digest(header).map_err(SignatureError::Encoding)?;
    verify_owner_signature(&header.miner_key, &digest, &header.signature)
}

/// Verifies a job's client signature for the chain with `genesis_id`.
pub fn verify_job_client(
    tx: &JobTransaction,
    genesis_id: &Digest,
) -> Result<VerifiedOwner, SignatureError> {
    let digest = job_signing_digest(tx, genesis_id).map_err(SignatureError::Encoding)?;
    verify_owner_signature(&tx.client_key, &digest, &tx.signature)
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
    /// A header, share or job signature does not verify (names the object).
    Signature(&'static str, SignatureError),
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
            WorkError::Signature(what, err) => write!(f, "{what} signature: {err}"),
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
