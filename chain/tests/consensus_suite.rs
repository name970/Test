//! Consensus and STF tests (§11 unit tests 3–7).

#![forbid(unsafe_code)]

use std::sync::Arc;

use chain::core_adapter::fw_context_to_core;
use chain::keys::unsigned;
use chain::{
    BlockTree, OwnerKeypair, SignatureError, StfError, TreeError, VerifiedShare, WorkError,
    apply_body, build_template, compute_body_root, compute_difficulty_context, compute_job_ids,
    verify_ml_dsa_87,
};
use ledger::{LedgerError, StateCategory};
use primitives::{Atoms, TOTAL_SUPPLY};
use wire::{
    Block, DIGEST_LEN, DifficultyMode, Digest, FeeTransition, FwContext, GenesisConfig, JobPayload,
    JobTransaction, MAX_REORG_DEPTH, ShareProof, Target,
};
use work::FreshWorkIds;

/// Test owner `i`, derived from a fixed seed so runs are reproducible.
fn key(i: u8) -> Arc<OwnerKeypair> {
    Arc::new(OwnerKeypair::from_seed(&[i; 32]))
}

/// A share proof for `apply_body`-level tests: synthetic work, a real owner signature.
/// `VerifiedShare::verify` is the only way to obtain a `VerifiedShare`.
fn synthetic_share(owner: &OwnerKeypair, work_id: Digest, height: u64) -> VerifiedShare {
    let zero = Digest::ZERO;
    let mut share = ShareProof {
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
        work_id,
        share_ticket: zero,
        share_target: Target(1 << 125),
        signature: unsigned(),
    };
    owner.sign_share(&mut share).unwrap();
    VerifiedShare::verify(&share).unwrap()
}

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
    genesis_id: &Digest,
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
    let client = key(100);
    let mut tx = JobTransaction {
        client_key: client.public_key().clone(),
        queue_fee,
        base_fee,
        v_exec,
        v_da: 50_000_000,
        v_ref: 50_000_000,
        expiry_height,
        nonce: seed,
        payload: JobPayload::ExactDot { xs, ys },
        signature: unsigned(),
    };
    client.sign_job(&mut tx, genesis_id).unwrap();
    tx
}

fn template_on(
    tree: &BlockTree,
    parent_id: Digest,
    owner: &Arc<OwnerKeypair>,
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
    owner: &Arc<OwnerKeypair>,
    mempool: &[JobTransaction],
    shares: &[ShareProof],
    nonce: u64,
) -> Block {
    let tpl = template_on(tree, parent_id, owner, mempool, shares);
    tpl.seal(nonce, &work_ids(&tpl.context_with_nonce(nonce)))
        .unwrap()
}

fn mine_share_on(
    tree: &BlockTree,
    parent_id: Digest,
    owner: &Arc<OwnerKeypair>,
    nonce: u64,
) -> ShareProof {
    let tpl = template_on(tree, parent_id, owner, &[], &[]);
    tpl.share(nonce, &work_ids(&tpl.context_with_nonce(nonce)))
        .unwrap()
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
    let (_, ledger, genesis_id) = chain::validate_genesis(&g).unwrap();
    let mut state = chain::NodeState::genesis(ledger);
    let owners: Vec<Arc<OwnerKeypair>> = (1..=5).map(key).collect();

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
        let share_owner = &owners[(next_u64() % 5) as usize];
        let mut wid = [0u8; DIGEST_LEN];
        wid[0..8].copy_from_slice(&h.to_be_bytes());
        wid[8..16].copy_from_slice(&next_u64().to_be_bytes());
        let shares = vec![synthetic_share(
            share_owner,
            Digest(wid),
            h.saturating_sub(1).max(1),
        )];

        let mut jobs = Vec::new();
        // The epoch-1 booking is paid at h = 10; from then on U has spendable rewards.
        if h >= 10 {
            let count = (next_u64() % 4) as usize;
            for j in 0..count {
                let seed = h * 100 + j as u64;
                let q_fee = ((next_u64() % 5) + 1) * 10_000_000;
                let b_fee = ((next_u64() % 10) + 1) * 100_000_000;
                let v_exec = ((next_u64() % 20) + 1) * 100_000_000;
                let job = make_job(seed, q_fee, b_fee, v_exec, h + 50, &genesis_id);
                // Stateless validation and compute_job_ids agree on tx_id.
                assert_eq!(
                    chain::validate_job_stateless(&job).unwrap().0,
                    compute_job_ids(&job).unwrap().0
                );
                jobs.push(job);
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
    let miner = key(1);

    // Advance to height 10, where the epoch-1 booking is paid into U.
    for h in 1..=10 {
        let blk = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], h);
        extend(&mut tree, blk);
    }

    let job = make_job(
        555,
        100_000_000,
        1_000_000_000,
        8_000_000_000,
        100,
        &tree.genesis_id(),
    );
    let honest = mine_block_on(&tree, tree.tip_id(), &miner, &[job], &[], 11);
    assert_eq!(honest.body.jobs.len(), 1);
    assert!(honest.body.fee_transitions.len() >= 2);

    // Derived fields: rejected by apply_verified_block even in a block that is otherwise
    // fully valid (body recommitted, work re-sealed, header re-signed), so the failure
    // is the derived-field check itself.
    let rejects_with = |tree: &mut BlockTree, mut block: Block, expected: &str| {
        reseal(&mut block, &miner);
        BlockTree::verify_stage_a(&block).unwrap();
        match tree.apply_verified_block(Arc::new(block)) {
            Err(TreeError::Invalid(reason)) => {
                assert!(reason.contains(expected), "{reason:?} lacks {expected:?}")
            }
            other => panic!("expected {expected:?}, got {other:?}"),
        }
    };
    let mut bad_digest = honest.clone();
    bad_digest.body.ledger_digest.0[0] ^= 0x01;
    rejects_with(&mut tree, bad_digest, "derived ledger_digest mismatch");

    let mut bad_ft_order = honest.clone();
    bad_ft_order.body.fee_transitions.reverse();
    rejects_with(&mut tree, bad_ft_order, "derived fee_transitions mismatch");

    let mut bad_leases = honest.clone();
    bad_leases.body.leases[0].owner = key(9).key_id();
    rejects_with(&mut tree, bad_leases, "derived leases mismatch");

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

    // Another key re-signing the header cannot claim the block.
    let mut bad_owner = honest.clone();
    key(2).sign_header(&mut bad_owner.header).unwrap();
    assert!(tree.pre_validate(&bad_owner, None).is_err());

    // One operand changed without updating body_core_root.
    let mut bad_operand = honest.clone();
    let JobPayload::ExactDot { ref mut xs, .. } = bad_operand.body.jobs[0].payload;
    xs[0] += 1.0;
    assert!(tree.pre_validate(&bad_operand, None).is_err());

    // Stage A: a forged nonce no longer matches the claimed work, even when the miner
    // re-signs the header (so the work check, not the signature, rejects it).
    let mut bad_nonce = honest.clone();
    bad_nonce.header.context.attempt_nonce += 1;
    miner.sign_header(&mut bad_nonce.header).unwrap();
    BlockTree::verify_signatures(&bad_nonce).unwrap();
    assert!(matches!(
        BlockTree::verify_stage_a(&bad_nonce),
        Err(WorkError::OutputRootMismatch)
    ));

    // The honest block still applies.
    extend(&mut tree, honest);
    assert_eq!(tree.tip_meta().height, 11);
}

/// Unit test 5 (§11): job flow, `FounderTrancheLocked`, and `R_l -> R_a` as two lots.
#[test]
fn job_fee_lock_and_two_lot_maturation_flow() {
    let g = fast_genesis(); // blocks_per_epoch = 5, maturation_delay = 2, settle_delay = 2
    let mut tree = BlockTree::new(g.clone()).unwrap();
    let miner = key(1);
    let job = make_job(
        999,
        100_000_000,
        1_000_000_000,
        8_000_000_000,
        100,
        &tree.genesis_id(),
    );

    // Funding before any reward reached U fails on the locked founder tranches.
    let sh = synthetic_share(&miner, Digest([1u8; DIGEST_LEN]), 1);
    let (s1, _) = apply_body(&tree.tip_state(), &g, 1, &[], &[sh]).unwrap();
    let res = apply_body(&s1, &g, 2, std::slice::from_ref(&job), &[]);
    assert!(matches!(
        res,
        Err(StfError::Ledger(LedgerError::FounderTrancheLocked { .. }))
    ));

    // Blocks 1..=9: the job stays out of the template.
    for h in 1..=9 {
        let blk = mine_block_on(
            &tree,
            tree.tip_id(),
            &miner,
            std::slice::from_ref(&job),
            &[],
            h,
        );
        assert!(blk.body.jobs.is_empty(), "h = {h}");
        extend(&mut tree, blk);
    }

    // h = 10 (epoch 2): the epoch-1 booking is paid into U and the job is admitted;
    // R_l holds queue_fee + base_fee = 1.1 * 10^9 atoms as two locked fees.
    let b10 = mine_block_on(
        &tree,
        tree.tip_id(),
        &miner,
        std::slice::from_ref(&job),
        &[],
        10,
    );
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
        let blk = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], h);
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
    let (miner, sharer) = (key(1), key(2));

    for h in 1..=5 {
        let blk = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], h);
        extend(&mut tree, blk);
    }

    // A share mined at height 1 is outside the window (3) at height 6.
    let p0 = tree.canonical_id_at(0).unwrap();
    let old_share = mine_share_on(&tree, p0, &sharer, 77);
    assert_eq!(old_share.context.height, 1);
    let mut blk_with_old = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], 6);
    blk_with_old.body.shares.push(old_share);
    blk_with_old.header.context.body_core_root = compute_body_root(&blk_with_old.body).unwrap();
    assert!(tree.pre_validate(&blk_with_old, None).is_err());

    // A valid share mined at height 5 on the canonical parent.
    let p4 = tree.canonical_id_at(4).unwrap();
    let valid_share = mine_share_on(&tree, p4, &sharer, 88);
    assert_eq!(valid_share.context.height, 5);

    // Another owner re-signing the share: its key id does not match the policy digests.
    let mut bad_owner_share = valid_share.clone();
    key(99).sign_share(&mut bad_owner_share).unwrap();
    let mut blk_bad_owner = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], 6);
    blk_bad_owner.body.shares.push(bad_owner_share);
    blk_bad_owner.header.context.body_core_root = compute_body_root(&blk_bad_owner.body).unwrap();
    assert!(tree.pre_validate(&blk_bad_owner, None).is_err());

    // Wrong share target.
    let mut bad_target_share = valid_share.clone();
    bad_target_share.share_target = Target(u128::MAX - 10);
    let mut blk_bad_target = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], 6);
    blk_bad_target.body.shares.push(bad_target_share);
    blk_bad_target.header.context.body_core_root = compute_body_root(&blk_bad_target.body).unwrap();
    assert!(tree.pre_validate(&blk_bad_target, None).is_err());

    // The valid share goes through the template and registers with its owner.
    let with_share = mine_block_on(
        &tree,
        tree.tip_id(),
        &miner,
        &[],
        std::slice::from_ref(&valid_share),
        6,
    );
    assert_eq!(with_share.body.shares, vec![valid_share.clone()]);
    extend(&mut tree, with_share);
    let registered = tree.tip_state().shares[&valid_share.work_id].owner;
    assert_eq!(
        registered.key_id(),
        sharer.key_id(),
        "share registered for its owner"
    );
    assert_eq!(registered.owner_id(), sharer.owner_id());

    // Advance past MAX_REORG_DEPTH and check that a fork from genesis is TooDeep.
    for h in 7..=MAX_REORG_DEPTH + 6 {
        let blk = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], h);
        tree.pre_validate(&blk, None).unwrap();
        tree.apply_verified_block(Arc::new(blk)).unwrap();
    }
    let deep_fork_blk = mine_block_on(&tree, tree.genesis_id(), &sharer, &[], &[], 999);
    assert_eq!(
        tree.pre_validate(&deep_fork_blk, None),
        Err(TreeError::TooDeep)
    );
}

/// Recommits a hand-edited body and re-seals the block for `miner`: the attacker's
/// path to a self-consistent block (every attempt seals under `fast_genesis`).
fn reseal(block: &mut Block, miner: &OwnerKeypair) {
    block.header.context.body_core_root = compute_body_root(&block.body).unwrap();
    let ids = chain::core_adapter::fresh_work_ids_to_digests(&work_ids(&block.header.context));
    block.header.output_root = ids.output_root;
    block.header.work_id = ids.work_id;
    block.header.block_id = ids.block_id;
    block.header.share_ticket = ids.share_ticket;
    block.header.seal_ticket = ids.seal_ticket;
    miner.sign_header(&mut block.header).unwrap();
}

/// ML-DSA-87 rules: a forged header, share or job signature rejects the block in
/// Stage A and in `apply_verified_block`; a job signed for another chain or under
/// another key does not verify; `tx_id` does not depend on the signature.
#[test]
fn signatures_bind_headers_shares_and_jobs() {
    let g = fast_genesis();
    let mut tree = BlockTree::new(g).unwrap();
    let (miner, sharer) = (key(1), key(2));
    for h in 1..=10 {
        let blk = mine_block_on(&tree, tree.tip_id(), &miner, &[], &[], h);
        extend(&mut tree, blk);
    }
    let genesis_id = tree.genesis_id();
    // Mined at height 10 (on block 9), so includable at height 11.
    let share = mine_share_on(&tree, tree.canonical_id_at(9).unwrap(), &sharer, 31);
    let job = make_job(
        4242,
        100_000_000,
        1_000_000_000,
        8_000_000_000,
        100,
        &genesis_id,
    );
    let honest = mine_block_on(
        &tree,
        tree.tip_id(),
        &miner,
        std::slice::from_ref(&job),
        std::slice::from_ref(&share),
        11,
    );
    assert_eq!((honest.body.jobs.len(), honest.body.shares.len()), (1, 1));
    BlockTree::verify_signatures(&honest).unwrap();

    // Header: one flipped signature byte. Stage B ignores signatures; Stage A and
    // apply_verified_block (which also serves trusted disk replay) reject it.
    let mut forged_header = honest.clone();
    forged_header.header.signature.0[100] ^= 0x01;
    tree.pre_validate(&forged_header, None).unwrap();
    assert!(matches!(
        BlockTree::verify_stage_a(&forged_header),
        Err(WorkError::Signature("header", _))
    ));
    assert!(tree.apply_verified_block(Arc::new(forged_header)).is_err());

    // Share: a forged owner signature inside an otherwise valid, re-sealed block.
    let mut forged_share = honest.clone();
    forged_share.body.shares[0].signature.0[7] ^= 0x80;
    reseal(&mut forged_share, &miner);
    tree.pre_validate(&forged_share, None).unwrap();
    assert!(matches!(
        BlockTree::verify_stage_a(&forged_share),
        Err(WorkError::Signature("share", _))
    ));
    assert!(tree.apply_verified_block(Arc::new(forged_share)).is_err());

    // A template never includes a share whose signature fails.
    let mut bad_sig_share = share.clone();
    bad_sig_share.signature.0[0] ^= 0x01;
    let tpl = template_on(
        &tree,
        tree.tip_id(),
        &miner,
        &[],
        std::slice::from_ref(&bad_sig_share),
    );
    assert!(tpl.body.shares.is_empty());

    // Job: signed for another genesis, or carrying another client's key.
    let other_genesis = Digest([0xee; DIGEST_LEN]);
    let mut replayed = job.clone();
    key(100).sign_job(&mut replayed, &other_genesis).unwrap();
    assert!(chain::verify_job_client(&replayed, &other_genesis).is_ok());
    assert_eq!(
        chain::verify_job_client(&replayed, &genesis_id),
        Err(SignatureError::Invalid)
    );
    let mut wrong_job = honest.clone();
    wrong_job.body.jobs[0] = replayed;
    reseal(&mut wrong_job, &miner);
    tree.pre_validate(&wrong_job, None).unwrap();
    assert!(matches!(
        BlockTree::verify_stage_a(&wrong_job),
        Err(WorkError::Signature("job", SignatureError::Invalid))
    ));

    let mut swapped_key = job.clone();
    swapped_key.client_key = key(101).public_key().clone();
    assert!(chain::verify_job_client(&swapped_key, &genesis_id).is_err());

    // Hedged signing: re-signing yields new signature bytes but the same tx_id.
    let mut resigned = job.clone();
    key(100).sign_job(&mut resigned, &genesis_id).unwrap();
    assert_ne!(resigned.signature, job.signature);
    assert_eq!(
        compute_job_ids(&resigned).unwrap(),
        compute_job_ids(&job).unwrap()
    );
    assert!(chain::verify_job_client(&resigned, &genesis_id).is_ok());

    // The honest block applies; both owners enter the state through verified keys.
    extend(&mut tree, honest.clone());
    let state = tree.tip_state();
    assert_eq!(state.shares[&share.work_id].owner.key_id(), sharer.key_id());
    assert_eq!(
        state.shares[&honest.header.work_id].owner.key_id(),
        miner.key_id(),
        "the header share is registered for the miner"
    );
    assert_eq!(tree.tip_meta().miner_key_id, miner.key_id());
}

/// ML-DSA-87 interoperates with an independent FIPS 204 implementation: the
/// vectors come from OpenSSL 4.0 (via Python `cryptography`), with the seed
/// `00 01 .. 1f` and the context string `PLAU/TX/v2`.
#[test]
fn ml_dsa_87_matches_an_independent_implementation() {
    let seed: [u8; 32] = core::array::from_fn(|i| i as u8);
    let kp = OwnerKeypair::from_seed(&seed);
    // FIPS 204 KeyGen_internal from the same seed gives the same public key, so the
    // same key id: H(PLAU/TESTNET/OWNER/v1, pk) computed by Python hashlib.
    assert_eq!(
        kp.key_id().to_hex(),
        "de663ef45925f1a8520bd9b8a796d0cb3adea342369e21af7a1146dd114f5cba\
         ac4e33a674ee5fb2e711c48b973a8a2a33fc6d15052a521a5a4d0e339c39755b"
    );

    // An OpenSSL signature over the digest 0x42 * 64 verifies here.
    let sig_hex = include_str!("vectors/ml_dsa_87_openssl.sig.hex").trim();
    let sig = wire::Signature::from_hex(sig_hex).unwrap();
    let digest = Digest([0x42; DIGEST_LEN]);
    verify_ml_dsa_87(kp.public_key(), &digest, &sig).unwrap();
    // Twice: the second check uses the cached decoded key and must agree.
    verify_ml_dsa_87(kp.public_key(), &digest, &sig).unwrap();

    // Any other digest, key or corrupted signature fails.
    assert_eq!(
        verify_ml_dsa_87(kp.public_key(), &Digest([0x43; DIGEST_LEN]), &sig),
        Err(SignatureError::Invalid)
    );
    assert!(verify_ml_dsa_87(key(5).public_key(), &digest, &sig).is_err());
    let mut corrupted = sig.clone();
    corrupted.0[4_000] ^= 0x10;
    assert!(verify_ml_dsa_87(kp.public_key(), &digest, &corrupted).is_err());
    assert!(verify_ml_dsa_87(kp.public_key(), &digest, &unsigned()).is_err());
}
