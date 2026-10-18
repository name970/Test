//! Plauthium Testnet synchronous chain library (`chain`).
//!
//! Wraps the frozen mathematical core (`primitives`, `work`, `scivm`, `scheduler`, `ledger`)
//! with genesis validation, Stage A/B verification, the state transition function (`apply_body`),
//! block template construction, and fork choice (`BlockTree`). Nothing here is async: the
//! `node` crate drives it from Tokio actors.

#![forbid(unsafe_code)]

pub mod core_adapter;
pub mod difficulty;
pub mod stf;
pub mod template;
pub mod tree;

pub use core_adapter::{
    ClaimedWork, JobValidationError, Lottery, WorkError, compute_body_root, compute_client_digest,
    compute_difficulty_context, compute_genesis_id, compute_job_ids, compute_miner_policy_digest,
    compute_payout_policy_digest, compute_program_and_air_ids, compute_result_digest,
    testnet_ozaki_profile, top128, validate_genesis, validate_job_stateless, verify_fresh_work,
};
pub use difficulty::{
    ChainView, expected_targets, scale_target, seal_weight, share_target_for, share_weight, work_of,
};
pub use stf::{
    DerivedBody, NodeState, OpenJob, ShareRecord, StfError, VerifiedShare, apply_body,
    compute_ledger_digest, to_category_delta,
};
pub use template::{Template, build_template};
pub use tree::{
    AncestorChainView, BlockMeta, BlockSummary, BlockTree, InsertOutcome, JobSummary,
    ReceiptSummary, ShareSummary, TreeError,
};
