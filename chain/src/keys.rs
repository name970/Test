//! ML-DSA-87 owner keys (`chain::keys`): key generation from a FIPS 204 seed and
//! signing of headers, shares and jobs over their domain-separated digests.
//!
//! Verification lives in [`crate::core_adapter`]; this module only signs.

use core::fmt;

use ml_dsa::common::getrandom::SysRng;
use ml_dsa::{ExpandedSigningKey, MlDsa87, Seed};
use wire::{BlockHeader, Digest, JobTransaction, PublicKey, ShareProof, Signature};

use crate::core_adapter::{
    SIGNATURE_CONTEXT, header_signing_digest, job_signing_digest, owner_id_from_key_id,
    owner_key_id, share_signing_digest,
};

/// Length of the FIPS 204 key-generation seed `ξ`, the only secret that is stored.
pub const SEED_LEN: usize = 32;

/// An ML-DSA-87 key pair with its on-chain identity.
pub struct OwnerKeypair {
    seed: [u8; SEED_LEN],
    /// Boxed: the expanded key is about 100 KB, too large to move around by value
    /// (unoptimized builds overflow a 2 MiB thread stack).
    signing_key: Box<ExpandedSigningKey<MlDsa87>>,
    public_key: PublicKey,
    key_id: Digest,
}

impl OwnerKeypair {
    /// Derives the key pair from a 32-byte seed (FIPS 204 `ML-DSA.KeyGen_internal`).
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let signing_key = Box::new(ExpandedSigningKey::<MlDsa87>::from_seed(&Seed::from(*seed)));
        let encoded = signing_key.verifying_key().encode();
        let public_key =
            PublicKey::from_slice(&encoded).expect("invariant: ML-DSA-87 keys are 2,592 bytes");
        let key_id = owner_key_id(&public_key);
        Self {
            seed: *seed,
            signing_key,
            public_key,
            key_id,
        }
    }

    /// Generates a fresh key pair from the operating system's RNG.
    pub fn generate() -> Result<Self, String> {
        let mut seed = [0u8; SEED_LEN];
        ml_dsa::common::getrandom::fill(&mut seed)
            .map_err(|e| format!("OS random number generator failed: {e}"))?;
        Ok(Self::from_seed(&seed))
    }

    /// The secret seed. Treat it as key material.
    pub fn seed(&self) -> &[u8; SEED_LEN] {
        &self.seed
    }

    /// The encoded public key.
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// The owner key id, `H(PLAU/TESTNET/OWNER/v1, public key)`.
    pub fn key_id(&self) -> Digest {
        self.key_id
    }

    /// The 64-bit `scheduler::OwnerId` derived from the key id.
    pub fn owner_id(&self) -> u64 {
        owner_id_from_key_id(&self.key_id)
    }

    /// Signs a domain-separated digest with hedged (randomized) FIPS 204 `ML-DSA.Sign`
    /// and the context [`SIGNATURE_CONTEXT`].
    pub fn sign_digest(&self, digest: &Digest) -> Result<Signature, String> {
        let sig = self
            .signing_key
            .sign_randomized(&digest.0, SIGNATURE_CONTEXT, &mut SysRng)
            .map_err(|e| format!("ML-DSA-87 signing failed: {e}"))?;
        Signature::from_slice(&sig.encode())
    }

    /// Sets `header.miner_key` to this key and signs the header.
    pub fn sign_header(&self, header: &mut BlockHeader) -> Result<(), String> {
        header.miner_key = self.public_key.clone();
        header.signature = self.sign_digest(&header_signing_digest(header)?)?;
        Ok(())
    }

    /// Sets `share.owner_key` to this key and signs the share.
    pub fn sign_share(&self, share: &mut ShareProof) -> Result<(), String> {
        share.owner_key = self.public_key.clone();
        share.signature = self.sign_digest(&share_signing_digest(share)?)?;
        Ok(())
    }

    /// Sets `tx.client_key` to this key and signs the job for the chain with `genesis_id`.
    pub fn sign_job(&self, tx: &mut JobTransaction, genesis_id: &Digest) -> Result<(), String> {
        tx.client_key = self.public_key.clone();
        tx.signature = self.sign_digest(&job_signing_digest(tx, genesis_id)?)?;
        Ok(())
    }
}

impl fmt::Debug for OwnerKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnerKeypair")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// An all-zero signature: the placeholder a freshly built object carries until it is signed.
pub fn unsigned() -> Signature {
    Signature(Box::new([0u8; wire::SIGNATURE_LEN]))
}
