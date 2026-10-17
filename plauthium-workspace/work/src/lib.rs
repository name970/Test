//! Module 1 — the canonical Fresh Work tile and its Freivalds verification.
//!
//! The native work unit is the exact integer product `Z = XY` of two
//! `512 x 512` INT8 matrices with entries in `{-127, ..., 127}` (spec §3.1).
//! Every entry of `Z` satisfies `|Z_ij| <= 512 * 127^2 = 8,258,048 < 2^23`
//! (eq. 3.2), so two admissible results that agree modulo `p = 2^61 - 1` are
//! equal over the integers (eq. 3.3). Correctness is checked by `r_F = 6`
//! Freivalds rounds over `F_p` (§3.2), whose static error is at most `p^-6`.

use core::fmt;

use primitives::{DomainHasher, DomainSeparatedHash, Mersenne61, MockXof, labels};

/// Tile dimension `n = 512`.
pub const TILE_DIM: usize = 512;
/// Entries per tile, `n^2`.
pub const TILE_ELEMS: usize = TILE_DIM * TILE_DIM;
/// Smallest admissible INT8 input. `-128` is excluded: `512 * 128^2 = 2^23`
/// would break the strict bound of eq. (3.2).
pub const INT8_MIN: i8 = -127;
/// Largest admissible INT8 input.
pub const INT8_MAX: i8 = 127;
/// Maximum absolute value of an accumulated INT32 entry, `512 * 127^2`.
pub const ACCUMULATION_BOUND: i32 = 8_258_048;

/// Normative number of Freivalds rounds, `r_F = 6`.
pub const DEFAULT_FREIVALDS_ROUNDS: usize = 6;
/// Classical multiply-accumulate count of one tile, `n^3 = 2^27` (eq. 3.4).
pub const CLASSICAL_MACS: u64 = (TILE_DIM as u64) * (TILE_DIM as u64) * (TILE_DIM as u64);
/// Field multiplications of six Freivalds rounds, `3 r_F n^2 = 4,718,592` (eq. 3.8).
pub const FREIVALDS_FIELD_MULS: u64 = 3 * DEFAULT_FREIVALDS_ROUNDS as u64 * TILE_ELEMS as u64;

const _: () = assert!(TILE_DIM as i64 * 127 * 127 == ACCUMULATION_BOUND as i64);
const _: () = assert!((ACCUMULATION_BOUND as i64) < (1 << 23));
const _: () = assert!(TILE_DIM as i64 * 128 * 128 == 1 << 23);
const _: () = assert!(CLASSICAL_MACS == 1 << 27);
const _: () = assert!(FREIVALDS_FIELD_MULS * 256 == 9 * CLASSICAL_MACS);

/// Matrix entry types admitted by the tile domain.
pub trait TileElement: Copy + Default + Into<i64> {
    /// Largest admissible absolute value.
    const MAX_ABS: i64;
}

impl TileElement for i8 {
    const MAX_ABS: i64 = INT8_MAX as i64;
}

impl TileElement for i32 {
    const MAX_ABS: i64 = ACCUMULATION_BOUND as i64;
}

/// A `512 x 512` row-major matrix whose entries are always inside the tile domain.
#[derive(Clone, PartialEq, Eq)]
pub struct Matrix<T> {
    data: Box<[T]>,
}

/// The canonical Work Unit: a `512 x 512` matrix over `{-127, ..., 127}`.
pub type WorkUnit = Matrix<i8>;
/// The accumulation target: a `512 x 512` INT32 matrix with `|z| <= 8,258,048`.
pub type AccumulationTarget = Matrix<i32>;

/// Rejection reasons for matrices outside the tile domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkError {
    /// The entry count is not `512 * 512`.
    Dimension {
        /// Required entry count.
        expected: usize,
        /// Supplied entry count.
        found: usize,
    },
    /// An entry lies outside the admissible range.
    OutOfDomain {
        /// Row of the offending entry.
        row: usize,
        /// Column of the offending entry.
        col: usize,
        /// The offending value.
        value: i64,
        /// The admissible absolute bound.
        max_abs: i64,
    },
}

impl fmt::Display for WorkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkError::Dimension { expected, found } => {
                write!(f, "tile needs {expected} entries, got {found}")
            }
            WorkError::OutOfDomain {
                row,
                col,
                value,
                max_abs,
            } => write!(
                f,
                "entry ({row}, {col}) = {value} is outside [-{max_abs}, {max_abs}]"
            ),
        }
    }
}

impl std::error::Error for WorkError {}

impl<T: TileElement> Matrix<T> {
    /// Builds a tile from row-major entries, rejecting wrong sizes and out-of-domain values.
    pub fn from_vec(data: Vec<T>) -> Result<Self, WorkError> {
        if data.len() != TILE_ELEMS {
            return Err(WorkError::Dimension {
                expected: TILE_ELEMS,
                found: data.len(),
            });
        }
        if let Some(index) = data.iter().position(|&v| !in_domain(v)) {
            return Err(out_of_domain(index, data[index]));
        }
        Ok(Matrix {
            data: data.into_boxed_slice(),
        })
    }

    /// The all-zero tile.
    pub fn zeros() -> Self {
        Matrix {
            data: vec![T::default(); TILE_ELEMS].into_boxed_slice(),
        }
    }

    /// Entry at `(row, col)`. Panics if either index is `>= 512`.
    pub fn get(&self, row: usize, col: usize) -> T {
        self.data[index(row, col)]
    }

    /// Replaces the entry at `(row, col)` if the new value is in the domain.
    pub fn set(&mut self, row: usize, col: usize, value: T) -> Result<(), WorkError> {
        let i = index(row, col);
        if !in_domain(value) {
            return Err(out_of_domain(i, value));
        }
        self.data[i] = value;
        Ok(())
    }

    /// Row `row` as a slice.
    pub fn row(&self, row: usize) -> &[T] {
        &self.data[row * TILE_DIM..(row + 1) * TILE_DIM]
    }

    /// All entries, row-major.
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// Largest absolute entry.
    pub fn max_abs(&self) -> i64 {
        self.data.iter().map(|&v| v.into().abs()).max().unwrap_or(0)
    }
}

impl Matrix<i8> {
    /// The identity tile.
    pub fn identity() -> Self {
        let mut m = Self::zeros();
        for i in 0..TILE_DIM {
            m.data[index(i, i)] = 1;
        }
        m
    }

    /// The exact INT32 product `self * rhs` (`n^3` multiply-accumulates).
    ///
    /// No partial sum can leave `[-8,258,048, 8,258,048]`, so the result is in
    /// the accumulation domain by construction.
    pub fn product(&self, rhs: &Matrix<i8>) -> Matrix<i32> {
        let mut out = vec![0i32; TILE_ELEMS];
        for (out_row, lhs_row) in out
            .chunks_exact_mut(TILE_DIM)
            .zip(self.data.chunks_exact(TILE_DIM))
        {
            for (&a, rhs_row) in lhs_row.iter().zip(rhs.data.chunks_exact(TILE_DIM)) {
                if a == 0 {
                    continue;
                }
                let a = i32::from(a);
                for (o, &b) in out_row.iter_mut().zip(rhs_row) {
                    *o += a * i32::from(b);
                }
            }
        }
        Matrix {
            data: out.into_boxed_slice(),
        }
    }
}

impl<T: TileElement + fmt::Debug> fmt::Debug for Matrix<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Matrix<{}>[{TILE_DIM}x{TILE_DIM}; row0[..4]={:?}]",
            core::any::type_name::<T>(),
            &self.data[..4]
        )
    }
}

fn in_domain<T: TileElement>(value: T) -> bool {
    value.into().abs() <= T::MAX_ABS
}

fn out_of_domain<T: TileElement>(index: usize, value: T) -> WorkError {
    WorkError::OutOfDomain {
        row: index / TILE_DIM,
        col: index % TILE_DIM,
        value: value.into(),
        max_abs: T::MAX_ABS,
    }
}

fn index(row: usize, col: usize) -> usize {
    assert!(row < TILE_DIM && col < TILE_DIM, "tile index out of range");
    row * TILE_DIM + col
}

// ---------------------------------------------------------------------------
// Fresh Work context, seed and instance generation (spec §5.1–§5.2)
// ---------------------------------------------------------------------------

/// Number of fields bound into the Fresh Work context `x` (eq. 5.1).
pub const FRESH_WORK_CONTEXT_FIELDS: usize = 16;

/// The sixteen-field Fresh Work context `x`, bound before any computation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshWorkContext {
    /// Chain domain.
    pub chain_id: u64,
    /// Genesis domain.
    pub genesis_id: DomainSeparatedHash,
    /// Protocol version.
    pub protocol_version: u32,
    /// Active cryptographic suite.
    pub crypto_suite_id: u32,
    /// Fresh Work program.
    pub program_id: DomainSeparatedHash,
    /// AIR of the Fresh Work proof.
    pub air_id: DomainSeparatedHash,
    /// Instance generator version.
    pub generator_version: u32,
    /// Parent block.
    pub parent_canonical_id: DomainSeparatedHash,
    /// Stable anchor.
    pub stable_anchor_id: DomainSeparatedHash,
    /// Height of the attempted block.
    pub height: u64,
    /// Miner policy, fixing the beneficiary before work.
    pub miner_policy_digest: DomainSeparatedHash,
    /// Payout policy.
    pub payout_policy_digest: DomainSeparatedHash,
    /// Attempt nonce.
    pub attempt_nonce: u64,
    /// Body root fixed before the attempt.
    pub body_core_root: DomainSeparatedHash,
    /// Difficulty context derived from the parent.
    pub parent_derived_difficulty_context: DomainSeparatedHash,
    /// Time bucket.
    pub time_bucket: u64,
}

impl FreshWorkContext {
    /// The injective encoding `Enc(x)` of all sixteen fields in normative order.
    pub fn encode(&self) -> Vec<u8> {
        let fields: [&[u8]; FRESH_WORK_CONTEXT_FIELDS] = [
            &self.chain_id.to_be_bytes(),
            self.genesis_id.as_bytes(),
            &self.protocol_version.to_be_bytes(),
            &self.crypto_suite_id.to_be_bytes(),
            self.program_id.as_bytes(),
            self.air_id.as_bytes(),
            &self.generator_version.to_be_bytes(),
            self.parent_canonical_id.as_bytes(),
            self.stable_anchor_id.as_bytes(),
            &self.height.to_be_bytes(),
            self.miner_policy_digest.as_bytes(),
            self.payout_policy_digest.as_bytes(),
            &self.attempt_nonce.to_be_bytes(),
            self.body_core_root.as_bytes(),
            self.parent_derived_difficulty_context.as_bytes(),
            &self.time_bucket.to_be_bytes(),
        ];
        let mut out = Vec::new();
        for field in fields {
            out.extend_from_slice(&(field.len() as u64).to_be_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    /// `seed = H_512(Enc(FW/INPUT/v2, x))` (eq. 5.2).
    pub fn seed(&self) -> DomainSeparatedHash {
        DomainSeparatedHash::new(labels::FW_INPUT, &[&self.encode()])
    }

    /// Expands the seed into the two operand tiles `X` and `Y` (eq. 5.3).
    pub fn instance(&self) -> FreshWorkInstance {
        let seed = self.seed();
        FreshWorkInstance {
            x: expand_seed(labels::FW_A, &seed),
            y: expand_seed(labels::FW_B, &seed),
            seed,
        }
    }
}

/// The operands generated from one Fresh Work seed.
#[derive(Clone, Debug)]
pub struct FreshWorkInstance {
    /// The seed both operands were expanded from.
    pub seed: DomainSeparatedHash,
    /// Left operand `X`.
    pub x: WorkUnit,
    /// Right operand `Y`.
    pub y: WorkUnit,
}

/// Expands a seed into a uniform INT8 tile by byte rejection (eq. 5.3):
/// `Map(b) = b - 127` for `b` in `{0, ..., 254}`, and `b = 255` is rejected,
/// so entries are exactly uniform on `[-127, 127]`.
///
/// The byte stream is the [`MockXof`] placeholder for SHAKE256.
pub fn expand_seed(label: &str, seed: &DomainSeparatedHash) -> WorkUnit {
    let mut xof = MockXof::new(label, seed);
    let mut data = Vec::with_capacity(TILE_ELEMS);
    while data.len() < TILE_ELEMS {
        let byte = xof.next_byte();
        if byte != u8::MAX {
            data.push((i16::from(byte) - 127) as i8);
        }
    }
    Matrix {
        data: data.into_boxed_slice(),
    }
}

/// Identifiers and lottery values of one evaluated attempt (eqs. 5.4–5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreshWorkIds {
    /// `outputRoot = H_512(Enc(FW/OUTPUT/v2, XY))`.
    pub output_root: DomainSeparatedHash,
    /// `WorkID = H_512(Enc(FW/ID/v2, x, outputRoot))`; also the ShareID.
    pub work_id: DomainSeparatedHash,
    /// `BlockID = H_512(Enc(BLOCK/v2, x, outputRoot))`.
    pub block_id: DomainSeparatedHash,
    /// Share lottery value `θ_S = H_512(Enc(FW/SHARE/v2, WorkID))`.
    pub share_ticket: DomainSeparatedHash,
    /// Seal lottery value `θ_B = H_512(Enc(FW/SEAL/v2, WorkID))`.
    pub seal_ticket: DomainSeparatedHash,
}

impl FreshWorkIds {
    /// Derives all identifiers from the context and the committed product.
    /// No witness material enters these maps (Theorem 5.1).
    pub fn derive(context: &FreshWorkContext, product: &AccumulationTarget) -> Self {
        let x = context.encode();
        let output_root = {
            let mut h = DomainHasher::new(labels::FW_OUTPUT);
            h.field_i32s(product.as_slice());
            h.finalize()
        };
        let work_id = DomainSeparatedHash::new(labels::FW_ID, &[&x, output_root.as_bytes()]);
        let block_id = DomainSeparatedHash::new(labels::BLOCK, &[&x, output_root.as_bytes()]);
        FreshWorkIds {
            output_root,
            work_id,
            block_id,
            share_ticket: DomainSeparatedHash::new(labels::FW_SHARE, &[work_id.as_bytes()]),
            seal_ticket: DomainSeparatedHash::new(labels::FW_SEAL, &[work_id.as_bytes()]),
        }
    }
}

// ---------------------------------------------------------------------------
// Freivalds verification over F_p (spec §3.2)
// ---------------------------------------------------------------------------

/// Checks `A(B v) = C v` over `F_p` for `rounds` independent challenge vectors.
///
/// The vectors are drawn only after `A`, `B` and `C` are bound into a
/// Fiat–Shamir transcript, with one domain per round and rejection sampling
/// to uniform `F_p` elements. Each round costs `3 n^2` field multiplications;
/// the normative six rounds cost `4,718,592 = (9/256) n^3`.
///
/// Returns `false` for `rounds == 0`. Consensus uses [`DEFAULT_FREIVALDS_ROUNDS`].
/// The matrix types already enforce the tile domains, which is what makes
/// agreement modulo `p` imply equality over the integers (eq. 3.3).
///
/// **PoC status:** the transcript hash and XOF are placeholders (see
/// [`primitives::DomainSeparatedHash`]); the field arithmetic is exact.
pub fn verify_freivalds_rounds(
    a: &Matrix<i8>,
    b: &Matrix<i8>,
    c: &Matrix<i32>,
    rounds: usize,
) -> bool {
    if rounds == 0 {
        return false;
    }
    let transcript = {
        let mut h = DomainHasher::new(labels::FW_FREIVALDS);
        h.field_i8s(a.as_slice())
            .field_i8s(b.as_slice())
            .field_i32s(c.as_slice());
        h.finalize()
    };
    (0..rounds as u64).all(|round| {
        let v = challenge_vector(&transcript, round);
        let bv = mat_vec(b, &v);
        mat_vec(a, &bv) == mat_vec(c, &v)
    })
}

/// Uniform challenge `v^(round)` in `F_p^n`: 61-bit draws, rejecting the single
/// value `p` so every element of `[0, p)` is equally likely.
fn challenge_vector(transcript: &DomainSeparatedHash, round: u64) -> Vec<Mersenne61> {
    let mut round_seed = DomainHasher::new(labels::FW_FREIVALDS);
    round_seed.field_digest(transcript).field_u64(round);
    let mut xof = MockXof::new(labels::FW_FREIVALDS, &round_seed.finalize());
    let mut v = Vec::with_capacity(TILE_DIM);
    while v.len() < TILE_DIM {
        let candidate = xof.next_u64() & Mersenne61::P;
        if candidate != Mersenne61::P {
            v.push(Mersenne61::new(candidate));
        }
    }
    v
}

fn mat_vec<T: TileElement>(m: &Matrix<T>, v: &[Mersenne61]) -> Vec<Mersenne61> {
    m.data
        .chunks_exact(TILE_DIM)
        .map(|row| signed_dot(row, v))
        .collect()
}

/// `sum_k row[k] * v[k]` over `F_p`. Positive and negative terms accumulate
/// exactly in `u128`: `|row[k]| <= 2^23` and `v[k] < 2^61`, so each sum stays
/// below `512 * 2^84 = 2^93`.
fn signed_dot<T: TileElement>(row: &[T], v: &[Mersenne61]) -> Mersenne61 {
    let (mut positive, mut negative) = (0u128, 0u128);
    for (&x, y) in row.iter().zip(v) {
        let x: i64 = x.into();
        let term = u128::from(x.unsigned_abs()) * u128::from(y.value());
        if x >= 0 {
            positive += term;
        } else {
            negative += term;
        }
    }
    Mersenne61::new(Mersenne61::reduce_u128(positive))
        - Mersenne61::new(Mersenne61::reduce_u128(negative))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(nonce: u64) -> FreshWorkContext {
        let d = |tag: &[u8]| DomainSeparatedHash::new("TEST/CTX/v2", &[tag]);
        FreshWorkContext {
            chain_id: 1,
            genesis_id: d(b"genesis"),
            protocol_version: 2,
            crypto_suite_id: 1,
            program_id: d(b"program"),
            air_id: d(b"air"),
            generator_version: 2,
            parent_canonical_id: d(b"parent"),
            stable_anchor_id: d(b"anchor"),
            height: 1,
            miner_policy_digest: d(b"miner"),
            payout_policy_digest: d(b"payout"),
            attempt_nonce: nonce,
            body_core_root: d(b"body"),
            parent_derived_difficulty_context: d(b"difficulty"),
            time_bucket: 0,
        }
    }

    #[test]
    fn accumulation_bound_is_exact_and_strict() {
        let n: i64 = TILE_DIM as i64;
        let bound = n * 127 * 127;
        assert!(bound <= ACCUMULATION_BOUND as i64);
        assert_eq!(bound, 8_258_048);
        assert!(bound < 1 << 23);
        // Admitting -128 would reach 2^23 exactly and break strictness.
        assert_eq!(n * 128 * 128, 1 << 23);
        // Any two admissible results differ by < 2^24 < p (eq. 3.3).
        assert!(2 * bound < 1 << 24);
        assert!((1i64 << 24) < Mersenne61::P as i64);
    }

    #[test]
    fn freivalds_cost_is_nine_256ths_of_classical() {
        assert_eq!(FREIVALDS_FIELD_MULS, 4_718_592);
        assert_eq!(CLASSICAL_MACS, 134_217_728);
        assert_eq!(FREIVALDS_FIELD_MULS * 256, 9 * CLASSICAL_MACS);
    }

    #[test]
    fn work_unit_domain_excludes_minus_128() {
        let mut entries = vec![0i8; TILE_ELEMS];
        entries[7] = INT8_MIN;
        entries[8] = INT8_MAX;
        assert!(WorkUnit::from_vec(entries.clone()).is_ok());
        entries[9] = i8::MIN;
        assert_eq!(
            WorkUnit::from_vec(entries).unwrap_err(),
            WorkError::OutOfDomain {
                row: 0,
                col: 9,
                value: -128,
                max_abs: 127
            }
        );
        assert!(matches!(
            WorkUnit::from_vec(vec![0; 10]),
            Err(WorkError::Dimension { found: 10, .. })
        ));
        let mut m = WorkUnit::zeros();
        assert!(m.set(511, 511, i8::MIN).is_err());
        assert_eq!(m.get(511, 511), 0);
    }

    #[test]
    fn accumulation_target_domain_is_bounded() {
        let mut target = AccumulationTarget::zeros();
        assert!(target.set(0, 0, ACCUMULATION_BOUND).is_ok());
        assert!(target.set(0, 1, -ACCUMULATION_BOUND).is_ok());
        assert!(target.set(0, 2, ACCUMULATION_BOUND + 1).is_err());
        assert!(target.set(0, 3, -ACCUMULATION_BOUND - 1).is_err());
        assert!(AccumulationTarget::from_vec(vec![1 << 23; TILE_ELEMS]).is_err());
    }

    #[test]
    fn seed_expansion_is_deterministic_and_in_domain() {
        let ctx = context(0);
        let inst = ctx.instance();
        assert_eq!(inst.x, expand_seed(labels::FW_A, &ctx.seed()));
        assert_ne!(inst.x, inst.y, "FW/A and FW/B are separate domains");
        assert_ne!(
            inst.x,
            context(1).instance().x,
            "the nonce changes the seed"
        );
        assert!(
            inst.x
                .as_slice()
                .iter()
                .all(|&v| (INT8_MIN..=INT8_MAX).contains(&v))
        );
        // 262,144 uniform draws hit both ends of the range.
        assert!(inst.x.as_slice().contains(&INT8_MIN));
        assert!(inst.x.as_slice().contains(&INT8_MAX));
    }

    #[test]
    fn context_binds_sixteen_fields() {
        let ctx = context(0);
        assert_eq!(
            ctx.encode().len(),
            FRESH_WORK_CONTEXT_FIELDS * 8 + 9 * 64 + 4 * 8 + 3 * 4
        );
        let mut moved = ctx.clone();
        moved.time_bucket = 1;
        assert_ne!(ctx.seed(), moved.seed());
    }

    #[test]
    fn freivalds_accepts_true_product_and_rejects_tampering() {
        let inst = context(0).instance();
        let z = inst.x.product(&inst.y);
        assert!(z.max_abs() <= ACCUMULATION_BOUND as i64);
        assert!(verify_freivalds_rounds(
            &inst.x,
            &inst.y,
            &z,
            DEFAULT_FREIVALDS_ROUNDS
        ));

        // A single entry off by one is caught.
        let mut tampered = z.clone();
        let v = tampered.get(300, 17);
        tampered
            .set(300, 17, if v > 0 { v - 1 } else { v + 1 })
            .unwrap();
        assert!(!verify_freivalds_rounds(
            &inst.x,
            &inst.y,
            &tampered,
            DEFAULT_FREIVALDS_ROUNDS
        ));

        // Operand order matters: YX is not XY.
        assert!(!verify_freivalds_rounds(
            &inst.y,
            &inst.x,
            &z,
            DEFAULT_FREIVALDS_ROUNDS
        ));
        assert!(!verify_freivalds_rounds(&inst.x, &inst.y, &z, 0));

        let ids = FreshWorkIds::derive(&context(0), &z);
        assert_eq!(ids, FreshWorkIds::derive(&context(0), &z));
        assert_ne!(ids.work_id, ids.block_id);
        assert_ne!(ids.share_ticket, ids.seal_ticket);
        assert_ne!(
            ids.output_root,
            FreshWorkIds::derive(&context(0), &tampered).output_root
        );
    }

    #[test]
    fn freivalds_accepts_extreme_admissible_product() {
        // X = -127 everywhere, Y = 127 everywhere: every entry is exactly -8,258,048.
        let x = WorkUnit::from_vec(vec![INT8_MIN; TILE_ELEMS]).unwrap();
        let y = WorkUnit::from_vec(vec![INT8_MAX; TILE_ELEMS]).unwrap();
        let z = AccumulationTarget::from_vec(vec![-ACCUMULATION_BOUND; TILE_ELEMS]).unwrap();
        assert!(verify_freivalds_rounds(
            &x,
            &y,
            &z,
            DEFAULT_FREIVALDS_ROUNDS
        ));
        let id = WorkUnit::identity();
        assert!(verify_freivalds_rounds(
            &id,
            &y,
            &id.product(&y),
            DEFAULT_FREIVALDS_ROUNDS
        ));
    }
}
