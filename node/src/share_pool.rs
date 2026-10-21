//! Share pool (`node::share_pool`, §6.7): verified share proofs waiting for a block.
//!
//! It holds this node's own shares and the gossiped shares that passed every check,
//! signature and work included, one per `work_id`. Templates pick from it
//! (`chain::build_template`), so any miner can include any owner's share.

use std::collections::HashSet;

use chain::SignedShare;
use wire::Digest;

/// Maximum pooled shares. The inclusion window holds far fewer in a healthy network
/// (about `share_ratio` shares per height).
pub const SHARE_POOL_CAPACITY: usize = 1_024;

/// Verified shares by `work_id`.
pub struct SharePool {
    shares: Vec<SignedShare>,
    work_ids: HashSet<Digest>,
    capacity: usize,
}

impl SharePool {
    /// An empty pool holding at most `capacity` shares.
    pub fn new(capacity: usize) -> Self {
        Self {
            shares: Vec::new(),
            work_ids: HashSet::new(),
            capacity,
        }
    }

    /// Number of pooled shares.
    pub fn len(&self) -> usize {
        self.shares.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.shares.is_empty()
    }

    /// Whether a share with this `work_id` is pooled.
    pub fn contains(&self, work_id: &Digest) -> bool {
        self.work_ids.contains(work_id)
    }

    /// The pooled shares, in no particular order.
    pub fn as_slice(&self) -> &[SignedShare] {
        &self.shares
    }

    /// Adds a share; returns false if its `work_id` is already pooled. A full pool
    /// evicts its oldest share (lowest height, then lowest `work_id`), unless the new
    /// share would be that oldest one, in which case it is not added.
    pub fn insert(&mut self, share: SignedShare) -> bool {
        let work_id = share.proof().work_id;
        if self.work_ids.contains(&work_id) {
            return false;
        }
        if self.shares.len() >= self.capacity {
            let key = |s: &SignedShare| (s.proof().context.height, s.proof().work_id);
            let Some(oldest) = self
                .shares
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| key(s))
                .map(|(i, _)| i)
            else {
                return false;
            };
            if key(&share) <= key(&self.shares[oldest]) {
                return false;
            }
            let evicted = self.shares.swap_remove(oldest);
            self.work_ids.remove(&evicted.proof().work_id);
        }
        self.work_ids.insert(work_id);
        self.shares.push(share);
        true
    }

    /// Drops the shares mined below `min_height` (outside the inclusion window) and
    /// those for which `registered` holds (already included on the canonical chain).
    pub fn prune(&mut self, min_height: u64, registered: impl Fn(&Digest) -> bool) {
        let work_ids = &mut self.work_ids;
        self.shares.retain(|s| {
            let keep = s.proof().context.height >= min_height && !registered(&s.proof().work_id);
            if !keep {
                work_ids.remove(&s.proof().work_id);
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain::OwnerKeypair;
    use chain::keys::unsigned;
    use wire::{FwContext, ShareProof, Target};

    fn share(owner: &OwnerKeypair, height: u64, tag: u8) -> SignedShare {
        let zero = Digest::ZERO;
        let mut proof = ShareProof {
            context: FwContext {
                chain_id: 0,
                genesis_id: zero,
                protocol_version: 0,
                crypto_suite_id: 0,
                program_id: zero,
                air_id: zero,
                generator_version: 0,
                parent_canonical_id: zero,
                stable_anchor_id: zero,
                height,
                miner_policy_digest: zero,
                payout_policy_digest: zero,
                attempt_nonce: 0,
                body_core_root: zero,
                parent_derived_difficulty_context: zero,
                time_bucket: 0,
            },
            owner_key: owner.public_key().clone(),
            output_root: zero,
            work_id: Digest([tag; 64]),
            share_ticket: zero,
            share_target: Target(1 << 120),
            signature: unsigned(),
        };
        owner.sign_share(&mut proof).unwrap();
        SignedShare::verify(proof).unwrap()
    }

    #[test]
    fn dedupes_by_work_id_evicts_oldest_and_prunes() {
        let owner = OwnerKeypair::from_seed(&[1; 32]);
        let mut pool = SharePool::new(3);
        assert!(pool.insert(share(&owner, 5, 1)));
        assert!(!pool.insert(share(&owner, 5, 1)), "one share per work_id");
        assert!(pool.insert(share(&owner, 3, 2)));
        assert!(pool.insert(share(&owner, 7, 3)));
        assert_eq!(pool.len(), 3);

        // Full: a share older than everything pooled is not added...
        assert!(!pool.insert(share(&owner, 2, 4)));
        // ...a newer one evicts the oldest (height 3).
        assert!(pool.insert(share(&owner, 6, 5)));
        assert!(!pool.contains(&Digest([2; 64])));
        assert!(pool.contains(&Digest([5; 64])));

        // Pruning: below the window, or already registered on the chain.
        pool.prune(6, |w| *w == Digest([3; 64]));
        let left: Vec<Digest> = pool.as_slice().iter().map(|s| s.proof().work_id).collect();
        assert_eq!(left, vec![Digest([5; 64])]);
        assert!(!pool.contains(&Digest([1; 64])));
        assert!(
            pool.insert(share(&owner, 5, 1)),
            "a pruned work_id may return"
        );
    }
}
