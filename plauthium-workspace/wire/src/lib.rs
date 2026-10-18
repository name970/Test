//! Plauthium Testnet wire types, protocol constants, and deterministic binary codec.
//!
//! Every type marked *normative* is part of the protocol: its field and variant
//! order is the bincode serialization order and is hashed into `tx_id`,
//! `body_core_root` and `genesis_id` (testnet blueprint §4.2). Do not reorder.

#![forbid(unsafe_code)]

use core::fmt;
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Protocol constants (`wire::consts`, blueprint §3.1).
pub mod consts {
    use core::time::Duration;

    /// P2P protocol version in `Handshake`.
    pub const WIRE_VERSION: u32 = 1;
    /// `protocol_version` context field (core `*/v2` labels).
    pub const CORE_PROTOCOL_VERSION: u64 = 2;
    /// `crypto_suite_id`; 0 means "non-cryptographic placeholder suite".
    pub const PLACEHOLDER_SUITE_ID: u64 = 0;
    /// `generator_version` context field.
    pub const GENERATOR_VERSION: u64 = 2;
    /// Bytes per digest.
    pub const DIGEST_LEN: usize = 64;
    /// Lower clamp for any lottery target.
    pub const MIN_TARGET: u128 = 1u128 << 64;
    /// Operand length of an `ExactDot` job (`= work::TILE_DIM`).
    pub const DOT_LEN: usize = 512;
    /// Encoded `JobTransaction` limit in bytes.
    pub const MAX_JOB_BYTES: usize = 16_384;
    /// TCP frame limit in bytes.
    pub const MAX_FRAME_BYTES: usize = 16_777_216;
    /// Maximum blocks per `SyncResponse`.
    pub const MAX_SYNC_BLOCKS: usize = 64;
    /// Soft byte limit per `SyncResponse`.
    pub const MAX_SYNC_BYTES: usize = 8_388_608;
    /// Initial overlap when starting a sync.
    pub const SYNC_LOOKBACK: u64 = 8;
    /// Deepest reorg a node follows; also snapshot retention depth.
    pub const MAX_REORG_DEPTH: u64 = 128;
    /// Allowed clock lead of `time_bucket` in seconds before a block is deferred.
    pub const MAX_FUTURE_SECS: u64 = 15;
    /// Wait timeout for the peer's `Handshake`.
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Timeout for one outstanding `SyncRequest` per peer.
    pub const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
    /// Maximum blocks with an unknown parent kept in memory.
    pub const ORPHAN_LIMIT: usize = 256;
    /// Maximum transactions in the mempool.
    pub const MEMPOOL_CAPACITY: usize = 4_096;
}

pub use consts::*;

/// Canonical bincode codec (`wire::codec`), the only module that calls `bincode`.
pub mod codec {
    use super::MAX_FRAME_BYTES;
    use bincode::Options;
    use core::fmt;
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// Codec error wrapper.
    #[derive(Debug)]
    pub struct CodecError(pub bincode::Error);

    impl fmt::Display for CodecError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "codec error: {}", self.0)
        }
    }

    impl std::error::Error for CodecError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    impl From<bincode::Error> for CodecError {
        fn from(err: bincode::Error) -> Self {
            CodecError(err)
        }
    }

    /// Normative bincode options (§4.2).
    pub fn options() -> impl Options + Copy {
        bincode::DefaultOptions::new()
            .with_limit(MAX_FRAME_BYTES as u64)
            .with_little_endian()
            .with_fixint_encoding()
            .reject_trailing_bytes()
    }

    /// Encodes a value using the canonical wire format.
    pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, CodecError> {
        options().serialize(v).map_err(CodecError)
    }

    /// Decodes a value using the canonical wire format, rejecting trailing bytes.
    pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, CodecError> {
        options().deserialize(b).map_err(CodecError)
    }
}

// ---------------------------------------------------------------------------
// Primitive wire types (§4.3)
// ---------------------------------------------------------------------------

/// Canonical bytes of a `DomainSeparatedHash`.
// normative
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; DIGEST_LEN]);

impl Digest {
    /// All-zero digest.
    pub const ZERO: Self = Digest([0u8; DIGEST_LEN]);

    /// Encodes the full digest as 128 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Returns the first `n` hex characters of the digest.
    pub fn short_hex(&self, n: usize) -> String {
        let full = self.to_hex();
        full.chars().take(n).collect()
    }

    /// Parses a 128-character hex string (optionally `0x`-prefixed) into a `Digest`.
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let bytes = from_hex(s)?;
        if bytes.len() != DIGEST_LEN {
            return Err(format!(
                "expected {} hex chars ({} bytes), got {} bytes",
                DIGEST_LEN * 2,
                DIGEST_LEN,
                bytes.len()
            ));
        }
        let mut arr = [0u8; DIGEST_LEN];
        arr.copy_from_slice(&bytes);
        Ok(Digest(arr))
    }
}

impl Default for Digest {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short_hex(8))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return s.serialize_str(&to_hex(&self.0));
        }
        let mut t = s.serialize_tuple(DIGEST_LEN)?;
        for b in &self.0 {
            t.serialize_element(b)?;
        }
        t.end()
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            struct HexVisitor;
            impl Visitor<'_> for HexVisitor {
                type Value = Digest;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "a {}-character lowercase hex string", DIGEST_LEN * 2)
                }

                fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                    if v.len() != DIGEST_LEN * 2 {
                        return Err(E::custom(format!(
                            "expected {} hex chars, got {}",
                            DIGEST_LEN * 2,
                            v.len()
                        )));
                    }
                    Digest::from_hex(v).map_err(E::custom)
                }
            }
            return d.deserialize_str(HexVisitor);
        }

        struct TupleVisitor;
        impl<'de> Visitor<'de> for TupleVisitor {
            type Value = Digest;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a {DIGEST_LEN}-byte tuple")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut arr = [0u8; DIGEST_LEN];
                for (i, slot) in arr.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                Ok(Digest(arr))
            }
        }
        d.deserialize_tuple(DIGEST_LEN, TupleVisitor)
    }
}

/// Lottery target over the top 128 bits (§6.1).
// normative
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target(pub u128);

impl Target {
    /// Formats the target as `"0x"` followed by 32 lowercase hex digits.
    pub fn to_hex(self) -> String {
        format!("0x{:032x}", self.0)
    }

    /// Parses a `"0x"`-prefixed 32-character hex string.
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let hex_part = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .ok_or_else(|| "Target hex string must start with 0x".to_string())?;
        if hex_part.len() != 32 {
            return Err(format!(
                "expected 32 hex chars after 0x, got {}",
                hex_part.len()
            ));
        }
        u128::from_str_radix(hex_part, 16)
            .map(Target)
            .map_err(|e| format!("invalid Target hex: {e}"))
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_hex())
        } else {
            s.serialize_u128(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            struct TargetHexVisitor;
            impl Visitor<'_> for TargetHexVisitor {
                type Value = Target;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("0x followed by 32 hex characters")
                }

                fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                    Target::from_hex(v).map_err(E::custom)
                }
            }
            d.deserialize_str(TargetHexVisitor)
        } else {
            struct U128Visitor;
            impl Visitor<'_> for U128Visitor {
                type Value = Target;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a u128 target")
                }

                fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> {
                    Ok(Target(v))
                }
            }
            d.deserialize_u128(U128Visitor)
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".to_string());
    }
    s.as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok((hex_val(pair[0])? << 4) | hex_val(pair[1])?))
        .collect()
}

fn hex_val(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex character: {:?}", b as char)),
    }
}

// ---------------------------------------------------------------------------
// Genesis (§3.4)
// ---------------------------------------------------------------------------

/// Difficulty retarget mode.
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DifficultyMode {
    /// The seal target never changes.
    Fixed,
    /// The seal target is rescaled every `window` blocks (§6.4).
    Windowed {
        /// Retarget window in blocks (`>= 2`).
        window: u64,
    },
}

/// Consensus-critical genesis configuration (§3.2, §3.4).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Human-readable chain name (label only).
    pub name: String,
    /// Context field 1, checked in `Handshake`.
    pub chain_id: u64,
    /// `time_bucket` of the virtual height 0.
    pub genesis_time_bucket: u64,
    /// Seconds per time bucket.
    pub time_bucket_secs: u64,
    /// Block interval the windowed retarget aims for.
    pub target_block_secs: u64,
    /// Seal target `Θ_B` of the first blocks.
    pub initial_seal_target: Target,
    /// `Θ_S / Θ_B`.
    pub share_ratio: u64,
    /// Retarget rule.
    pub difficulty: DifficultyMode,
    /// `LedgerParams::blocks_per_epoch`.
    pub blocks_per_epoch: u64,
    /// Maturation delay `L` in epochs.
    pub maturation_delay: u64,
    /// Numerator `u` of the emission rate `η = u / v`.
    pub emission_numerator: u64,
    /// Denominator `v` of the emission rate `η = u / v`.
    pub emission_denominator: u64,
    /// `stable_anchor_id` is the ancestor at `h - anchor_depth`.
    pub anchor_depth: u64,
    /// A job admitted at `h` settles at `h + settle_delay`.
    pub settle_delay: u64,
    /// Blocks after inclusion before a share counts for leases.
    pub share_maturity_depth: u64,
    /// The D'Hondt cohort covers inclusions in `[h - snapshot_window, h - share_maturity_depth]`.
    pub snapshot_window: u64,
    /// A share mined at height `s` can be included up to `s + share_inclusion_window`.
    pub share_inclusion_window: u64,
    /// Maximum jobs per block.
    pub max_jobs_per_block: u64,
    /// Maximum share proofs per block.
    pub max_shares_per_block: u64,
    /// Maximum encoded `Block` size.
    pub max_block_bytes: u64,
}

impl GenesisConfig {
    /// Default testnet genesis parameters from §3.2.
    pub fn default_testnet(genesis_time_bucket: u64) -> Self {
        Self {
            name: "plauthium-testnet-0".to_string(),
            chain_id: 0x504C_4154,
            genesis_time_bucket,
            time_bucket_secs: 1,
            target_block_secs: 10,
            initial_seal_target: Target(u128::MAX / 32),
            share_ratio: 4,
            difficulty: DifficultyMode::Windowed { window: 32 },
            blocks_per_epoch: 30,
            maturation_delay: 2,
            emission_numerator: 1,
            emission_denominator: 1_000,
            anchor_depth: 6,
            settle_delay: 2,
            share_maturity_depth: 3,
            snapshot_window: 60,
            share_inclusion_window: 30,
            max_jobs_per_block: 32,
            max_shares_per_block: 16,
            max_block_bytes: 1_048_576,
        }
    }
}

// ---------------------------------------------------------------------------
// Block and transaction structures (§5)
// ---------------------------------------------------------------------------

/// Fresh Work context: the 16 fields of WP (5.1).
// normative — field order = WP (5.1) = serialization order
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FwContext {
    /// 1: chain domain.
    pub chain_id: u64,
    /// 2: Genesis domain.
    pub genesis_id: Digest,
    /// 3: protocol version.
    pub protocol_version: u64,
    /// 4: cryptographic suite.
    pub crypto_suite_id: u64,
    /// 5: Fresh Work program.
    pub program_id: Digest,
    /// 6: AIR of the Fresh Work proof.
    pub air_id: Digest,
    /// 7: instance generator version.
    pub generator_version: u64,
    /// 8: parent link, the parent's `block_id`.
    pub parent_canonical_id: Digest,
    /// 9: parent link to the stable anchor ancestor.
    pub stable_anchor_id: Digest,
    /// 10: height of the attempted block.
    pub height: u64,
    /// 11: miner policy, fixing the beneficiary before work.
    pub miner_policy_digest: Digest,
    /// 12: payout policy.
    pub payout_policy_digest: Digest,
    /// 13: attempt nonce.
    pub attempt_nonce: u64,
    /// 14: body root fixed before the attempt.
    pub body_core_root: Digest,
    /// 15: difficulty context derived from the parent.
    pub parent_derived_difficulty_context: Digest,
    /// 16: time bucket.
    pub time_bucket: u64,
}

/// Block header (§5.2).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    /// The 16 bound context fields; carry both parent links.
    pub context: FwContext,
    /// `H(FW/OUTPUT/v2, XY)`.
    pub output_root: Digest,
    /// Work identifier, which is also the ShareID.
    pub work_id: Digest,
    /// `H(BLOCK/v2, x, outputRoot)`: the block's identity.
    pub block_id: Digest,
    /// Share lottery value `θ_S`.
    pub share_ticket: Digest,
    /// Seal lottery value `θ_B`.
    pub seal_ticket: Digest,
    /// `Θ_B` for this height, bound through context field 15.
    pub seal_target: Target,
    /// `Θ_S` for this height, bound through context field 15.
    pub share_target: Target,
    /// `scheduler::OwnerId` of the beneficiary, bound through context fields 11 and 12.
    pub miner_owner: u64,
}

/// Full block (§5.2).
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// The sealed header.
    pub header: BlockHeader,
    /// The body committed by `header.context.body_core_root`.
    pub body: BlockBody,
}

/// Block body (§5.3).
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockBody {
    /// Newly funded and admitted jobs; sorted by `tx_id`, unique.
    pub jobs: Vec<JobTransaction>,
    /// Fresh Work shares (rights); sorted by `work_id`, unique.
    pub shares: Vec<ShareProof>,
    /// Derived: `leases[i]` belongs to `jobs[i]`.
    pub leases: Vec<LeaseAssignment>,
    /// Derived: jobs admitted at `h - settle_delay`, executed and settled.
    pub receipts: Vec<SettlementReceipt>,
    /// Derived: every ledger operation of this block, in execution order.
    pub fee_transitions: Vec<FeeTransition>,
    /// Derived: commitment to the post-state (§5.8).
    pub ledger_digest: Digest,
}

/// Commercial job transaction (§5.4).
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobTransaction {
    /// Free tag, unauthenticated in v0.
    pub client: Digest,
    /// Queue fee `f_queue` in atoms, `> 0`.
    pub queue_fee: u64,
    /// Base Fee `F_base` in atoms, `> 0`.
    pub base_fee: u64,
    /// Executor remuneration in atoms, `> 0`.
    pub v_exec: u64,
    /// Data-availability remuneration in atoms.
    pub v_da: u64,
    /// Refund share in atoms.
    pub v_ref: u64,
    /// Last height at which the job may be included.
    pub expiry_height: u64,
    /// Client-chosen nonce; makes `tx_id` unique.
    pub nonce: u64,
    /// The computation to run.
    pub payload: JobPayload,
}

impl JobTransaction {
    /// Computes `deposit = base_fee + v_exec + v_da + v_ref` with checked addition.
    pub fn checked_deposit(&self) -> Option<u64> {
        self.base_fee
            .checked_add(self.v_exec)?
            .checked_add(self.v_da)?
            .checked_add(self.v_ref)
    }

    /// Computes total client charge `deposit + queue_fee` with checked addition.
    pub fn checked_total_charge(&self) -> Option<u64> {
        self.checked_deposit()?.checked_add(self.queue_fee)
    }
}

/// Commercial job payload (§5.4).
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum JobPayload {
    /// SciVM Ozaki-II exact dot product of two `DOT_LEN` operand vectors.
    ExactDot {
        /// Left operands (integer-valued).
        xs: Vec<f64>,
        /// Right operands (integer-valued).
        ys: Vec<f64>,
    },
}

/// Fresh Work share proof (§5.5).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareProof {
    /// Full context of the attempt (its own parent, height and body root).
    pub context: FwContext,
    /// Beneficiary; must match context fields 11 and 12.
    pub owner: u64,
    /// Claimed `outputRoot`.
    pub output_root: Digest,
    /// Claimed `WorkID`, which is the ShareID.
    pub work_id: Digest,
    /// Claimed `θ_S`.
    pub share_ticket: Digest,
    /// `Θ_S` at `context.height` on the including chain.
    pub share_target: Target,
}

/// Lease assignment (§5.6).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseAssignment {
    /// Leased job.
    pub job_id: u64,
    /// ShareID that won the slot.
    pub share_id: Digest,
    /// Owner of that ShareID, the executor by attribution.
    pub owner: u64,
}

/// Settlement receipt (§5.6).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementReceipt {
    /// Settled job.
    pub job_id: u64,
    /// Its transaction identifier.
    pub tx_id: Digest,
    /// Owner of the leased ShareID.
    pub executor_owner: u64,
    /// Height of `JOB_ADMIT_AND_LEASE`.
    pub admitted_at: u64,
    /// `TESTNET_PROFILE.exact_dot(xs, ys)`.
    pub result: i128,
    /// `H(RESULT, job_id, result)`.
    pub result_digest: Digest,
    /// Executor remuneration paid to `U`.
    pub v_exec: u64,
    /// Data-availability remuneration paid to `U`.
    pub v_da: u64,
    /// Refund paid to `U`.
    pub v_ref: u64,
}

/// Signed change per category in `ledger::StateCategory::ALL` order (§5.7):
/// `[U, S_z, E_esc, B_bond, V_L2, R_a, R_l, P_pending]`.
// normative
pub type CategoryDelta = [i128; 8];

/// Per-owner reward entry in `EpochBooked` (§5.7).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerReward {
    /// Share owner.
    pub owner: u64,
    /// Atoms booked for that owner this epoch.
    pub atoms: u64,
}

/// Ledger transition record in execution order (§5.7).
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeTransition {
    /// `RESERVE_MATURE`: locked fees whose maturity epoch arrived move `R_l -> R_a`.
    ReserveMature {
        /// Epoch of the boundary block.
        epoch: u64,
        /// Lots created from the matured fees.
        matured_lots: Vec<u64>,
        /// Per-category delta.
        delta: CategoryDelta,
    },
    /// `FW_REWARD`: the previous boundary's booking moves `P_pending -> U`.
    FwReward {
        /// Epoch of the boundary block.
        epoch: u64,
        /// Atoms paid.
        amount: u64,
        /// Per-category delta.
        delta: CategoryDelta,
    },
    /// Epoch budget booked `R_a -> P_pending` against the previous epoch's shares.
    EpochBooked {
        /// Epoch of the boundary block.
        epoch: u64,
        /// `E_e`, the sum of all lot budgets.
        budget: u64,
        /// `P_e`, the booked payouts.
        booked: u64,
        /// `r_e`, newly segregated dust.
        dust: u64,
        /// Lots that contributed.
        lots: u32,
        /// Shares weighed.
        shares: u32,
        /// Booked atoms per owner, sorted by owner.
        per_owner: Vec<OwnerReward>,
        /// Per-category delta.
        delta: CategoryDelta,
    },
    /// `RESULT_SETTLE`: escrow pays `v_exec + v_da + v_ref` to `U`.
    ResultSettle {
        /// Settled job.
        job_id: u64,
        /// Executor remuneration.
        v_exec: u64,
        /// Data-availability remuneration.
        v_da: u64,
        /// Refund.
        v_ref: u64,
        /// Per-category delta.
        delta: CategoryDelta,
    },
    /// `ESCROW_FUND_AND_QUEUE`: `U` funds the escrow and pays the queue fee to `R_l`.
    EscrowFundAndQueue {
        /// Funded job.
        job_id: u64,
        /// Escrow deposit `D`.
        deposit: u64,
        /// Queue fee.
        queue_fee: u64,
        /// Per-category delta.
        delta: CategoryDelta,
    },
    /// `JOB_ADMIT_AND_LEASE`: the Base Fee moves irretrievably `E_esc -> R_l`.
    JobAdmitAndLease {
        /// Admitted job.
        job_id: u64,
        /// Base Fee.
        base_fee: u64,
        /// Lease owner.
        owner: u64,
        /// Per-category delta.
        delta: CategoryDelta,
    },
}

impl FeeTransition {
    /// Returns the per-category delta of this transition.
    pub fn delta(&self) -> &CategoryDelta {
        match self {
            FeeTransition::ReserveMature { delta, .. }
            | FeeTransition::FwReward { delta, .. }
            | FeeTransition::EpochBooked { delta, .. }
            | FeeTransition::ResultSettle { delta, .. }
            | FeeTransition::EscrowFundAndQueue { delta, .. }
            | FeeTransition::JobAdmitAndLease { delta, .. } => delta,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire messages (§4.4)
// ---------------------------------------------------------------------------

/// P2P wire message (§4.4).
// normative — variant order is the bincode discriminant
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireMessage {
    /// First frame on every connection, sent by both sides.
    Handshake {
        /// Must equal `WIRE_VERSION`.
        version: u32,
        /// Must equal the receiver's `genesis.chain_id`.
        chain_id: u64,
        /// Sender's tip height (sync hint only).
        best_height: u64,
        /// Sender's tip `block_id`, or `genesis_id` at height 0.
        best_block_id: Digest,
    },
    /// A block accepted by the sender. Boxed only to keep the enum small in
    /// memory; serde encodes `Box<Block>` exactly like `Block`.
    GossipBlock(Box<Block>),
    /// A job accepted into the sender's mempool.
    GossipJob(JobTransaction),
    /// Request for canonical blocks starting at `from_height`.
    SyncRequest {
        /// First requested height (`>= 1`).
        from_height: u64,
    },
    /// Contiguous canonical blocks answering a `SyncRequest`.
    SyncResponse {
        /// Blocks in ascending height order.
        blocks: Vec<Block>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appendix_b_golden_vectors() {
        let mut seq = [0u8; DIGEST_LEN];
        for (i, slot) in seq.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let hs = WireMessage::Handshake {
            version: 1,
            chain_id: 0x504C_4154,
            best_height: 7,
            best_block_id: Digest(seq),
        };
        let encoded_hs = codec::encode(&hs).unwrap();
        assert_eq!(encoded_hs.len(), 88);
        let expected_hs_hex = concat!(
            "00000000",
            "01000000",
            "54414c5000000000",
            "0700000000000000",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f"
        );
        assert_eq!(to_hex(&encoded_hs), expected_hs_hex);
        let decoded_hs: WireMessage = codec::decode(&encoded_hs).unwrap();
        assert_eq!(decoded_hs, hs);

        let sr = WireMessage::SyncRequest {
            from_height: 0x0102,
        };
        let encoded_sr = codec::encode(&sr).unwrap();
        assert_eq!(encoded_sr.len(), 12);
        assert_eq!(to_hex(&encoded_sr), "030000000201000000000000");
        let decoded_sr: WireMessage = codec::decode(&encoded_sr).unwrap();
        assert_eq!(decoded_sr, sr);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let sr = WireMessage::SyncRequest { from_height: 42 };
        let mut bytes = codec::encode(&sr).unwrap();
        bytes.push(0x00);
        assert!(codec::decode::<WireMessage>(&bytes).is_err());
    }

    #[test]
    fn digest_and_target_round_trip() {
        let d = Digest([0xabu8; DIGEST_LEN]);
        let b = codec::encode(&d).unwrap();
        assert_eq!(b.len(), DIGEST_LEN);
        assert_eq!(codec::decode::<Digest>(&b).unwrap(), d);
        assert_eq!(Digest::from_hex(&d.to_hex()).unwrap(), d);
        assert!(Digest::from_hex("abc").is_err());

        let t = Target(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        let tb = codec::encode(&t).unwrap();
        assert_eq!(tb.len(), 16);
        assert_eq!(codec::decode::<Target>(&tb).unwrap(), t);
        assert_eq!(Target::from_hex(&t.to_hex()).unwrap(), t);
    }
}
