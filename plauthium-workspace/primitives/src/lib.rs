//! Shared Plauthium primitives.
//!
//! Every consensus quantity is an integer (spec §1.4): amounts are [`Atoms`],
//! field arithmetic is over [`Mersenne61`], and every committed object is a
//! 512-bit [`DomainSeparatedHash`] of a typed, length-prefixed encoding.
//!
//! **PoC status:** [`DomainSeparatedHash`] and [`MockXof`] are deterministic,
//! *non-cryptographic* placeholders for SHA3-512 and SHAKE256. They keep the
//! normative interface (label, injective encoding, 64-byte digests) so that the
//! real primitives can be swapped in without touching callers.

use core::fmt;
use core::ops::{Add, AddAssign, Mul, Neg, Sub};

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
// Domain-separated hashing (spec §1.4, eq. 1.3) — PLACEHOLDER for SHA3-512
// ---------------------------------------------------------------------------

/// Digest length of SHA3-512 in bytes.
pub const DIGEST_LEN: usize = 64;

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

/// A 512-bit digest `H_512(Enc(LABEL/v2, f_1, ..., f_k))`.
///
/// Ordering is lexicographic over the 64 bytes, which equals the numeric order
/// of the digest read as a big-endian integer (spec §1.4) and the canonical
/// 64-byte-string order used for ShareID tie-breaks (spec §6.3).
///
/// **PLACEHOLDER:** the compression function is a non-cryptographic mixer.
/// Replace with SHA3-512 (FIPS 202) before any security claim is made.
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
pub struct DomainHasher {
    sponge: MockSponge,
    fields: u64,
}

impl DomainHasher {
    /// Starts a hash under a versioned domain label.
    pub fn new(label: &str) -> Self {
        let mut sponge = MockSponge::new();
        sponge.absorb(&(label.len() as u64).to_be_bytes());
        sponge.absorb(label.as_bytes());
        DomainHasher { sponge, fields: 0 }
    }

    /// Appends one length-prefixed byte field.
    pub fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.begin_field(bytes.len());
        self.sponge.absorb(bytes);
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
        self.begin_field(values.len());
        for &v in values {
            self.sponge.absorb_byte(v as u8);
        }
        self
    }

    /// Appends a field of `i32` values, four big-endian bytes each.
    pub fn field_i32s(&mut self, values: &[i32]) -> &mut Self {
        self.begin_field(values.len() * 4);
        for &v in values {
            self.sponge.absorb(&v.to_be_bytes());
        }
        self
    }

    /// Completes the encoding and returns the digest.
    pub fn finalize(&self) -> DomainSeparatedHash {
        let mut sponge = self.sponge.clone();
        sponge.absorb(&self.fields.to_be_bytes());
        DomainSeparatedHash(sponge.squeeze())
    }

    fn begin_field(&mut self, byte_len: usize) {
        self.fields += 1;
        self.sponge.absorb(&(byte_len as u64).to_be_bytes());
    }
}

/// Extendable-output stream bound to a label and a seed.
///
/// **PLACEHOLDER** for `SHAKE256(Enc(label, seed))`; output block `i` is
/// `H_512(Enc(label, seed, i))` of the placeholder hash.
#[derive(Clone)]
pub struct MockXof {
    prefix: DomainHasher,
    counter: u64,
    block: [u8; DIGEST_LEN],
    offset: usize,
}

impl MockXof {
    /// Starts a stream for `label` and `seed`.
    pub fn new(label: &str, seed: &DomainSeparatedHash) -> Self {
        let mut prefix = DomainHasher::new(label);
        prefix.field_digest(seed);
        MockXof {
            prefix,
            counter: 0,
            block: [0; DIGEST_LEN],
            offset: DIGEST_LEN,
        }
    }

    /// Next output byte.
    pub fn next_byte(&mut self) -> u8 {
        if self.offset == DIGEST_LEN {
            let mut block = self.prefix.clone();
            block.field_u64(self.counter);
            self.block = *block.finalize().as_bytes();
            self.counter += 1;
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

const SPONGE_LANES: usize = 8;
const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// Non-cryptographic 512-bit state used by the hashing placeholders.
#[derive(Clone)]
struct MockSponge {
    lanes: [u64; SPONGE_LANES],
    buffer: [u8; 8],
    buffered: usize,
    absorbed: u64,
}

impl MockSponge {
    fn new() -> Self {
        let mut lanes = [0u64; SPONGE_LANES];
        for (i, lane) in lanes.iter_mut().enumerate() {
            *lane = mix64(GOLDEN_GAMMA.wrapping_mul(i as u64 + 1));
        }
        MockSponge {
            lanes,
            buffer: [0; 8],
            buffered: 0,
            absorbed: 0,
        }
    }

    fn absorb(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.absorb_byte(b);
        }
    }

    fn absorb_byte(&mut self, byte: u8) {
        self.buffer[self.buffered] = byte;
        self.buffered += 1;
        self.absorbed = self.absorbed.wrapping_add(1);
        if self.buffered == 8 {
            self.absorb_word(u64::from_le_bytes(self.buffer));
            self.buffered = 0;
        }
    }

    fn absorb_word(&mut self, word: u64) {
        for i in 0..SPONGE_LANES {
            let neighbour = self.lanes[(i + SPONGE_LANES - 1) % SPONGE_LANES];
            self.lanes[i] = mix64(self.lanes[i] ^ word ^ neighbour.rotate_left(17));
        }
    }

    fn squeeze(mut self) -> [u8; DIGEST_LEN] {
        let absorbed = self.absorbed;
        self.absorb_byte(0x80);
        while self.buffered != 0 {
            self.absorb_byte(0);
        }
        self.absorb_word(absorbed);
        for _ in 0..SPONGE_LANES {
            self.absorb_word(GOLDEN_GAMMA);
        }
        let mut out = [0u8; DIGEST_LEN];
        for (chunk, lane) in out.chunks_exact_mut(8).zip(self.lanes) {
            chunk.copy_from_slice(&lane.to_be_bytes());
        }
        out
    }
}

/// SplitMix64 finalizer.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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
        let mut one = MockXof::new(labels::FW_A, &seed);
        let mut two = MockXof::new(labels::FW_A, &seed);
        let mut other = MockXof::new(labels::FW_B, &seed);
        let a: Vec<u8> = (0..200).map(|_| one.next_byte()).collect();
        let b: Vec<u8> = (0..200).map(|_| two.next_byte()).collect();
        let c: Vec<u8> = (0..200).map(|_| other.next_byte()).collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
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
