//! State transition function (`chain::stf`, §7).

use core::fmt;
use std::collections::BTreeMap;

use ledger::{BalanceDelta, JobId, Ledger, LedgerError, StateCategory};
use primitives::{Atoms, ShareWeight};
use scheduler::{OwnerId, ShareID, dhondt_allocation};
use wire::{
    CategoryDelta, Digest, FeeTransition, GenesisConfig, JobTransaction, LeaseAssignment,
    OwnerReward, SettlementReceipt,
};

use crate::core_adapter::{
    JobValidationError, LABEL_LEDGER, compute_result_digest, digest_to_hash, domain_hash,
    validate_job_stateless,
};

/// Registered Fresh Work share in `NodeState` (§7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRecord {
    /// ShareID.
    pub work_id: Digest,
    /// Beneficiary.
    pub owner: u64,
    /// `w_s`, fixed by the share target at the share's height.
    pub weight: u64,
    /// Height the share was mined at.
    pub height: u64,
    /// Height of the block that registered it.
    pub included_at: u64,
}

/// Admitted commercial job awaiting settlement in `NodeState` (§7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenJob {
    /// Job identifier.
    pub job_id: u64,
    /// Transaction identifier.
    pub tx_id: Digest,
    /// Height of `JOB_ADMIT_AND_LEASE`.
    pub admitted_at: u64,
    /// Lease owner (executor by attribution).
    pub owner: u64,
    /// Leased ShareID.
    pub share_id: Digest,
    /// Executor remuneration.
    pub v_exec: u64,
    /// Data-availability remuneration.
    pub v_da: u64,
    /// Refund share.
    pub v_ref: u64,
    /// Exact dot product computed at admission.
    pub result: i128,
}

/// Immutable per-block state snapshot (§7.1).
#[derive(Clone, Debug)]
pub struct NodeState {
    /// Height of the block this state follows.
    pub height: u64,
    /// The eight-category core ledger.
    pub ledger: Ledger,
    /// Registered shares by ShareID (pruned per §6.7).
    pub shares: BTreeMap<Digest, ShareRecord>,
    /// Admitted, unsettled jobs by `job_id`.
    pub open_jobs: BTreeMap<u64, OpenJob>,
    /// Used `tx_id`s with their expiry height (pruned once expired).
    pub used_tx: BTreeMap<Digest, u64>,
    /// Used `job_id`s with their expiry height (pruned once expired).
    pub used_job_ids: BTreeMap<u64, u64>,
    /// Atoms booked at the last epoch boundary, paid at the next one.
    pub pending_payout: u64,
}

impl NodeState {
    /// Builds the genesis `NodeState` at height 0 (§3.4).
    pub fn genesis(ledger: Ledger) -> Self {
        Self {
            height: 0,
            ledger,
            shares: BTreeMap::new(),
            open_jobs: BTreeMap::new(),
            used_tx: BTreeMap::new(),
            used_job_ids: BTreeMap::new(),
            pending_payout: 0,
        }
    }

    /// Returns the mature D'Hondt lease cohort for a block at height `h`, sorted by
    /// ShareID (§6.7).
    pub fn lease_cohort(&self, h: u64, g: &GenesisConfig) -> Vec<ShareRecord> {
        let Some(max_inc) = h.checked_sub(g.share_maturity_depth) else {
            return Vec::new();
        };
        let min_inc = h.saturating_sub(g.snapshot_window);
        self.shares_included_in(min_inc, max_inc)
    }

    /// Returns the shares weighed at the epoch boundary `h`: those included in the
    /// previous epoch, sorted by ShareID (§6.7).
    pub fn epoch_shares(&self, h: u64, g: &GenesisConfig) -> Vec<ShareRecord> {
        let Some(max_inc) = h.checked_sub(1) else {
            return Vec::new();
        };
        self.shares_included_in(h.saturating_sub(g.blocks_per_epoch), max_inc)
    }

    fn shares_included_in(&self, min_inc: u64, max_inc: u64) -> Vec<ShareRecord> {
        // `shares` is a BTreeMap keyed by work_id, so the result is in ShareID order.
        self.shares
            .values()
            .filter(|s| (min_inc..=max_inc).contains(&s.included_at))
            .cloned()
            .collect()
    }
}

/// Share pre-validated (Stage A and B) for `apply_body` (§7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedShare {
    /// ShareID.
    pub work_id: Digest,
    /// Beneficiary.
    pub owner: u64,
    /// `w_s`.
    pub weight: u64,
    /// Height the share was mined at.
    pub height: u64,
}

/// Derived body commitments produced by `apply_body` (§5.3, §7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedBody {
    /// `leases[i]` belongs to `jobs[i]`.
    pub leases: Vec<LeaseAssignment>,
    /// Receipts of the jobs settled in this block.
    pub receipts: Vec<SettlementReceipt>,
    /// Every ledger operation, in execution order.
    pub fee_transitions: Vec<FeeTransition>,
    /// Commitment to the post-state.
    pub ledger_digest: Digest,
}

/// Errors produced by `apply_body` (§7.2). Any error rejects the whole block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StfError {
    /// A core ledger operation was rejected.
    Ledger(LedgerError),
    /// More than `max_jobs_per_block` jobs.
    TooManyJobs(usize),
    /// More than `max_shares_per_block` shares.
    TooManyShares(usize),
    /// Jobs are not strictly increasing by `tx_id`.
    JobsNotSortedOrUnique,
    /// Shares are not strictly increasing by `work_id`.
    SharesNotSortedOrUnique,
    /// A job fails stateless validation.
    InvalidJob(JobValidationError),
    /// A job is included after its expiry height.
    JobExpired {
        /// The job.
        job_id: u64,
        /// Its expiry height.
        expiry: u64,
        /// The including height.
        height: u64,
    },
    /// The `tx_id` was already used.
    DuplicateTx(Digest),
    /// The `job_id` was already used.
    DuplicateJobId(u64),
    /// Jobs were included while the mature share cohort is empty.
    EmptyLeaseCohort,
    /// The D'Hondt scheduler rejected the cohort or did not fill every slot.
    SchedulerError(String),
    /// The share is already registered.
    DuplicateShare(Digest),
}

impl fmt::Display for StfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StfError::Ledger(e) => write!(f, "ledger error: {e}"),
            StfError::TooManyJobs(n) => write!(f, "too many jobs in block: {n}"),
            StfError::TooManyShares(n) => write!(f, "too many shares in block: {n}"),
            StfError::JobsNotSortedOrUnique => {
                write!(f, "jobs must be strictly sorted by tx_id")
            }
            StfError::SharesNotSortedOrUnique => {
                write!(f, "shares must be strictly sorted by work_id")
            }
            StfError::InvalidJob(e) => write!(f, "invalid job: {e}"),
            StfError::JobExpired {
                job_id,
                expiry,
                height,
            } => write!(f, "job {job_id} expired at {expiry} (height {height})"),
            StfError::DuplicateTx(id) => write!(f, "duplicate tx_id {id:?}"),
            StfError::DuplicateJobId(id) => write!(f, "duplicate job_id {id}"),
            StfError::EmptyLeaseCohort => {
                write!(f, "cannot admit jobs with an empty mature share cohort")
            }
            StfError::SchedulerError(msg) => write!(f, "scheduler error: {msg}"),
            StfError::DuplicateShare(id) => write!(f, "duplicate share work_id {id:?}"),
        }
    }
}

impl std::error::Error for StfError {}

impl From<LedgerError> for StfError {
    fn from(e: LedgerError) -> Self {
        StfError::Ledger(e)
    }
}

/// Converts a `ledger::BalanceDelta` to a wire `CategoryDelta` in `StateCategory::ALL` order (§5.7).
pub fn to_category_delta(delta: &BalanceDelta) -> CategoryDelta {
    StateCategory::ALL.map(|c| delta.get(c))
}

/// Computes `ledger_digest` from the post-state (§3.3, §5.8): 15 fixed-width parts.
pub fn compute_ledger_digest(state: &NodeState) -> Digest {
    let ledger = &state.ledger;
    let balances = StateCategory::ALL.map(|c| ledger.balance(c).get().to_be_bytes());
    let head = [state.height.to_be_bytes(), ledger.epoch().to_be_bytes()];
    let tail = [
        ledger.segregated_dust().get().to_be_bytes(),
        (ledger.fee_lots().len() as u64).to_be_bytes(),
        (ledger.locked_fees().len() as u64).to_be_bytes(),
        state.pending_payout.to_be_bytes(),
    ];
    let parts: Vec<&[u8]> = head
        .iter()
        .chain(&balances)
        .chain(&tail)
        .map(|p| p.as_slice())
        .collect();
    domain_hash(LABEL_LEDGER, &parts)
}

/// Normative state transition function `apply_body` (§7.2).
///
/// Works on a clone of `parent`; on any error the clone is discarded, so a block
/// applies all-or-nothing (WP 7.14).
pub fn apply_body(
    parent: &NodeState,
    g: &GenesisConfig,
    h: u64,
    jobs: &[JobTransaction],
    shares: &[VerifiedShare],
) -> Result<(NodeState, DerivedBody), StfError> {
    if jobs.len() as u64 > g.max_jobs_per_block {
        return Err(StfError::TooManyJobs(jobs.len()));
    }
    if shares.len() as u64 > g.max_shares_per_block {
        return Err(StfError::TooManyShares(shares.len()));
    }
    if shares.windows(2).any(|w| w[0].work_id >= w[1].work_id) {
        return Err(StfError::SharesNotSortedOrUnique);
    }
    if let Some(dup) = shares
        .iter()
        .find(|sh| parent.shares.contains_key(&sh.work_id))
    {
        return Err(StfError::DuplicateShare(dup.work_id));
    }

    let mut s = parent.clone();
    let mut fee_transitions = Vec::new();
    let mut receipts = Vec::new();
    let mut leases = Vec::with_capacity(jobs.len());

    s.ledger.advance_to(h)?;

    // (1) Epoch boundary, WP (7.24)-(7.25)
    if h >= 1 && h.is_multiple_of(g.blocks_per_epoch) {
        let e = s.ledger.epoch();
        let (d_mat, lots) = s.ledger.reserve_mature()?;
        fee_transitions.push(FeeTransition::ReserveMature {
            epoch: e,
            matured_lots: lots.into_iter().map(|l| l.0).collect(),
            delta: to_category_delta(&d_mat),
        });

        let d_rew = s.ledger.fw_reward(Atoms(s.pending_payout))?;
        fee_transitions.push(FeeTransition::FwReward {
            epoch: e,
            amount: s.pending_payout,
            delta: to_category_delta(&d_rew),
        });

        let epoch_cohort = s.epoch_shares(h, g);
        let weights: Vec<ShareWeight> = epoch_cohort.iter().map(|sh| sh.weight).collect();
        let (d_book, payout) = s.ledger.book_epoch(&weights)?;

        let mut owner_totals: BTreeMap<u64, u64> = BTreeMap::new();
        for (sh, reward) in epoch_cohort.iter().zip(&payout.per_share) {
            if reward.get() > 0 {
                *owner_totals.entry(sh.owner).or_insert(0) += reward.get();
            }
        }
        let per_owner: Vec<OwnerReward> = owner_totals
            .into_iter()
            .map(|(owner, atoms)| OwnerReward { owner, atoms })
            .collect();

        s.pending_payout = payout.booked.get();
        fee_transitions.push(FeeTransition::EpochBooked {
            epoch: e,
            budget: payout.budget.get(),
            booked: payout.booked.get(),
            dust: payout.dust.get(),
            lots: payout.lots.len() as u32,
            shares: weights.len() as u32,
            per_owner,
            delta: to_category_delta(&d_book),
        });
    }

    // (2) Settle the jobs admitted at h - settle_delay, in job_id order
    let due_ids: Vec<u64> = s
        .open_jobs
        .values()
        .filter(|j| j.admitted_at.saturating_add(g.settle_delay) == h)
        .map(|j| j.job_id)
        .collect();
    for job_id in due_ids {
        let Some(j) = s.open_jobs.remove(&job_id) else {
            continue;
        };
        let d = s.ledger.result_settle(
            JobId(j.job_id),
            Atoms(j.v_exec),
            Atoms(j.v_da),
            Atoms(j.v_ref),
        )?;
        fee_transitions.push(FeeTransition::ResultSettle {
            job_id: j.job_id,
            v_exec: j.v_exec,
            v_da: j.v_da,
            v_ref: j.v_ref,
            delta: to_category_delta(&d),
        });
        receipts.push(SettlementReceipt {
            job_id: j.job_id,
            tx_id: j.tx_id,
            executor_owner: j.owner,
            admitted_at: j.admitted_at,
            result: j.result,
            result_digest: compute_result_digest(j.job_id, j.result),
            v_exec: j.v_exec,
            v_da: j.v_da,
            v_ref: j.v_ref,
        });
    }

    // (3) Fund the new jobs (sorted by tx_id)
    let mut validated: Vec<(Digest, u64, i128)> = Vec::with_capacity(jobs.len());
    for tx in jobs {
        let (tx_id, job_id, deposit, dot_result) =
            validate_job_stateless(tx).map_err(StfError::InvalidJob)?;
        if validated.last().is_some_and(|&(prev, _, _)| prev >= tx_id) {
            return Err(StfError::JobsNotSortedOrUnique);
        }
        if h > tx.expiry_height {
            return Err(StfError::JobExpired {
                job_id,
                expiry: tx.expiry_height,
                height: h,
            });
        }
        if s.used_tx.contains_key(&tx_id) {
            return Err(StfError::DuplicateTx(tx_id));
        }
        if s.used_job_ids.contains_key(&job_id) || validated.iter().any(|v| v.1 == job_id) {
            return Err(StfError::DuplicateJobId(job_id));
        }

        let d =
            s.ledger
                .escrow_fund_and_queue(JobId(job_id), Atoms(deposit), Atoms(tx.queue_fee))?;
        fee_transitions.push(FeeTransition::EscrowFundAndQueue {
            job_id,
            deposit,
            queue_fee: tx.queue_fee,
            delta: to_category_delta(&d),
        });
        validated.push((tx_id, job_id, dot_result));
    }

    // (4) Allocate leases with D'Hondt over the mature cohort, WP (6.4)
    if !jobs.is_empty() {
        let cohort = s.lease_cohort(h, g);
        if cohort.is_empty() {
            return Err(StfError::EmptyLeaseCohort);
        }
        let share_ids: Vec<ShareID> = cohort
            .iter()
            .map(|sh| ShareID {
                id: digest_to_hash(&sh.work_id),
                weight: sh.weight,
                owner: OwnerId(sh.owner),
            })
            .collect();

        let alloc = dhondt_allocation(&share_ids, jobs.len())
            .map_err(|e| StfError::SchedulerError(e.to_string()))?;
        if alloc.assigned() != jobs.len() {
            return Err(StfError::SchedulerError(
                "D'Hondt did not assign all requested job slots".to_string(),
            ));
        }

        for ((tx, &(tx_id, job_id, dot_result)), &slot) in
            jobs.iter().zip(&validated).zip(&alloc.slots)
        {
            let share = cohort.get(slot).ok_or_else(|| {
                StfError::SchedulerError(format!("slot index {slot} outside the cohort"))
            })?;
            let d = s
                .ledger
                .job_admit_and_lease(JobId(job_id), Atoms(tx.base_fee))?;
            fee_transitions.push(FeeTransition::JobAdmitAndLease {
                job_id,
                base_fee: tx.base_fee,
                owner: share.owner,
                delta: to_category_delta(&d),
            });
            leases.push(LeaseAssignment {
                job_id,
                share_id: share.work_id,
                owner: share.owner,
            });
            s.open_jobs.insert(
                job_id,
                OpenJob {
                    job_id,
                    tx_id,
                    admitted_at: h,
                    owner: share.owner,
                    share_id: share.work_id,
                    v_exec: tx.v_exec,
                    v_da: tx.v_da,
                    v_ref: tx.v_ref,
                    result: dot_result,
                },
            );
            s.used_tx.insert(tx_id, tx.expiry_height);
            s.used_job_ids.insert(job_id, tx.expiry_height);
        }
    }

    // (5) Register the shares of the body
    for sh in shares {
        s.shares.insert(
            sh.work_id,
            ShareRecord {
                work_id: sh.work_id,
                owner: sh.owner,
                weight: sh.weight,
                height: sh.height,
                included_at: h,
            },
        );
    }

    // (6) Prune, check, commit
    s.used_tx.retain(|_, &mut expiry| expiry >= h);
    s.used_job_ids.retain(|_, &mut expiry| expiry >= h);
    let retain = g
        .snapshot_window
        .max(g.blocks_per_epoch)
        .max(g.share_inclusion_window)
        .saturating_add(1);
    if let Some(min_keep) = h.checked_sub(retain) {
        s.shares.retain(|_, sh| sh.included_at >= min_keep);
    }

    s.ledger.check_invariants()?;
    s.height = h;
    let ledger_digest = compute_ledger_digest(&s);

    Ok((
        s,
        DerivedBody {
            leases,
            receipts,
            fee_transitions,
            ledger_digest,
        },
    ))
}
