//! Module 4 — deterministic scheduler and sequential D'Hondt allocation.
//!
//! The unit of execution entitlement is a discrete, immutable ShareID of
//! verified mature work — never an address (spec §6.1). Each slot goes to the
//! eligible ShareID with the largest D'Hondt quotient `w_s / (a_s + 1)`, ties
//! broken by the smallest ShareID (eq. 6.4). Quotients are compared exactly by
//! cross-multiplication (eq. 6.5); no floating point is involved.
//!
//! Owner labels enter only through bonded capacity `K_i`. Without binding
//! capacity, an entity's allocation is the sum over its ShareIDs and does not
//! depend on how they are partitioned into owners or addresses: there is no
//! aggregation premium (Theorem 6.3).
//!
//! **PoC status:** the normative weight `w_s = floor(2^640 / Θ_S(s))` (eq. 6.1)
//! needs a 640-bit integer. The PoC narrows [`ShareWeight`] to `u64`, which
//! keeps every cross-product `w_s (a_r + 1)` exact in `u128`.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use primitives::DomainSeparatedHash;
pub use primitives::ShareWeight;

/// Beneficiary bound in the Fresh Work context; enters D'Hondt only via capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerId(pub u64);

/// A discrete unit of execution entitlement: one verified mature Fresh Work share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShareID {
    /// `ShareID = WorkID`, a 64-byte digest compared as a canonical string.
    pub id: DomainSeparatedHash,
    /// Integer weight fixed by the consensus target of the parent block.
    pub weight: ShareWeight,
    /// Owner, used only to look up bonded lease capacity.
    pub owner: OwnerId,
}

/// Concurrent-lease capacity `K_i^free` per owner for one batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Capacity {
    /// No binding capacity: every ShareID stays eligible.
    #[default]
    Unbounded,
    /// Free capacity per owner; owners absent from the map have none.
    PerOwner(BTreeMap<OwnerId, u64>),
}

/// Rejection reasons when forming a cohort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchedulerError {
    /// The same ShareID appears twice (identifiers are unique, eq. 7.23).
    DuplicateShareId(DomainSeparatedHash),
    /// A ShareID has zero weight; normative weights are at least `2^128`.
    ZeroWeight(DomainSeparatedHash),
    /// Restoring a cohort requires one allocation counter per ShareID.
    CounterCountMismatch {
        /// Number of shares in the cohort.
        shares: usize,
        /// Number of supplied allocation counters.
        counters: usize,
    },
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulerError::DuplicateShareId(id) => write!(f, "duplicate ShareID {id:?}"),
            SchedulerError::ZeroWeight(id) => write!(f, "ShareID {id:?} has zero weight"),
            SchedulerError::CounterCountMismatch { shares, counters } => {
                write!(f, "cohort has {shares} shares but {counters} allocation counters")
            }
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Result of one allocation batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// `slots[t]` is the index (into the cohort's shares) of the ShareID that
    /// receives slot `t`, in canonical lease order.
    pub slots: Vec<usize>,
    /// Slots each ShareID received in this batch, indexed like the shares.
    pub per_share: Vec<u64>,
}

impl Allocation {
    /// Number of slots assigned (less than requested only if capacity ran out).
    pub fn assigned(&self) -> usize {
        self.slots.len()
    }
}

/// A snapshot `S` of mature qualifying ShareIDs with persistent counters `a_s`.
///
/// Counters persist across the batches of a cohort, so allocating `N_1` then
/// `N_2` slots without binding capacity equals allocating `N_1 + N_2` at once.
#[derive(Clone, Debug)]
pub struct Cohort {
    shares: Vec<ShareID>,
    counters: Vec<u64>,
}

impl Cohort {
    /// Forms a cohort, rejecting duplicate ShareIDs and zero weights.
    pub fn new(shares: Vec<ShareID>) -> Result<Self, SchedulerError> {
        let counters = vec![0; shares.len()];
        Self::with_counters(shares, counters)
    }

    /// Restores a cohort from its shares and cumulative allocation counters.
    /// Counter `i` belongs to share `i`; neither vector is reordered.
    pub fn with_counters(
        shares: Vec<ShareID>,
        counters: Vec<u64>,
    ) -> Result<Self, SchedulerError> {
        if shares.len() != counters.len() {
            return Err(SchedulerError::CounterCountMismatch {
                shares: shares.len(),
                counters: counters.len(),
            });
        }
        let mut seen = BTreeSet::new();
        for share in &shares {
            if share.weight == 0 {
                return Err(SchedulerError::ZeroWeight(share.id));
            }
            if !seen.insert(share.id) {
                return Err(SchedulerError::DuplicateShareId(share.id));
            }
        }
        Ok(Cohort { shares, counters })
    }

    /// The cohort's ShareIDs, in input order.
    pub fn shares(&self) -> &[ShareID] {
        &self.shares
    }

    /// Cumulative slots `a_s` held by each ShareID in this cohort.
    pub fn counters(&self) -> &[u64] {
        &self.counters
    }

    /// Total weight `W = sum w_s`.
    pub fn total_weight(&self) -> u128 {
        self.shares.iter().map(|s| u128::from(s.weight)).sum()
    }

    /// Runs the sequential D'Hondt recursion (eq. 6.4) for up to `slots` steps.
    ///
    /// Each step assigns exactly one slot to `argmax w_s / (a_s + 1)` over the
    /// eligible set `E_t = {s : K_owner(s) > 0}`, ties to the least ShareID,
    /// then increments that ShareID's counter and decrements its owner's
    /// capacity. The loop stops when `t = N` or `E_t` is empty.
    pub fn allocate_batch(&mut self, slots: usize, capacity: &Capacity) -> Allocation {
        let mut free: Option<BTreeMap<OwnerId, u64>> = match capacity {
            Capacity::Unbounded => None,
            Capacity::PerOwner(map) => Some(map.clone()),
        };
        let mut allocation = Allocation {
            slots: Vec::with_capacity(slots),
            per_share: vec![0; self.shares.len()],
        };
        while allocation.slots.len() < slots {
            let eligible = |s: &ShareID| match &free {
                None => true,
                Some(map) => map.get(&s.owner).is_some_and(|&k| k > 0),
            };
            let Some(winner) = self.select(eligible) else {
                break;
            };
            self.counters[winner] += 1;
            allocation.per_share[winner] += 1;
            allocation.slots.push(winner);
            if let Some(map) = free.as_mut()
                && let Some(k) = map.get_mut(&self.shares[winner].owner)
            {
                *k -= 1;
            }
        }
        allocation
    }

    /// Index of the eligible ShareID with the largest quotient, ties to the least ID.
    fn select(&self, eligible: impl Fn(&ShareID) -> bool) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (s, share) in self.shares.iter().enumerate() {
            if !eligible(share) {
                continue;
            }
            best = match best {
                None => Some(s),
                Some(b) => match self.compare_quotients(s, b) {
                    core::cmp::Ordering::Greater => Some(s),
                    core::cmp::Ordering::Less => Some(b),
                    core::cmp::Ordering::Equal if share.id < self.shares[b].id => Some(s),
                    core::cmp::Ordering::Equal => Some(b),
                },
            };
        }
        best
    }

    /// Exact comparison `w_s/(a_s+1)` vs `w_r/(a_r+1)` as `w_s (a_r+1)` vs `w_r (a_s+1)`.
    fn compare_quotients(&self, s: usize, r: usize) -> core::cmp::Ordering {
        let lhs = u128::from(self.shares[s].weight) * (u128::from(self.counters[r]) + 1);
        let rhs = u128::from(self.shares[r].weight) * (u128::from(self.counters[s]) + 1);
        lhs.cmp(&rhs)
    }
}

/// Allocates `available_slots` to discrete ShareIDs by sequential D'Hondt,
/// for a fresh cohort without binding capacity.
///
/// Quotients are `w_s / (a_s + 1)` and slots are assigned strictly to
/// individual ShareIDs — never to owners — so splitting or merging holdings
/// across addresses cannot change any entity's total (no aggregation premium).
/// Every slot is assigned (Theorem 6.1).
pub fn dhondt_allocation(
    shares: &[ShareID],
    available_slots: usize,
) -> Result<Allocation, SchedulerError> {
    Ok(Cohort::new(shares.to_vec())?.allocate_batch(available_slots, &Capacity::Unbounded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(tag: u64, weight: ShareWeight, owner: u64) -> ShareID {
        ShareID {
            id: DomainSeparatedHash::new("TEST/SHARE/v2", &[&tag.to_be_bytes()]),
            weight,
            owner: OwnerId(owner),
        }
    }

    #[test]
    fn floor_quota_defect_is_avoided() {
        // Nine equal sources and eight slots: every floor quota is zero
        // (eq. 6.3), yet D'Hondt assigns all eight.
        let shares: Vec<ShareID> = (0..9).map(|i| share(i, 1_000, i)).collect();
        let alloc = dhondt_allocation(&shares, 8).unwrap();
        assert_eq!(alloc.assigned(), 8);
        assert_eq!(alloc.per_share.iter().sum::<u64>(), 8);
        assert!(alloc.per_share.iter().all(|&a| a <= 1));
        // Ties go to the least ShareIDs, so the largest one is left out.
        let largest = (0..9).max_by_key(|&i| shares[i].id).unwrap();
        assert_eq!(alloc.per_share[largest], 0);
        let mut by_id: Vec<usize> = (0..9).collect();
        by_id.sort_by_key(|&i| shares[i].id);
        assert_eq!(alloc.slots, by_id[..8].to_vec());
    }

    #[test]
    fn allocation_is_complete_and_within_proportionality_bounds() {
        let weights = [97u64, 3, 41, 41, 250, 1, 13, 600];
        let shares: Vec<ShareID> = weights
            .iter()
            .enumerate()
            .map(|(i, &w)| share(i as u64, w, 0))
            .collect();
        let total: u128 = weights.iter().map(|&w| u128::from(w)).sum();
        for n in [1usize, 7, 50, 333, 1_046] {
            let alloc = dhondt_allocation(&shares, n).unwrap();
            assert_eq!(alloc.per_share.iter().sum::<u64>(), n as u64, "Theorem 6.1");
            for (a, &w) in alloc.per_share.iter().zip(&weights) {
                let (a, w, n, s) = (
                    u128::from(*a),
                    u128::from(w),
                    n as u128,
                    shares.len() as u128,
                );
                // floor(N w / W) <= a_s(N) <= (N + |S|) w / W   (eq. 6.8)
                assert!(n * w / total <= a);
                assert!(a * total <= (n + s) * w);
            }
        }
    }

    #[test]
    fn no_aggregation_premium_without_binding_capacity() {
        let weights = [500u64, 300, 200, 123, 77];
        let separate: Vec<ShareID> = weights
            .iter()
            .enumerate()
            .map(|(i, &w)| share(i as u64, w, i as u64))
            .collect();
        // The same ShareIDs merged under one owner.
        let merged: Vec<ShareID> = separate
            .iter()
            .map(|s| ShareID {
                owner: OwnerId(42),
                ..*s
            })
            .collect();
        let a = dhondt_allocation(&separate, 64).unwrap();
        let b = dhondt_allocation(&merged, 64).unwrap();
        assert_eq!(a, b, "owner labels do not enter the allocation");
        // Entity holding shares 0 and 3: its slots are just the sum over its ShareIDs.
        assert_eq!(
            a.per_share[0] + a.per_share[3],
            b.per_share[0] + b.per_share[3]
        );
    }

    #[test]
    fn counters_persist_across_batches() {
        let shares: Vec<ShareID> = [5u64, 3, 2]
            .iter()
            .enumerate()
            .map(|(i, &w)| share(i as u64, w, 0))
            .collect();
        let mut split = Cohort::new(shares.clone()).unwrap();
        let first = split.allocate_batch(4, &Capacity::Unbounded);
        let second = split.allocate_batch(6, &Capacity::Unbounded);
        let whole = Cohort::new(shares)
            .unwrap()
            .allocate_batch(10, &Capacity::Unbounded);
        let joined: Vec<usize> = first.slots.iter().chain(&second.slots).copied().collect();
        assert_eq!(joined, whole.slots);
        assert_eq!(split.counters(), &[5, 3, 2]);
    }

    #[test]
    fn restored_counters_continue_the_same_allocation() {
        let shares = vec![share(0, 5, 0), share(1, 3, 1), share(2, 2, 2)];
        let mut uninterrupted = Cohort::new(shares.clone()).unwrap();
        uninterrupted.allocate_batch(7, &Capacity::Unbounded);
        let mut restored =
            Cohort::with_counters(shares, uninterrupted.counters().to_vec()).unwrap();

        let expected = uninterrupted.allocate_batch(13, &Capacity::Unbounded);
        assert_eq!(restored.allocate_batch(13, &Capacity::Unbounded), expected);
        assert_eq!(restored.counters(), uninterrupted.counters());
    }

    #[test]
    fn restored_cohort_validates_counter_count_and_shares() {
        let s = share(0, 5, 0);
        assert!(matches!(
            Cohort::with_counters(vec![s], vec![]),
            Err(SchedulerError::CounterCountMismatch { shares: 1, counters: 0 })
        ));
        assert!(matches!(
            Cohort::with_counters(vec![s, s], vec![1, 2]),
            Err(SchedulerError::DuplicateShareId(_))
        ));
        assert!(matches!(
            Cohort::with_counters(vec![share(1, 0, 1)], vec![2]),
            Err(SchedulerError::ZeroWeight(_))
        ));
    }

    #[test]
    fn binding_capacity_redirects_slots_and_can_exhaust() {
        let shares = vec![share(0, 1_000, 1), share(1, 10, 2), share(2, 10, 3)];
        let mut cohort = Cohort::new(shares).unwrap();
        let capacity = Capacity::PerOwner(BTreeMap::from([(OwnerId(1), 2), (OwnerId(2), 1)]));
        let alloc = cohort.allocate_batch(10, &capacity);
        // Owner 1 is capped at two leases; owner 3 has no bonded capacity.
        assert_eq!(alloc.per_share, vec![2, 1, 0]);
        assert_eq!(alloc.assigned(), 3, "stops when the eligible set is empty");
    }

    #[test]
    fn exact_comparison_does_not_overflow() {
        let shares = vec![share(0, u64::MAX, 0), share(1, u64::MAX - 1, 0)];
        let alloc = dhondt_allocation(&shares, 4).unwrap();
        assert_eq!(alloc.slots, vec![0, 1, 0, 1]);
    }

    #[test]
    fn cohort_rejects_duplicates_and_zero_weight() {
        let s = share(7, 10, 0);
        assert_eq!(
            dhondt_allocation(&[s, s], 1),
            Err(SchedulerError::DuplicateShareId(s.id))
        );
        let z = share(8, 0, 0);
        assert_eq!(
            dhondt_allocation(&[z], 1),
            Err(SchedulerError::ZeroWeight(z.id))
        );
        assert_eq!(dhondt_allocation(&[], 5).unwrap().assigned(), 0);
    }
}
