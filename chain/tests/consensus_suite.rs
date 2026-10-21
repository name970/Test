//! Consensus and STF tests (§11 unit tests 3–7).

#![forbid(unsafe_code)]

use std::sync::Arc;

use chain::core_adapter::fw_context_to_core;
use chain::{
    BlockTree, StfError, TreeError, VerifiedShare, apply_body, build_template, compute_body_root,
    compute_client_digest, compute_difficulty_context, compute_job_ids,
};
use ledger::{LedgerError, StateCategory};
use primitives::{Atoms, TOTAL_SUPPLY};
use wire::{
    Block, DIGEST_LEN, DifficultyMode, Digest, FeeTransition, GenesisConfig, JobPayload,
    JobTransaction, MAX_REORG_DEPTH, ShareProof, Target,
};
use work::FreshWorkIds;

fn fast_genesis() -> GenesisConfig {
    let mut g = GenesisConfig::default_testnet(1_700_000_000);
    // Every attempt seals and every header is a share: one tile per block.
    g.initial_seal_target = Target(u128::MAX);
    g.share_ratio = 1;
    g.difficulty = DifficultyMode::Fixed;
    g.blocks_per_epoch = 5;
    g.maturation_delay = 2;
    g.settle_delay = 2;
    g.share_maturity_depth = 1;
    g.snapshot_window = 30;
    g.share_inclusion_window = 15;
    g
}

fn make_job(
    seed: u64,
    queue_fee: u64,
    base_fee: u64,
    v_exec: u64,
    expiry_height: u64,
) -> JobTransaction {
    let mut s = seed.wrapping_add(1);
    let mut next_val = || -> f64 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s % 2000) as i64 - 1000) as f64
    };
    let xs: Vec<f64> = (0..512).map(|_| next_val()).collect();
    let ys: Vec<f64> = (0..512).map(|_| next_val()).collect();
    JobTransaction {
        client: compute_client_digest("bench").unwrap(),
        queue_fee,
        base_fee,
        v_exec,
        v_da: 50_000_000,
        v_ref: 50_000_000,
        expiry_height,
        nonce: seed,
        payload: JobPayload::ExactDot { xs, ys },
    }
}

fn template_on(
    tree: &BlockTree,
    parent_id: Digest,
    owner: u64,
    mempool: &[JobTransaction],
    shares: &[ShareProof],
) -> chain::Template {
    let anc = tree.ancestor_view(parent_id).unwrap();
    let parent_meta = tree.get_meta(&parent_id).unwrap();
    let parent_state = tree.get_state(&parent_id).unwrap();
    build_template(
        1,
        tree.genesis(),
        tree.genesis_id(),
        &anc,
        parent_meta.height,
        &parent_state,
        mempool,
        shares,
        owner,
        parent_meta.time_bucket + 1,
    )
    .unwrap()
}

fn work_ids(ctx: &wire::FwContext) -> FreshWorkIds {
    let core_ctx = fw_context_to_core(ctx).unwrap();
    let inst = core_ctx.instance();
    let z = inst.x.product(&inst.y);
    FreshWorkIds::derive(&core_ctx, &z)
}

fn mine_block_on(
    tree: &BlockTree,
    parent_id: Digest,
    owner: u64,
    mempool: &[JobTransaction],
    shares: &[ShareProof],
    nonce: u64,
) -> Block {
    let tpl = template_on(tree, parent_id, owner, mempool, shares);
    tpl.seal(nonce, &work_ids(&tpl.context_with_nonce(nonce)))
}

fn mine_share_on(tree: &BlockTree, parent_id: Digest, owner: u64, nonce: u64) -> ShareProof {
    let tpl = template_on(tree, parent_id, owner, &[], &[]);
    tpl.share(nonce, &work_ids(&tpl.context_with_nonce(nonce)))
}

fn extend(tree: &mut BlockTree, block: Block) {
    tree.pre_validate(&block, None).unwrap();
    BlockTree::verify_stage_a(&block).unwrap();
    tree.apply_verified_block(Arc::new(block)).unwrap();
}

/// Unit test 3 (§11): 200 blocks with random job counts, random fees and epoch boundaries.
/// `apply_body` conserves `M = 1.5 * 10^15` atoms and passes `check_invariants()` after
/// every block.
#[test]
fn randomized_200_blocks_conserve_supply_m() {
    let g = fast_genesis();
    let (_, ledger, _) = chain::validate_genesis(&g).unwrap();
    let mut state = chain::NodeState::genesis(ledger);

    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut next_u64 = || -> u64 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut admitted = 0usize;
    let mut settled = 0usize;
    for h in 1..=200u64 {
        let share_owner = (next_u64() % 5) + 1;
        let mut wid = [0u8; DIGEST_LEN];
        wid[0..8].copy_from_slice(&h.to_be_bytes());
        wid[8..16].copy_from_slice(&next_u64().to_be_bytes());
        let shares = vec![VerifiedShare {
            work_id: Digest(wid),
            owner: share_owner,
            weight: 34_359_738_368,
            height: h.saturating_sub(1).max(1),
        }];

        let mut jobs = Vec::new();
        // The epoch-1 booking is paid at h = 10; from then on U has spendable rewards.
        if h >= 10 {
            let count = (next_u64() % 4) as usize;
            for j in 0..count {
                let seed = h * 100 + j as u64;
                let q_fee = ((next_u64() % 5) + 1) * 10_000_000;
                let b_fee = ((next_u64() % 10) + 1) * 100_000_000;
                let v_exec = ((next_u64() % 20) + 1) * 100_000_000;
                jobs.push(make_job(seed, q_fee, b_fee, v_exec, h + 50));
            }
            jobs.sort_by_key(|tx| compute_job_ids(tx).unwrap().0);
        }

        let (next_state, derived) = apply_body(&state, &g, h, &jobs, &shares).unwrap();
        assert_eq!(next_state.ledger.total(), u128::from(TOTAL_SUPPLY.get()));
        next_state.ledger.check_invariants().unwrap();
        for ft in &derived.fee_transitions {
            let sum: i128 = ft.delta().iter().sum();
            assert_eq!(sum, 0, "every fee transition delta must sum to zero");
        }
        assert_eq!(derived.leases.len(), jobs.len());
        admitted += jobs.len();
        settled += derived.receipts.len();
        state = next_state;
    }
    assert!(admitted > 100, "the random schedule must admit many jobs");
    assert_eq!(
        settled + state.open_jobs.len(),
        admitted,
        "every admitted job settles after settle_delay"
    );
}

/// Unit test 4 (§11): tampering with a derived field, a context field, a target,
/// `miner_owner`, an operand, or the fee-transition order is rejected.
#[test]
fn tampering_with_block_fields_is_rejected() {
    let g = fast_genesis();
    let mut tree = BlockTree::new(g).unwrap();

    // Advance to height 10, where the epoch-1 booking is paid into U.
    for h in 1..=10 {
        let blk = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], h);
        extend(&mut tree, blk);
    }

    let job = make_job(555, 100_000_000, 1_000_000_000, 8_000_000_000, 100);
    let honest = mine_block_on(&tree, tree.tip_id(), 1, &[job], &[], 11);
    assert_eq!(honest.body.jobs.len(), 1);
    assert!(honest.body.fee_transitions.len() >= 2);

    // Derived fields: rejected by apply_verified_block even with a consistent body root.
    let mut bad_digest = honest.clone();
    bad_digest.body.ledger_digest.0[0] ^= 0x01;
    bad_digest.header.context.body_core_root = compute_body_root(&bad_digest.body).unwrap();
    assert!(tree.apply_verified_block(Arc::new(bad_digest)).is_err());

    let mut bad_ft_order = honest.clone();
    bad_ft_order.body.fee_transitions.reverse();
    bad_ft_order.header.context.body_core_root = compute_body_root(&bad_ft_order.body).unwrap();
    assert!(tree.apply_verified_block(Arc::new(bad_ft_order)).is_err());

    let mut bad_leases = honest.clone();
    bad_leases.body.leases[0].owner = 999;
    bad_leases.header.context.body_core_root = compute_body_root(&bad_leases.body).unwrap();
    assert!(tree.apply_verified_block(Arc::new(bad_leases)).is_err());

    // Context field: rejected by Stage B.
    let mut bad_ctx = honest.clone();
    bad_ctx.header.context.height += 1;
    assert!(tree.pre_validate(&bad_ctx, None).is_err());

    // Targets, even with a matching difficulty context digest.
    let mut bad_target = honest.clone();
    bad_target.header.seal_target = Target(u128::MAX - 1);
    bad_target.header.context.parent_derived_difficulty_context = compute_difficulty_context(
        bad_target.header.seal_target,
        bad_target.header.share_target,
    );
    assert!(tree.pre_validate(&bad_target, None).is_err());

    let mut bad_owner = honest.clone();
    bad_owner.header.miner_owner = 2;
    assert!(tree.pre_validate(&bad_owner, None).is_err());

    // One operand changed without updating body_core_root.
    let mut bad_operand = honest.clone();
    let JobPayload::ExactDot { ref mut xs, .. } = bad_operand.body.jobs[0].payload;
    xs[0] += 1.0;
    assert!(tree.pre_validate(&bad_operand, None).is_err());

    // Stage A: a forged nonce no longer matches the claimed work.
    let mut bad_nonce = honest.clone();
    bad_nonce.header.context.attempt_nonce += 1;
    assert!(BlockTree::verify_stage_a(&bad_nonce).is_err());

    // The honest block still applies.
    extend(&mut tree, honest);
    assert_eq!(tree.tip_meta().height, 11);
}

/// Unit test 5 (§11): job flow, `FounderTrancheLocked`, and `R_l -> R_a` as two lots.
#[test]
fn job_fee_lock_and_two_lot_maturation_flow() {
    let g = fast_genesis(); // blocks_per_epoch = 5, maturation_delay = 2, settle_delay = 2
    let mut tree = BlockTree::new(g.clone()).unwrap();
    let job = make_job(999, 100_000_000, 1_000_000_000, 8_000_000_000, 100);

    // Funding before any reward reached U fails on the locked founder tranches.
    let sh = VerifiedShare {
        work_id: Digest([1u8; DIGEST_LEN]),
        owner: 1,
        weight: 100,
        height: 1,
    };
    let (s1, _) = apply_body(&tree.tip_state(), &g, 1, &[], &[sh]).unwrap();
    let res = apply_body(&s1, &g, 2, std::slice::from_ref(&job), &[]);
    assert!(matches!(
        res,
        Err(StfError::Ledger(LedgerError::FounderTrancheLocked { .. }))
    ));

    // Blocks 1..=9: the job stays out of the template.
    for h in 1..=9 {
        let blk = mine_block_on(&tree, tree.tip_id(), 1, std::slice::from_ref(&job), &[], h);
        assert!(blk.body.jobs.is_empty(), "h = {h}");
        extend(&mut tree, blk);
    }

    // h = 10 (epoch 2): the epoch-1 booking is paid into U and the job is admitted;
    // R_l holds queue_fee + base_fee = 1.1 * 10^9 atoms as two locked fees.
    let b10 = mine_block_on(&tree, tree.tip_id(), 1, std::slice::from_ref(&job), &[], 10);
    assert_eq!(b10.body.jobs.len(), 1);
    assert!(matches!(
        b10.body.fee_transitions[1],
        FeeTransition::FwReward { amount, .. } if amount > 0
    ));
    extend(&mut tree, b10);
    let ledger = &tree.tip_state().ledger;
    assert_eq!(
        ledger.balance(StateCategory::LockedBaseFees),
        Atoms(1_100_000_000)
    );
    // After admission the escrow holds exactly v_exec + v_da + v_ref.
    assert_eq!(ledger.balance(StateCategory::Escrow), Atoms(8_100_000_000));
    assert_eq!(ledger.locked_fees().len(), 2);

    // Paid in epoch 2 with L = 2: maturity epoch 2 + 2 + 1 = 5, i.e. h = 25.
    for h in 11..=25 {
        let blk = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], h);
        assert_eq!(blk.body.receipts.len(), usize::from(h == 12), "h = {h}");
        if h == 25 {
            let mat = blk
                .body
                .fee_transitions
                .iter()
                .find_map(|ft| match ft {
                    FeeTransition::ReserveMature { matured_lots, .. } => Some(matured_lots.clone()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                mat.len(),
                2,
                "queue_fee and base_fee mature as 2 independent lots"
            );
        }
        extend(&mut tree, blk);
    }

    let ledger = &tree.tip_state().ledger;
    assert_eq!(ledger.balance(StateCategory::LockedBaseFees), Atoms::ZERO);
    assert_eq!(ledger.balance(StateCategory::Escrow), Atoms::ZERO);
    assert_eq!(
        ledger.fee_lots().len(),
        3,
        "Genesis lot + 2 matured fee lots"
    );
}

/// Unit tests 6 and 7 (§11): share inclusion rules and `MAX_REORG_DEPTH`.
#[test]
fn share_validation_rules_and_max_reorg_depth() {
    let mut g = fast_genesis();
    g.share_inclusion_window = 3;
    let mut tree = BlockTree::new(g).unwrap();

    for h in 1..=5 {
        let blk = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], h);
        extend(&mut tree, blk);
    }

    // A share mined at height 1 is outside the window (3) at height 6.
    let p0 = tree.canonical_id_at(0).unwrap();
    let old_share = mine_share_on(&tree, p0, 2, 77);
    assert_eq!(old_share.context.height, 1);
    let mut blk_with_old = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], 6);
    blk_with_old.body.shares.push(old_share);
    blk_with_old.header.context.body_core_root = compute_body_root(&blk_with_old.body).unwrap();
    assert!(tree.pre_validate(&blk_with_old, None).is_err());

    // A valid share mined at height 5 on the canonical parent.
    let p4 = tree.canonical_id_at(4).unwrap();
    let valid_share = mine_share_on(&tree, p4, 2, 88);
    assert_eq!(valid_share.context.height, 5);

    // Wrong owner digest.
    let mut bad_owner_share = valid_share.clone();
    bad_owner_share.owner = 99;
    let mut blk_bad_owner = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], 6);
    blk_bad_owner.body.shares.push(bad_owner_share);
    blk_bad_owner.header.context.body_core_root = compute_body_root(&blk_bad_owner.body).unwrap();
    assert!(tree.pre_validate(&blk_bad_owner, None).is_err());

    // Wrong share target.
    let mut bad_target_share = valid_share.clone();
    bad_target_share.share_target = Target(u128::MAX - 10);
    let mut blk_bad_target = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], 6);
    blk_bad_target.body.shares.push(bad_target_share);
    blk_bad_target.header.context.body_core_root = compute_body_root(&blk_bad_target.body).unwrap();
    assert!(tree.pre_validate(&blk_bad_target, None).is_err());

    // The valid share goes through the template and registers with its owner.
    let with_share = mine_block_on(
        &tree,
        tree.tip_id(),
        1,
        &[],
        std::slice::from_ref(&valid_share),
        6,
    );
    assert_eq!(with_share.body.shares, vec![valid_share.clone()]);
    extend(&mut tree, with_share);
    assert_eq!(
        tree.tip_state().shares[&valid_share.work_id].owner,
        2,
        "share registered for its owner"
    );

    // Advance past MAX_REORG_DEPTH and check that a fork from genesis is TooDeep.
    for h in 7..=MAX_REORG_DEPTH + 6 {
        let blk = mine_block_on(&tree, tree.tip_id(), 1, &[], &[], h);
        tree.pre_validate(&blk, None).unwrap();
        tree.apply_verified_block(Arc::new(blk)).unwrap();
    }
    let deep_fork_blk = mine_block_on(&tree, tree.genesis_id(), 2, &[], &[], 999);
    assert_eq!(
        tree.pre_validate(&deep_fork_blk, None),
        Err(TreeError::TooDeep)
    );
}
