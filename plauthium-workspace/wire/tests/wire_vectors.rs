//! Wire protocol unit tests (§11 unit tests 1 and 2, Appendix B).

#![forbid(unsafe_code)]

use wire::{
    Block, BlockBody, BlockHeader, DIGEST_LEN, DOT_LEN, DifficultyMode, Digest, FeeTransition,
    FwContext, GenesisConfig, JobPayload, JobTransaction, LeaseAssignment, MAX_FRAME_BYTES,
    OwnerReward, SettlementReceipt, ShareProof, Target, WireMessage, codec,
};

fn sample_digest(seed: u8) -> Digest {
    let mut bytes = [0u8; DIGEST_LEN];
    for (i, slot) in bytes.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    Digest(bytes)
}

fn sample_context() -> FwContext {
    FwContext {
        chain_id: 0x504C_4154,
        genesis_id: sample_digest(1),
        protocol_version: 2,
        crypto_suite_id: 0,
        program_id: sample_digest(2),
        air_id: sample_digest(3),
        generator_version: 2,
        parent_canonical_id: sample_digest(4),
        stable_anchor_id: sample_digest(5),
        height: 42,
        miner_policy_digest: sample_digest(6),
        payout_policy_digest: sample_digest(7),
        attempt_nonce: 99,
        body_core_root: sample_digest(8),
        parent_derived_difficulty_context: sample_digest(9),
        time_bucket: 1_792_281_900,
    }
}

fn sample_header() -> BlockHeader {
    BlockHeader {
        context: sample_context(),
        output_root: sample_digest(10),
        work_id: sample_digest(11),
        block_id: sample_digest(12),
        share_ticket: sample_digest(13),
        seal_ticket: sample_digest(14),
        seal_target: Target(u128::MAX / 32),
        share_target: Target(u128::MAX / 8),
        miner_owner: 1,
    }
}

fn sample_job() -> JobTransaction {
    JobTransaction {
        client: sample_digest(20),
        queue_fee: 100_000_000,
        base_fee: 1_000_000_000,
        v_exec: 8_000_000_000,
        v_da: 500_000_000,
        v_ref: 500_000_000,
        expiry_height: 250,
        nonce: 7,
        payload: JobPayload::ExactDot {
            xs: vec![1.0; DOT_LEN],
            ys: vec![-2.0; DOT_LEN],
        },
    }
}

fn sample_share() -> ShareProof {
    ShareProof {
        context: sample_context(),
        owner: 2,
        output_root: sample_digest(30),
        work_id: sample_digest(31),
        share_ticket: sample_digest(32),
        share_target: Target(u128::MAX / 8),
    }
}

#[test]
fn appendix_b_encoded_sizes_and_round_trips() {
    let header = sample_header();
    let header_bytes = codec::encode(&header).unwrap();
    assert_eq!(header_bytes.len(), 992);
    assert_eq!(codec::decode::<BlockHeader>(&header_bytes).unwrap(), header);

    let job = sample_job();
    let job_bytes = codec::encode(&job).unwrap();
    assert_eq!(job_bytes.len(), 8_332);
    assert_eq!(codec::decode::<JobTransaction>(&job_bytes).unwrap(), job);

    let share = sample_share();
    let share_bytes = codec::encode(&share).unwrap();
    assert_eq!(share_bytes.len(), 848);
    assert_eq!(codec::decode::<ShareProof>(&share_bytes).unwrap(), share);

    let genesis = GenesisConfig::default_testnet(1_792_281_600);
    let gen_bytes = codec::encode(&genesis).unwrap();
    assert_eq!(codec::decode::<GenesisConfig>(&gen_bytes).unwrap(), genesis);

    let json = serde_json::to_string_pretty(&genesis).unwrap();
    assert!(json.contains("\"initial_seal_target\": \"0x07ffffffffffffffffffffffffffffff\""));
    let from_json: GenesisConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(from_json, genesis);

    let lease = LeaseAssignment {
        job_id: 123,
        share_id: sample_digest(40),
        owner: 3,
    };
    let receipt = SettlementReceipt {
        job_id: 123,
        tx_id: sample_digest(41),
        executor_owner: 3,
        admitted_at: 40,
        result: -1024,
        result_digest: sample_digest(42),
        v_exec: 8_000_000_000,
        v_da: 500_000_000,
        v_ref: 500_000_000,
    };
    let transitions = vec![
        FeeTransition::ReserveMature {
            epoch: 2,
            matured_lots: vec![1, 2],
            delta: [0, 0, 0, 0, 0, 1100, -1100, 0],
        },
        FeeTransition::FwReward {
            epoch: 2,
            amount: 500,
            delta: [500, 0, 0, 0, 0, 0, 0, -500],
        },
        FeeTransition::EpochBooked {
            epoch: 2,
            budget: 1000,
            booked: 999,
            dust: 1,
            lots: 3,
            shares: 4,
            per_owner: vec![OwnerReward {
                owner: 1,
                atoms: 999,
            }],
            delta: [0, 0, 0, 0, 0, -999, 0, 999],
        },
        FeeTransition::ResultSettle {
            job_id: 123,
            v_exec: 80,
            v_da: 5,
            v_ref: 5,
            delta: [90, 0, -90, 0, 0, 0, 0, 0],
        },
        FeeTransition::EscrowFundAndQueue {
            job_id: 123,
            deposit: 100,
            queue_fee: 1,
            delta: [-101, 0, 100, 0, 0, 0, 1, 0],
        },
        FeeTransition::JobAdmitAndLease {
            job_id: 123,
            base_fee: 10,
            owner: 3,
            delta: [0, 0, -10, 0, 0, 0, 10, 0],
        },
    ];
    for ft in &transitions {
        assert_eq!(ft.delta().iter().sum::<i128>(), 0);
    }

    let body = BlockBody {
        jobs: vec![job.clone()],
        shares: vec![share],
        leases: vec![lease],
        receipts: vec![receipt],
        fee_transitions: transitions,
        ledger_digest: sample_digest(50),
    };
    let block = Block { header, body };

    for msg in [
        WireMessage::Handshake {
            version: 1,
            chain_id: 0x504C_4154,
            best_height: 42,
            best_block_id: sample_digest(60),
        },
        WireMessage::GossipBlock(Box::new(block.clone())),
        WireMessage::GossipJob(job),
        WireMessage::SyncRequest { from_height: 10 },
        WireMessage::SyncResponse {
            blocks: vec![block],
        },
    ] {
        let encoded = codec::encode(&msg).unwrap();
        let decoded: WireMessage = codec::decode(&encoded).unwrap();
        assert_eq!(decoded, msg);

        // A trailing byte must be rejected.
        let mut with_trailing = encoded.clone();
        with_trailing.push(0xff);
        assert!(codec::decode::<WireMessage>(&with_trailing).is_err());
        // So must a truncated frame.
        assert!(codec::decode::<WireMessage>(&encoded[..encoded.len() - 1]).is_err());
    }
}

#[test]
fn oversized_payload_is_rejected_by_codec_limit() {
    let huge = vec![0u8; MAX_FRAME_BYTES + 1024];
    assert!(codec::encode(&huge).is_err());
    assert!(codec::decode::<Vec<u8>>(&huge).is_err());
    // A length prefix that claims more than the limit fails before allocating.
    let mut lying = codec::encode(&vec![0u8; 4]).unwrap();
    lying[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(codec::decode::<Vec<u8>>(&lying).is_err());
}

#[test]
fn difficulty_mode_round_trip() {
    for mode in [
        DifficultyMode::Fixed,
        DifficultyMode::Windowed { window: 32 },
    ] {
        let bytes = codec::encode(&mode).unwrap();
        assert_eq!(codec::decode::<DifficultyMode>(&bytes).unwrap(), mode);
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(serde_json::from_str::<DifficultyMode>(&json).unwrap(), mode);
    }
}
