//! Module 1 (SciVM) — the Ozaki-II numerical profile over native INT8 tiles.
//!
//! The Scientific Virtual Machine executes admitted commercial and scientific
//! tasks with **zero consensus weight** (spec §1.1, eq. 3.23). Dense
//! high-precision matrix products are carried out by deterministic Ozaki-II
//! residue splitting (spec §3.6): scaled integer operands are split into
//! centered residues modulo `r` pairwise-coprime auxiliary moduli
//! `2 <= q_j <= 255`, multiplied as exact INT8 x INT8 -> INT32 tiles, reduced
//! modulo each `q_j`, and recombined by centered CRT. The base field `F_p` and
//! Module 3 are unchanged; auxiliary moduli never replace `p`.
//!
//! **PoC status:** the profile, residue map, accumulation and CRT are exact
//! integer code. The STARK `TraceCheck`, `Scale`/`Unscale` of floating-point
//! formats and the remaining `Sem` semantics are out of scope.

use core::fmt;

use work::{ACCUMULATION_BOUND, INT8_MAX, INT8_MIN, TILE_DIM};

/// Smallest admissible auxiliary modulus.
pub const MIN_MODULUS: u32 = 2;
/// Largest admissible auxiliary modulus; centered residues then fit `[-127, 127]`.
pub const MAX_MODULUS: u32 = 255;
/// Contribution of any SciVM task to chain weight, `ΔW_consensus(τ) = 0`.
pub const CONSENSUS_WEIGHT: u64 = 0;
/// Largest magnitude at which every integer is exactly representable in `f64`.
pub const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_992.0; // 2^53
/// PoC limit: centered CRT is evaluated in `i128`, so `Q` must stay below `2^127`.
pub const MAX_RECONSTRUCTION_MODULUS: u128 = 1 << 127;

/// Rounding rule `Rnd` of the target format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RoundingMode {
    /// IEEE 754 round to nearest, ties to even.
    #[default]
    NearestTiesToEven,
    /// Round toward zero.
    TowardZero,
    /// Round toward positive infinity.
    TowardPositive,
    /// Round toward negative infinity.
    TowardNegative,
}

/// The task-bound numerical profile `Π_num` (eq. 3.20), restricted to the
/// fields that govern Ozaki-II residue splitting and exact reconstruction.
///
/// The profile is immutably bound by the client at admission; two executions
/// with the same program, inputs, state and profile produce identical outputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OzakiIIProfile {
    /// Profile version `ν`, which also fixes the Ozaki-II construction.
    pub version: u32,
    /// Auxiliary moduli `q = (q_1, ..., q_r)`: pairwise coprime, each in `[2, 255]`.
    pub moduli: Vec<u32>,
    /// Common inner dimension `K` of the operands.
    pub inner_dim: u64,
    /// Certified bound `B_X >= max |X~|` on the scaled left operand.
    pub bound_x: u64,
    /// Certified bound `B_Y >= max |Y~|` on the scaled right operand.
    pub bound_y: u64,
    /// Rounding rule of the output format.
    pub rounding: RoundingMode,
}

/// Rejection reasons of the SciVM profile and residue pipeline.
#[derive(Clone, Debug, PartialEq)]
pub enum SciVmError {
    /// `r = 0`: at least one auxiliary modulus is required.
    NoModuli,
    /// A modulus lies outside `[2, 255]`.
    ModulusOutOfRange(u32),
    /// Two moduli share a factor.
    ModuliNotCoprime(u32, u32),
    /// `K = 0`.
    ZeroInnerDimension,
    /// An operand is not a finite integer of magnitude at most `2^53`.
    NonIntegralOperand {
        /// Position in the input slice.
        index: usize,
        /// The offending value.
        value: f64,
    },
    /// An operand exceeds its certified bound `B_X` or `B_Y`.
    OperandOutOfBound {
        /// Position in the input slice.
        index: usize,
        /// The offending magnitude.
        magnitude: u64,
        /// The certified bound.
        bound: u64,
    },
    /// `Q <= 2 B_rec`: the moduli cannot reconstruct the product exactly.
    InsufficientReconstructionCapacity {
        /// `Q = prod q_j`.
        modulus: u128,
        /// `2 B_rec`.
        twice_bound: u128,
    },
    /// `Q` or `B_rec` exceeds the PoC's `i128` reconstruction range.
    ReconstructionRangeExceeded,
    /// Operand slices or residue vectors have inconsistent lengths.
    LengthMismatch,
}

impl fmt::Display for SciVmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SciVmError::NoModuli => write!(f, "profile needs at least one modulus"),
            SciVmError::ModulusOutOfRange(q) => {
                write!(f, "modulus {q} outside [{MIN_MODULUS}, {MAX_MODULUS}]")
            }
            SciVmError::ModuliNotCoprime(a, b) => write!(f, "moduli {a} and {b} are not coprime"),
            SciVmError::ZeroInnerDimension => write!(f, "inner dimension K must be positive"),
            SciVmError::NonIntegralOperand { index, value } => {
                write!(f, "operand {index} = {value} is not an exact integer")
            }
            SciVmError::OperandOutOfBound {
                index,
                magnitude,
                bound,
            } => {
                write!(
                    f,
                    "operand {index} has |x| = {magnitude} > certified bound {bound}"
                )
            }
            SciVmError::InsufficientReconstructionCapacity {
                modulus,
                twice_bound,
            } => {
                write!(f, "Q = {modulus} <= 2 B_rec = {twice_bound}")
            }
            SciVmError::ReconstructionRangeExceeded => {
                write!(f, "Q or B_rec exceeds the PoC i128 reconstruction range")
            }
            SciVmError::LengthMismatch => write!(f, "inconsistent operand or residue lengths"),
        }
    }
}

impl std::error::Error for SciVmError {}

impl OzakiIIProfile {
    /// Checks the admission conditions of eq. (3.22): `r >= 1`, every
    /// `2 <= q_j <= 255`, pairwise coprimality, and `Q > 2 B_rec` with
    /// `B_rec = K B_X B_Y`. Insufficient capacity is a rejection, never a
    /// silent approximation.
    pub fn validate(&self) -> Result<(), SciVmError> {
        validate_moduli(&self.moduli)?;
        if self.inner_dim == 0 {
            return Err(SciVmError::ZeroInnerDimension);
        }
        let modulus = self.reconstruction_modulus()?;
        let twice_bound = self
            .reconstruction_bound()?
            .checked_mul(2)
            .ok_or(SciVmError::ReconstructionRangeExceeded)?;
        if modulus <= twice_bound {
            return Err(SciVmError::InsufficientReconstructionCapacity {
                modulus,
                twice_bound,
            });
        }
        Ok(())
    }

    /// `Q = prod q_j`, which must stay below [`MAX_RECONSTRUCTION_MODULUS`] in the PoC.
    pub fn reconstruction_modulus(&self) -> Result<u128, SciVmError> {
        reconstruction_modulus(&self.moduli)
    }

    /// `B_rec = K B_X B_Y`, the bound on every entry of the scaled integer product.
    pub fn reconstruction_bound(&self) -> Result<u128, SciVmError> {
        u128::from(self.inner_dim)
            .checked_mul(u128::from(self.bound_x))
            .and_then(|v| v.checked_mul(u128::from(self.bound_y)))
            .ok_or(SciVmError::ReconstructionRangeExceeded)
    }

    /// Exact dot product `sum_k x_k y_k` of scaled integer operands through the
    /// Ozaki-II path: residue split, INT8 x INT8 -> INT32 accumulation per
    /// 512-wide tile, reduction modulo each `q_j`, then centered CRT.
    ///
    /// This is one entry of the tile product of eq. (3.21); a full matrix
    /// product repeats it for every output entry.
    pub fn exact_dot(&self, x: &[f64], y: &[f64]) -> Result<i128, SciVmError> {
        self.validate()?;
        if x.len() != y.len() || x.len() as u64 != self.inner_dim {
            return Err(SciVmError::LengthMismatch);
        }
        check_bound(x, self.bound_x)?;
        check_bound(y, self.bound_y)?;
        let k = x.len();
        let rx = try_split_residues(x, &self.moduli)?;
        let ry = try_split_residues(y, &self.moduli)?;

        let mut assembled = Vec::with_capacity(self.moduli.len());
        for (j, &q) in self.moduli.iter().enumerate() {
            let (xj, yj) = (&rx[j * k..(j + 1) * k], &ry[j * k..(j + 1) * k]);
            let mut acc_mod_q: i64 = 0;
            for (tile_x, tile_y) in xj.chunks(TILE_DIM).zip(yj.chunks(TILE_DIM)) {
                // One native tile: at most 512 INT8 products, so the INT32
                // accumulator stays within ±8,258,048 (eq. 3.22).
                let tile: i32 = tile_x
                    .iter()
                    .zip(tile_y)
                    .map(|(&a, &b)| i32::from(a) * i32::from(b))
                    .sum();
                debug_assert!(tile.abs() <= ACCUMULATION_BOUND);
                acc_mod_q = (acc_mod_q + i64::from(tile)).rem_euclid(i64::from(q));
            }
            assembled.push(acc_mod_q);
        }
        crt_centered(&assembled, &self.moduli)
    }
}

/// Splits scaled integer operands into centered residues
/// `cres_q(z) = z - q * floor((z + floor(q/2)) / q)` for every modulus.
///
/// The moduli must be **pairwise-coprime integers in `[2, 255]`**, so that each
/// residue lies in `[-127, 127]`: exactly the native INT8 tile domain, with
/// `-128` never produced. Operands must be finite integers with `|z| <= 2^53`
/// (the output of the profile's `Scale` step).
///
/// The output is residue-major: entry `j * n + i` is `cres_{q_j}(z_i)` for
/// `n = scaled_operands.len()`.
///
/// # Panics
///
/// Panics on invalid moduli or operands; use [`try_split_residues`] to handle
/// these as errors.
pub fn split_residues(scaled_operands: &[f64], moduli: &[u32]) -> Vec<i8> {
    try_split_residues(scaled_operands, moduli).unwrap_or_else(|e| panic!("split_residues: {e}"))
}

/// Fallible form of [`split_residues`].
pub fn try_split_residues(scaled_operands: &[f64], moduli: &[u32]) -> Result<Vec<i8>, SciVmError> {
    validate_moduli(moduli)?;
    let integers = scaled_operands
        .iter()
        .enumerate()
        .map(|(index, &value)| {
            exact_integer(value).ok_or(SciVmError::NonIntegralOperand { index, value })
        })
        .collect::<Result<Vec<i64>, _>>()?;
    let mut out = Vec::with_capacity(moduli.len() * integers.len());
    for &q in moduli {
        out.extend(integers.iter().map(|&z| centered_residue(z, q)));
    }
    Ok(out)
}

/// `cres_q(z)` for one value; the result lies in `[-floor(q/2), q - 1 - floor(q/2)]`.
pub fn centered_residue(z: i64, q: u32) -> i8 {
    let q = i64::from(q);
    let residue = z - q * (z + q / 2).div_euclid(q);
    debug_assert!((i64::from(INT8_MIN)..=i64::from(INT8_MAX)).contains(&residue));
    residue as i8
}

/// Centered CRT reconstruction `CRT_0`: the unique `Z` in
/// `[-floor(Q/2), ceil(Q/2) - 1]` with `Z = r_j (mod q_j)` for every `j`.
pub fn crt_centered(residues: &[i64], moduli: &[u32]) -> Result<i128, SciVmError> {
    validate_moduli(moduli)?;
    if residues.len() != moduli.len() {
        return Err(SciVmError::LengthMismatch);
    }
    let modulus = reconstruction_modulus(moduli)?;
    // Garner-style incremental combination; every intermediate is < Q < 2^127.
    let (mut value, mut combined) = (0u128, 1u128);
    for (&r, &q) in residues.iter().zip(moduli) {
        let q128 = u128::from(q);
        let target = r.rem_euclid(i64::from(q)) as u128;
        let current = value % q128;
        let diff = (target + q128 - current) % q128;
        let inverse = mod_inverse((combined % q128) as u32, q) as u128;
        let step = diff * inverse % q128;
        value += combined * step;
        combined *= q128;
    }
    debug_assert_eq!(combined, modulus);
    let half_up = modulus - modulus / 2; // ceil(Q/2)
    Ok(if value >= half_up {
        value as i128 - modulus as i128
    } else {
        value as i128
    })
}

fn validate_moduli(moduli: &[u32]) -> Result<(), SciVmError> {
    if moduli.is_empty() {
        return Err(SciVmError::NoModuli);
    }
    if let Some(&q) = moduli
        .iter()
        .find(|q| !(MIN_MODULUS..=MAX_MODULUS).contains(q))
    {
        return Err(SciVmError::ModulusOutOfRange(q));
    }
    for (i, &a) in moduli.iter().enumerate() {
        if let Some(&b) = moduli[i + 1..].iter().find(|&&b| gcd(a, b) != 1) {
            return Err(SciVmError::ModuliNotCoprime(a, b));
        }
    }
    Ok(())
}

fn reconstruction_modulus(moduli: &[u32]) -> Result<u128, SciVmError> {
    moduli
        .iter()
        .try_fold(1u128, |acc, &q| acc.checked_mul(u128::from(q)))
        .filter(|&m| m < MAX_RECONSTRUCTION_MODULUS)
        .ok_or(SciVmError::ReconstructionRangeExceeded)
}

fn check_bound(values: &[f64], bound: u64) -> Result<(), SciVmError> {
    for (index, &value) in values.iter().enumerate() {
        let magnitude = exact_integer(value)
            .ok_or(SciVmError::NonIntegralOperand { index, value })?
            .unsigned_abs();
        if magnitude > bound {
            return Err(SciVmError::OperandOutOfBound {
                index,
                magnitude,
                bound,
            });
        }
    }
    Ok(())
}

fn exact_integer(value: f64) -> Option<i64> {
    (value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_EXACT_F64_INTEGER)
        .then_some(value as i64)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Inverse of `a` modulo `q`; callers guarantee `gcd(a, q) = 1`.
fn mod_inverse(a: u32, q: u32) -> u32 {
    let (mut old_r, mut r) = (i64::from(a), i64::from(q));
    let (mut old_s, mut s) = (1i64, 0i64);
    while r != 0 {
        let quotient = old_r / r;
        (old_r, r) = (r, old_r - quotient * r);
        (old_s, s) = (s, old_s - quotient * s);
    }
    debug_assert_eq!(old_r, 1, "modulus not coprime");
    old_s.rem_euclid(i64::from(q)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fourteen pairwise-coprime moduli <= 255; Q is about 2^111.
    const MODULI: [u32; 14] = [
        255, 254, 253, 251, 247, 241, 239, 233, 229, 227, 223, 211, 199, 197,
    ];

    fn profile(inner_dim: u64, bound: u64) -> OzakiIIProfile {
        OzakiIIProfile {
            version: 1,
            moduli: MODULI.to_vec(),
            inner_dim,
            bound_x: bound,
            bound_y: bound,
            rounding: RoundingMode::NearestTiesToEven,
        }
    }

    #[test]
    fn residues_stay_in_the_int8_tile_domain() {
        for q in MIN_MODULUS..=MAX_MODULUS {
            for z in -600i64..600 {
                let r = centered_residue(z, q);
                assert!((INT8_MIN..=INT8_MAX).contains(&r), "q={q} z={z} r={r}");
                assert_eq!((z - i64::from(r)).rem_euclid(i64::from(q)), 0);
            }
        }
        assert_eq!(centered_residue(127, 255), 127);
        assert_eq!(centered_residue(128, 255), -127);
        assert_eq!(centered_residue(-1, 2), -1);
    }

    #[test]
    fn split_residues_layout_is_residue_major() {
        let out = split_residues(&[0.0, 300.0, -7.0], &[255, 11]);
        assert_eq!(out, vec![0, 45, -7, 0, 3, 4]);
    }

    #[test]
    fn moduli_must_be_pairwise_coprime_and_in_range() {
        assert_eq!(try_split_residues(&[1.0], &[]), Err(SciVmError::NoModuli));
        assert_eq!(
            try_split_residues(&[1.0], &[6, 9]),
            Err(SciVmError::ModuliNotCoprime(6, 9))
        );
        assert_eq!(
            try_split_residues(&[1.0], &[256]),
            Err(SciVmError::ModulusOutOfRange(256))
        );
        assert_eq!(
            try_split_residues(&[1.0], &[1]),
            Err(SciVmError::ModulusOutOfRange(1))
        );
        assert!(try_split_residues(&[1.0], &MODULI).is_ok());
    }

    #[test]
    #[should_panic(expected = "not coprime")]
    fn split_residues_panics_on_shared_factor() {
        split_residues(&[1.0], &[10, 15]);
    }

    #[test]
    fn operands_must_be_exact_integers() {
        for bad in [0.5, f64::NAN, f64::INFINITY, 2.0 * MAX_EXACT_F64_INTEGER] {
            assert!(matches!(
                try_split_residues(&[1.0, bad], &[255]),
                Err(SciVmError::NonIntegralOperand { index: 1, .. })
            ));
        }
    }

    #[test]
    fn crt_round_trips_every_split() {
        let values = [
            0i64,
            1,
            -1,
            123_456_789,
            -987_654_321_012,
            9_007_199_254_740_992,
            -9_007_199_254_740_992,
        ];
        let operands: Vec<f64> = values.iter().map(|&v| v as f64).collect();
        let residues = split_residues(&operands, &MODULI);
        for (i, &expected) in values.iter().enumerate() {
            let column: Vec<i64> = (0..MODULI.len())
                .map(|j| i64::from(residues[j * values.len() + i]))
                .collect();
            assert_eq!(
                crt_centered(&column, &MODULI).unwrap(),
                i128::from(expected)
            );
        }
    }

    #[test]
    fn profile_rejects_insufficient_reconstruction_capacity() {
        assert!(profile(1_000, 1 << 40).validate().is_ok());
        let small = OzakiIIProfile {
            moduli: vec![255, 254],
            ..profile(1_000, 1 << 40)
        };
        assert!(matches!(
            small.validate(),
            Err(SciVmError::InsufficientReconstructionCapacity { .. })
        ));
        assert_eq!(
            profile(0, 1).validate(),
            Err(SciVmError::ZeroInnerDimension)
        );
        assert_eq!(CONSENSUS_WEIGHT, 0);
    }

    #[test]
    fn exact_dot_product_through_residues_matches_integer_product() {
        // K = 1,000 spans two native tiles (512 + 488); |x|, |y| < 2^40.
        let k = 1_000usize;
        let x: Vec<f64> = (0..k)
            .map(|i| ((i as i64 * 7_919 - 3_000_000) * 131_071) as f64)
            .collect();
        let y: Vec<f64> = (0..k)
            .map(|i| ((5_000_000 - i as i64 * 104_729) * 8_191) as f64)
            .collect();
        let expected: i128 = x.iter().zip(&y).map(|(&a, &b)| a as i128 * b as i128).sum();
        let p = profile(k as u64, 1 << 40);
        assert_eq!(p.exact_dot(&x, &y).unwrap(), expected);
        assert!(matches!(
            p.exact_dot(&x[..10], &y[..10]),
            Err(SciVmError::LengthMismatch)
        ));
        let mut too_big = x.clone();
        too_big[3] = (1u64 << 41) as f64;
        assert!(matches!(
            p.exact_dot(&too_big, &y),
            Err(SciVmError::OperandOutOfBound { index: 3, .. })
        ));
    }
}
