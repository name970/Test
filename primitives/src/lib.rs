//! Shared Plauthium primitives.
//!
//! Every consensus quantity is an integer (spec §1.4): amounts are [`Atoms`],
//! field arithmetic is over [`Mersenne61`], and every committed object is a
//! 512-bit [`DomainSeparatedHash`] of a typed, length-prefixed encoding.
//!
//! Hashing is FIPS 202: [`DomainSeparatedHash`] is SHA3-512 and [`Shake256Xof`]
//! is SHAKE256, both over the same injective `Enc(label, f_1, ..., f_k)`.

use core::fmt;
use core::ops::{Add, AddAssign, Mul, Neg, Sub};

use sha3::Sha3_512;
use sha3::digest::{ExtendableOutput, FixedOutput, Update, XofReader};
use shake::{Shake256, Shake256Reader};

// ---------------------------------------------------------------------------
// Amounts and supply (spec §7.6, eq. 7.18)
// ---------------------------------------------------------------------------

/// Number of atoms in one PLAU (`1 TKN = 10^8 atoms`).
pub const ATOMS_PER_PLAU: u64 = 100_000_000;

/// Fixed supply in whole PLAU.
pub const TOTAL_SUPPLY_PLAU: u64 = 15_000_000;

/// Fixed supply `M = 1.5 * 10^15` atoms. It never changes in any reachable state.
pub const TOTAL_SUPPLY: Atoms = Atoms(1_500_000_000_000_000);

// `M = 15,000,000 PLAU` and `M < 2^51 < p`; evaluated at compile time, so an
// overflow or a violated bound fails the build.
const _: () = assert!(TOTAL_SUPPLY_PLAU * ATOMS_PER_PLAU == TOTAL_SUPPLY.0);
const _: () = assert!(TOTAL_SUPPLY.0 < (1u64 << 51));
const _: () = assert!((1u64 << 51) < Mersenne61::P);

/// An integer amount of the single native asset, in atoms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Atoms(pub u64);

impl Atoms {
    /// Zero atoms.
    pub const ZERO: Atoms = Atoms(0);

    /// Converts whole PLAU to atoms, or `None` on overflow.
    pub const fn from_plau(plau: u64) -> Option<Atoms> {
        match plau.checked_mul(ATOMS_PER_PLAU) {
            Some(atoms) => Some(Atoms(atoms)),
            None => None,
        }
    }

    /// The raw atom count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Checked addition.
    pub const fn checked_add(self, rhs: Atoms) -> Option<Atoms> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Atoms(v)),
            None => None,
        }
    }

    /// Checked subtraction.
    pub const fn checked_sub(self, rhs: Atoms) -> Option<Atoms> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Atoms(v)),
            None => None,
        }
    }
}

impl fmt::Display for Atoms {
    /// Formats as PLAU with all eight decimal places, e.g. `10.00000000 PLAU`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{:08} PLAU",
            self.0 / ATOMS_PER_PLAU,
            self.0 % ATOMS_PER_PLAU
        )
    }
}

/// Integer Fresh Work share weight `w_s` (spec §6.1).
///
/// **PoC width:** normatively `w_s = floor(2^640 / Θ_S(s))`, an integer of up
/// to 640 bits. The PoC narrows it to `u64` so that every product with a slot
/// counter (D'Hondt) or an atom budget (fee-lot payouts) is exact in `u128`.
pub type ShareWeight = u64;

// ---------------------------------------------------------------------------
// Base field F_p, p = 2^61 - 1 (spec §3.1, Table 1)
// ---------------------------------------------------------------------------

/// An element of the base field `F_p`, `p = 2^61 - 1`, kept canonical in `[0, p)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mersenne61(u64);

impl Mersenne61 {
    /// The Mersenne prime `p = 2^61 - 1 = 2,305,843,009,213,693,951`.
    pub const P: u64 = (1u64 << 61) - 1;
    /// Additive identity.
    pub const ZERO: Mersenne61 = Mersenne61(0);
    /// Multiplicative identity.
    pub const ONE: Mersenne61 = Mersenne61(1);

    /// Reduces any `u64` into the field.
    pub const fn new(value: u64) -> Self {
        Mersenne61(Self::reduce_u128(value as u128))
    }

    /// Centered lift of a signed integer: negative values map to `p - |v| mod p`.
    pub const fn from_i64(value: i64) -> Self {
        let magnitude = Self::new(value.unsigned_abs());
        if value < 0 {
            magnitude.neg_const()
        } else {
            magnitude
        }
    }

    /// The canonical representative in `[0, p)`.
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Reduces any `u128` modulo `p` using `2^61 = 1 (mod p)`.
    pub const fn reduce_u128(x: u128) -> u64 {
        let p = Self::P as u128;
        // x < 2^128  =>  folded < 2^61 + 2^67  =>  refolded < 2^61 + 2^7 < 2p.
        let folded = (x & p) + (x >> 61);
        let refolded = ((folded & p) + (folded >> 61)) as u64;
        if refolded >= Self::P {
            refolded - Self::P
        } else {
            refolded
        }
    }

    const fn neg_const(self) -> Self {
        if self.0 == 0 {
            self
        } else {
            Mersenne61(Self::P - self.0)
        }
    }
}

impl Add for Mersenne61 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        // Both operands < 2^61, so the sum < 2^62 cannot overflow.
        Mersenne61::new(self.0 + rhs.0)
    }
}

impl AddAssign for Mersenne61 {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Mersenne61 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self + (-rhs)
    }
}

impl Neg for Mersenne61 {
    type Output = Self;
    fn neg(self) -> Self {
        self.neg_const()
    }
}

impl Mul for Mersenne61 {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Mersenne61(Self::reduce_u128(self.0 as u128 * rhs.0 as u128))
    }
}

// ---------------------------------------------------------------------------
// Domain-separated hashing (spec §1.4, eq. 1.3): SHA3-512 and SHAKE256 (FIPS 202)
// ---------------------------------------------------------------------------

/// Digest length of SHA3-512 in bytes.
pub const DIGEST_LEN: usize = 64;

/// SHAKE256 rate in bytes: one Keccak-f[1600] permutation squeezes this many.
const SHAKE256_RATE: usize = 136;

/// Bytes staged per sponge update when encoding integer slices.
const STAGING_BYTES: usize = 4096;

/// Versioned domain labels fixed by the specification.
pub mod labels {
    /// Fresh Work seed, eq. (5.2).
    pub const FW_INPUT: &str = "FW/INPUT/v2";
    /// Expansion domain of the left operand `X`, eq. (5.3).
    pub const FW_A: &str = "FW/A";
    /// Expansion domain of the right operand `Y`, eq. (5.3).
    pub const FW_B: &str = "FW/B";
    /// Commitment to the canonical product `XY`, eq. (5.4).
    pub const FW_OUTPUT: &str = "FW/OUTPUT/v2";
    /// Work identifier, which is also the ShareID, eq. (5.4).
    pub const FW_ID: &str = "FW/ID/v2";
    /// Identifier of the block an attempt would seal, eq. (5.4).
    pub const BLOCK: &str = "BLOCK/v2";
    /// Share lottery value, eq. (5.5).
    pub const FW_SHARE: &str = "FW/SHARE/v2";
    /// Seal lottery value, eq. (5.5).
    pub const FW_SEAL: &str = "FW/SEAL/v2";
    /// Transaction effect identifier, eq. (7.5).
    pub const TX_EFFECTS: &str = "TX/EFFECTS/v2";
    /// Signature context string, eqs. (2.3)–(2.5).
    pub const PLAU_TX: &str = "PLAU/TX/v2";
    /// Freivalds challenge domain. The spec requires one domain per round
    /// (§3.2) but does not name it; this label is assigned by the PoC.
    pub const FW_FREIVALDS: &str = "FW/FREIVALDS/v2";
}

/// A 512-bit digest `H_512(Enc(LABEL/v2, f_1, ..., f_k))`, where `H_512` is
/// SHA3-512 (FIPS 202).
///
/// Ordering is lexicographic over the 64 bytes, which equals the numeric order
/// of the digest read as a big-endian integer (spec §1.4) and the canonical
/// 64-byte-string order used for ShareID tie-breaks (spec §6.3).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainSeparatedHash([u8; DIGEST_LEN]);

impl DomainSeparatedHash {
    /// Hashes `fields` under `label` with the injective encoding of [`DomainHasher`].
    pub fn new(label: &str, fields: &[&[u8]]) -> Self {
        let mut hasher = DomainHasher::new(label);
        for field in fields {
            hasher.field(field);
        }
        hasher.finalize()
    }

    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
        DomainSeparatedHash(bytes)
    }

    /// The digest bytes, big-endian.
    pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }

    /// The digest bytes by value; inverse of [`DomainSeparatedHash::from_bytes`].
    pub const fn to_bytes(&self) -> [u8; DIGEST_LEN] {
        self.0
    }

    /// Full lowercase hex encoding (128 characters).
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// First `bytes` bytes as hex, for logs and summaries.
    pub fn short_hex(&self, bytes: usize) -> String {
        self.0[..bytes.min(DIGEST_LEN)]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl fmt::Debug for DomainSeparatedHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H512({}..)", self.short_hex(8))
    }
}

/// Incremental builder for `H_512(Enc(label, f_1, ..., f_k))`.
///
/// `Enc` is injective: the label and every field are prefixed by their byte
/// length as a big-endian `u64`, and the field count is bound at finalization.
/// Field order is fixed by each call site, as the spec requires.
#[derive(Clone)]
pub struct DomainHasher(Encoder<Sha3_512>);

impl DomainHasher {
    /// Starts a hash under a versioned domain label.
    pub fn new(label: &str) -> Self {
        DomainHasher(Encoder::new(label))
    }

    /// Appends one length-prefixed byte field.
    pub fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.field(bytes);
        self
    }

    /// Appends a `u64` field (8 bytes, big-endian).
    pub fn field_u64(&mut self, value: u64) -> &mut Self {
        self.field(&value.to_be_bytes())
    }

    /// Appends a digest field.
    pub fn field_digest(&mut self, digest: &DomainSeparatedHash) -> &mut Self {
        self.field(digest.as_bytes())
    }

    /// Appends a field of `i8` values, one byte each (two's complement).
    pub fn field_i8s(&mut self, values: &[i8]) -> &mut Self {
        self.0.field_i8s(values);
        self
    }

    /// Appends a field of `i32` values, four big-endian bytes each.
    pub fn field_i32s(&mut self, values: &[i32]) -> &mut Self {
        self.0.field_i32s(values);
        self
    }

    /// Completes the encoding and returns the digest.
    pub fn finalize(&self) -> DomainSeparatedHash {
        DomainSeparatedHash(self.0.clone().finish().finalize_fixed().into())
    }
}

/// Extendable-output stream `SHAKE256(Enc(label, seed))` (FIPS 202).
///
/// The input uses the same injective encoding as [`DomainHasher`], with the
/// seed as its single field.
#[derive(Clone)]
pub struct Shake256Xof {
    reader: Shake256Reader,
    block: [u8; SHAKE256_RATE],
    offset: usize,
}

impl Shake256Xof {
    /// Starts a stream for `label` and `seed`.
    pub fn new(label: &str, seed: &DomainSeparatedHash) -> Self {
        let mut encoder = Encoder::<Shake256>::new(label);
        encoder.field(seed.as_bytes());
        Shake256Xof {
            reader: encoder.finish().finalize_xof(),
            block: [0; SHAKE256_RATE],
            offset: SHAKE256_RATE,
        }
    }

    /// Next output byte.
    pub fn next_byte(&mut self) -> u8 {
        if self.offset == SHAKE256_RATE {
            self.reader.read(&mut self.block);
            self.offset = 0;
        }
        let byte = self.block[self.offset];
        self.offset += 1;
        byte
    }

    /// Next eight output bytes as a big-endian `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        for b in &mut bytes {
            *b = self.next_byte();
        }
        u64::from_be_bytes(bytes)
    }
}

/// Streams `Enc(label, f_1, ..., f_k)` into a FIPS 202 sponge. This is the only
/// place the encoding is defined, so SHA3-512 and SHAKE256 inputs cannot drift.
#[derive(Clone)]
struct Encoder<S> {
    sponge: S,
    fields: u64,
}

impl<S: Default + Update> Encoder<S> {
    fn new(label: &str) -> Self {
        let mut sponge = S::default();
        sponge.update(&(label.len() as u64).to_be_bytes());
        sponge.update(label.as_bytes());
        Encoder { sponge, fields: 0 }
    }

    fn field(&mut self, bytes: &[u8]) {
        self.begin_field(bytes.len());
        self.sponge.update(bytes);
    }

    fn field_i8s(&mut self, values: &[i8]) {
        self.begin_field(values.len());
        let mut staged = [0u8; STAGING_BYTES];
        for chunk in values.chunks(STAGING_BYTES) {
            for (byte, &v) in staged.iter_mut().zip(chunk) {
                *byte = v as u8;
            }
            self.sponge.update(&staged[..chunk.len()]);
        }
    }

    fn field_i32s(&mut self, values: &[i32]) {
        self.begin_field(values.len() * 4);
        let mut staged = [0u8; STAGING_BYTES];
        for chunk in values.chunks(STAGING_BYTES / 4) {
            for (bytes, v) in staged.chunks_exact_mut(4).zip(chunk) {
                bytes.copy_from_slice(&v.to_be_bytes());
            }
            self.sponge.update(&staged[..chunk.len() * 4]);
        }
    }

    /// Binds the field count and returns the sponge, ready to finalize.
    fn finish(mut self) -> S {
        self.sponge.update(&self.fields.to_be_bytes());
        self.sponge
    }

    fn begin_field(&mut self, byte_len: usize) {
        self.fields += 1;
        self.sponge.update(&(byte_len as u64).to_be_bytes());
    }
}

// ---------------------------------------------------------------------------
// Post-quantum object sizes (spec Table 3) and block resources (spec §7.10)
// ---------------------------------------------------------------------------

/// Genesis-suite object sizes in bytes (spec Table 3).
pub mod pq_sizes {
    /// ML-DSA-87 public key.
    pub const ML_DSA_87_PUBLIC_KEY: u64 = 2_592;
    /// ML-DSA-87 signature.
    pub const ML_DSA_87_SIGNATURE: u64 = 4_627;
    /// SLH-DSA-SHAKE-256s public key.
    pub const SLH_DSA_SHAKE_256S_PUBLIC_KEY: u64 = 64;
    /// SLH-DSA-SHAKE-256s signature.
    pub const SLH_DSA_SHAKE_256S_SIGNATURE: u64 = 29_792;
    /// ML-KEM-1024 encapsulation key.
    pub const ML_KEM_1024_ENCAPSULATION_KEY: u64 = 1_568;
    /// ML-KEM-1024 ciphertext.
    pub const ML_KEM_1024_CIPHERTEXT: u64 = 1_568;
}

/// The five-component block resource vector `(B_bytes, B_sig, B_ZK, B_state, B_DA)`
/// of eq. (7.31). A block is valid only if the sum of its transactions' vectors
/// is componentwise at most the block limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BlockResourceVector {
    /// Serialized bytes, including post-quantum signatures and keys.
    pub bytes: u64,
    /// Signature verifications.
    pub signature_checks: u64,
    /// Zero-knowledge proof verifications.
    pub zk_checks: u64,
    /// State writes.
    pub state_writes: u64,
    /// Data-availability payload bytes.
    pub da_bytes: u64,
}

impl BlockResourceVector {
    /// The zero vector.
    pub const ZERO: BlockResourceVector = BlockResourceVector {
        bytes: 0,
        signature_checks: 0,
        zk_checks: 0,
        state_writes: 0,
        da_bytes: 0,
    };

    /// Componentwise checked sum.
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        Some(BlockResourceVector {
            bytes: self.bytes.checked_add(other.bytes)?,
            signature_checks: self.signature_checks.checked_add(other.signature_checks)?,
            zk_checks: self.zk_checks.checked_add(other.zk_checks)?,
            state_writes: self.state_writes.checked_add(other.state_writes)?,
            da_bytes: self.da_bytes.checked_add(other.da_bytes)?,
        })
    }

    /// The componentwise order `self ≼ limit`.
    pub fn fits_within(&self, limit: &Self) -> bool {
        self.bytes <= limit.bytes
            && self.signature_checks <= limit.signature_checks
            && self.zk_checks <= limit.zk_checks
            && self.state_writes <= limit.state_writes
            && self.da_bytes <= limit.da_bytes
    }

    /// Sums transaction vectors and checks the block limit; `None` if any
    /// component overflows or exceeds its limit.
    pub fn accumulate<'a>(
        txs: impl IntoIterator<Item = &'a BlockResourceVector>,
        limit: &Self,
    ) -> Option<Self> {
        let total = txs
            .into_iter()
            .try_fold(Self::ZERO, |acc, tx| acc.checked_add(tx))?;
        total.fits_within(limit).then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_supply_fits_in_u64_without_overflow() {
        let computed = TOTAL_SUPPLY_PLAU
            .checked_mul(ATOMS_PER_PLAU)
            .expect("15,000,000 PLAU * 10^8 must not overflow u64");
        assert_eq!(computed, 1_500_000_000_000_000);
        assert_eq!(Atoms::from_plau(TOTAL_SUPPLY_PLAU), Some(TOTAL_SUPPLY));
        // Headroom: M < 2^51 < p, so supply sums never alias in F_p.
        assert!(TOTAL_SUPPLY.get() < 1 << 51);
        assert!(u128::from(TOTAL_SUPPLY.get()) < u128::from(Mersenne61::P));
        // Even M summed with itself 4096 times stays inside u64.
        assert!(TOTAL_SUPPLY.get().checked_mul(4096).is_some());
    }

    #[test]
    fn atoms_arithmetic_and_display() {
        assert_eq!(ATOMS_PER_PLAU, 100_000_000);
        assert_eq!(Atoms::from_plau(u64::MAX), None);
        assert_eq!(Atoms(5).checked_sub(Atoms(6)), None);
        assert_eq!(Atoms(u64::MAX).checked_add(Atoms(1)), None);
        assert_eq!(Atoms(1_000_000_001).to_string(), "10.00000001 PLAU");
        assert_eq!(TOTAL_SUPPLY.to_string(), "15000000.00000000 PLAU");
    }

    #[test]
    fn mersenne61_constant_and_arithmetic() {
        assert_eq!(Mersenne61::P, 2_305_843_009_213_693_951);
        assert_eq!(Mersenne61::new(Mersenne61::P), Mersenne61::ZERO);
        assert_eq!(Mersenne61::new(Mersenne61::P + 5).value(), 5);
        assert_eq!(Mersenne61::new(u64::MAX).value(), u64::MAX % Mersenne61::P);
        let minus_one = Mersenne61::from_i64(-1);
        assert_eq!(minus_one.value(), Mersenne61::P - 1);
        assert_eq!(minus_one + Mersenne61::ONE, Mersenne61::ZERO);
        assert_eq!(minus_one * minus_one, Mersenne61::ONE);
        assert_eq!(
            Mersenne61::from_i64(-127) + Mersenne61::new(127),
            Mersenne61::ZERO
        );
        assert_eq!(
            Mersenne61::new(3) - Mersenne61::new(5),
            Mersenne61::from_i64(-2)
        );
        // Spot-check reduction against plain u128 remainders.
        for &(a, b) in &[
            (Mersenne61::P - 1, Mersenne61::P - 2),
            (1 << 60, 1 << 60),
            (12345, 67890),
        ] {
            let expected = ((a as u128 * b as u128) % Mersenne61::P as u128) as u64;
            assert_eq!((Mersenne61::new(a) * Mersenne61::new(b)).value(), expected);
        }
        assert_eq!(
            Mersenne61::reduce_u128(u128::MAX),
            (u128::MAX % Mersenne61::P as u128) as u64
        );
    }

    #[test]
    fn domain_separated_hash_is_deterministic_and_separated() {
        let a = DomainSeparatedHash::new(labels::FW_SHARE, &[b"work"]);
        assert_eq!(a, DomainSeparatedHash::new(labels::FW_SHARE, &[b"work"]));
        assert_ne!(a, DomainSeparatedHash::new(labels::FW_SEAL, &[b"work"]));
        // Length prefixes make the encoding injective across field boundaries.
        assert_ne!(
            DomainSeparatedHash::new(labels::FW_ID, &[b"ab", b"c"]),
            DomainSeparatedHash::new(labels::FW_ID, &[b"a", b"bc"])
        );
        assert_ne!(
            DomainSeparatedHash::new(labels::FW_ID, &[b""]),
            DomainSeparatedHash::new(labels::FW_ID, &[])
        );
        assert_eq!(a.to_hex().len(), 2 * DIGEST_LEN);
    }

    #[test]
    fn xof_is_deterministic_and_seed_bound() {
        let seed = DomainSeparatedHash::new(labels::FW_INPUT, &[b"x"]);
        let mut one = Shake256Xof::new(labels::FW_A, &seed);
        let mut two = Shake256Xof::new(labels::FW_A, &seed);
        let mut other = Shake256Xof::new(labels::FW_B, &seed);
        let a: Vec<u8> = (0..200).map(|_| one.next_byte()).collect();
        let b: Vec<u8> = (0..200).map(|_| two.next_byte()).collect();
        let c: Vec<u8> = (0..200).map(|_| other.next_byte()).collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn hashing_matches_fips_202_known_answers() {
        // Expected values come from an independent FIPS 202 implementation
        // (Python hashlib) over the explicit encoding: u64be(|label|) || label,
        // then u64be(|f_i|) || f_i for each field, then u64be(field count).
        assert_eq!(
            DomainSeparatedHash::new(labels::FW_ID, &[b"abc"]).to_hex(),
            "0bbb1255d27c9ede113769c8dcd9028660c6ec2ce89f2d8cf8b24957b4f62057\
             979dad7a89f2b5bb196b0573511be0543dd3ab0930ff95431a6854a69d7ed831"
        );
        assert_eq!(
            DomainSeparatedHash::new(labels::FW_ID, &[]).to_hex(),
            "9b7a8c1f6c33f951181f03970b17b5b4b245224d48579afa5a3b5b0861bdb877\
             9fc961acf03fbfcaa0cc91ee3452917b8d107a5b33b169fb23585e4d8019b219"
        );
        let mut h = DomainHasher::new(labels::FW_OUTPUT);
        h.field_i8s(&[-127, 0, 127])
            .field_i32s(&[-8_258_048, 1])
            .field_u64(7);
        assert_eq!(
            h.finalize().to_hex(),
            "0296b342b998fe04c9dcaa5eac9b8ae99591dba061909ec5648a99ef7d85e46e\
             6591b458d83a89b7cf54c4cf8bfeda518d1c9647b8ba5c2ba743cd88d3791c48"
        );

        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        let seed = DomainSeparatedHash::from_bytes(core::array::from_fn(|i| i as u8));
        let mut xof = Shake256Xof::new(labels::FW_A, &seed);
        let stream: Vec<u8> = (0..288).map(|_| xof.next_byte()).collect();
        assert_eq!(hex(&stream[..16]), "3e87a59a0f8f451d9f55a6d1ec30bc88");
        // Offsets 136 and 272 start the second and third squeezed blocks.
        assert_eq!(hex(&stream[136..152]), "6a79ea70a2473184f15dbba0eb28f0a0");
        assert_eq!(hex(&stream[272..288]), "6698b5c2f8d8ff674c99ee178145b43c");
        assert_eq!(
            Shake256Xof::new(labels::FW_A, &seed).next_u64(),
            0x3e87_a59a_0f8f_451d
        );
    }

    #[test]
    fn integer_fields_encode_like_their_byte_strings() {
        // Both lengths cross the 4,096-byte staging buffer.
        let i8s: Vec<i8> = (0..10_000).map(|i| (i % 255 - 127) as i8).collect();
        let i32s: Vec<i32> = (0..3_000).map(|i| (i - 1_500) * 5_501).collect();
        let i8_bytes: Vec<u8> = i8s.iter().map(|&v| v as u8).collect();
        let i32_bytes: Vec<u8> = i32s.iter().flat_map(|v| v.to_be_bytes()).collect();
        let mut typed = DomainHasher::new(labels::FW_FREIVALDS);
        typed.field_i8s(&i8s).field_i32s(&i32s);
        assert_eq!(
            typed.finalize(),
            DomainSeparatedHash::new(labels::FW_FREIVALDS, &[&i8_bytes, &i32_bytes])
        );
    }

    #[test]
    fn block_resource_vector_is_componentwise() {
        let limit = BlockResourceVector {
            bytes: 100_000,
            signature_checks: 10,
            zk_checks: 2,
            state_writes: 50,
            da_bytes: 1 << 20,
        };
        let tx = BlockResourceVector {
            bytes: pq_sizes::ML_DSA_87_SIGNATURE + pq_sizes::ML_DSA_87_PUBLIC_KEY,
            signature_checks: 1,
            zk_checks: 0,
            state_writes: 3,
            da_bytes: 0,
        };
        let total = BlockResourceVector::accumulate([tx; 10].iter(), &limit).unwrap();
        assert_eq!(total.signature_checks, 10);
        // An eleventh signature check breaks one component, even though the
        // other four still have headroom.
        assert!(BlockResourceVector::accumulate([tx; 11].iter(), &limit).is_none());
        let zk_heavy = BlockResourceVector {
            zk_checks: 3,
            ..BlockResourceVector::ZERO
        };
        assert!(!zk_heavy.fits_within(&limit));
    }
}
