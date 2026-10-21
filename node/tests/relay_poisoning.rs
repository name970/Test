//! Relay poisoning: a peer that relays a copy of a block or job with a forged
//! signature first must not stop the genuine copy from being accepted.
//!
//! `block_id` and `tx_id` deliberately exclude signatures, so the node's relay and
//! rejection caches must key on the full signed encoding instead.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use chain::core_adapter::fw_context_to_core;
use chain::keys::unsigned;
use chain::{BlockTree, OwnerKeypair, build_template};
use futures::{SinkExt, StreamExt};
use node::cli::RunArgs;
use node::mempool::MempoolCmd;
use node::{NodeHandle, start_node};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use wire::{
    Block, DifficultyMode, GenesisConfig, JobPayload, JobTransaction, MAX_FRAME_BYTES, Target,
    WIRE_VERSION, WireMessage, codec,
};
use work::FreshWorkIds;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Every attempt seals, so the test can build valid blocks with one tile each.
fn test_genesis() -> GenesisConfig {
    let mut g = GenesisConfig::default_testnet(1_700_000_000);
    g.initial_seal_target = Target(u128::MAX);
    g.share_ratio = 1;
    g.difficulty = DifficultyMode::Fixed;
    g
}

async fn start(genesis: &GenesisConfig) -> (NodeHandle, PathBuf) {
    let dir = std::env::temp_dir().join(format!("plauthium-relay-{}", now_secs_nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let genesis_path = dir.join("genesis.json");
    std::fs::write(&genesis_path, serde_json::to_string(genesis).unwrap()).unwrap();
    let node = start_node(RunArgs {
        data_dir: dir.join("node"),
        key_file: None,
        genesis: Some(genesis_path),
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers: vec![],
        mining_threads: 0,
        no_mine: true,
        verify_threads: Some(2),
        reset: true,
    })
    .await
    .unwrap();
    (node, dir)
}

fn now_secs_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Mines the next block on the local replica `tree` for `miner`.
fn mine_next(tree: &BlockTree, miner: &Arc<OwnerKeypair>) -> Block {
    let tip = tree.tip_meta().clone();
    let tpl = build_template(
        1,
        tree.genesis(),
        tree.genesis_id(),
        tree,
        tip.height,
        &tree.tip_state(),
        &[],
        &[],
        miner,
        now_secs(),
    )
    .unwrap();
    let ctx = fw_context_to_core(&tpl.context_with_nonce(0)).unwrap();
    let inst = ctx.instance();
    let z = inst.x.product(&inst.y);
    tpl.seal(0, &FreshWorkIds::derive(&ctx, &z)).unwrap()
}

/// A raw P2P connection that has completed the handshake.
struct RawPeer {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl RawPeer {
    async fn connect(addr: SocketAddr, genesis: &GenesisConfig, tree: &BlockTree) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let codec = LengthDelimitedCodec::builder()
            .length_field_type::<u32>()
            .big_endian()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        let mut peer = RawPeer {
            framed: Framed::new(stream, codec),
        };
        peer.send(&WireMessage::Handshake {
            version: WIRE_VERSION,
            chain_id: genesis.chain_id,
            best_height: 0,
            best_block_id: tree.genesis_id(),
        })
        .await;
        let first = peer.framed.next().await.unwrap().unwrap();
        assert!(matches!(
            codec::decode::<WireMessage>(&first).unwrap(),
            WireMessage::Handshake { .. }
        ));
        peer
    }

    async fn send(&mut self, msg: &WireMessage) {
        let bytes = codec::encode(msg).unwrap();
        self.framed.send(Bytes::from(bytes)).await.unwrap();
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn pooled_jobs(node: &NodeHandle) -> Vec<JobTransaction> {
    let (tx, rx) = oneshot::channel();
    node.mempool_tx
        .send(MempoolCmd::List {
            limit: 100,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_copies_do_not_shadow_genuine_blocks_and_jobs() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    assert_eq!(tree.genesis_id(), node.status_rx.borrow().genesis_id);
    let miner = Arc::new(OwnerKeypair::from_seed(&[1; 32]));
    let height = |n: &NodeHandle| n.status_rx.borrow().tip.height;

    // 1. Same connection, back to back: a forged-signature copy of block 1 (same
    //    block_id) is still being verified when the genuine copy arrives.
    let b1 = mine_next(&tree, &miner);
    let mut forged_b1 = b1.clone();
    forged_b1.header.signature.0[0] ^= 0x01;
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forged_b1)))
        .await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(b1.clone())))
        .await;
    wait_until("block 1 despite a forged copy in flight", || {
        height(&node) == 1
    })
    .await;
    assert_eq!(node.status_rx.borrow().tip.id, b1.header.block_id);
    tree.pre_validate(&b1, None).unwrap();
    tree.apply_verified_block(Arc::new(b1)).unwrap();

    // 2. The forged copy of block 2 is rejected first (its sender is banned and
    //    dropped); the genuine copy then arrives from another peer.
    let b2 = mine_next(&tree, &miner);
    let mut forged_b2 = b2.clone();
    forged_b2.header.signature = unsigned();
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("the attacker to be registered", || {
        node.peer_manager.peer_count() >= 1
    })
    .await;
    let before = node.peer_manager.peer_count();
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forged_b2)))
        .await;
    wait_until("the forged block 2 to be rejected", || {
        node.peer_manager.peer_count() < before
    })
    .await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    honest
        .send(&WireMessage::GossipBlock(Box::new(b2.clone())))
        .await;
    wait_until("block 2 after its forged copy was rejected", || {
        height(&node) == 2
    })
    .await;
    assert_eq!(node.status_rx.borrow().tip.id, b2.header.block_id);

    // 3. Jobs: a forged-signature copy (same tx_id) is refused, and the genuine job
    //    relayed right after it still reaches the mempool.
    let client = OwnerKeypair::from_seed(&[2; 32]);
    let mut job = JobTransaction {
        client_key: client.public_key().clone(),
        queue_fee: 100_000_000,
        base_fee: 1_000_000_000,
        v_exec: 8_000_000_000,
        v_da: 500_000_000,
        v_ref: 500_000_000,
        expiry_height: 1_000,
        nonce: 42,
        payload: JobPayload::ExactDot {
            xs: vec![2.0; 512],
            ys: vec![3.0; 512],
        },
        signature: unsigned(),
    };
    client.sign_job(&mut job, &tree.genesis_id()).unwrap();
    let mut forged_job = job.clone();
    forged_job.signature.0[10] ^= 0x01;
    honest.send(&WireMessage::GossipJob(forged_job)).await;
    honest.send(&WireMessage::GossipJob(job.clone())).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pooled = pooled_jobs(&node).await;
        if let Some(found) = pooled.first() {
            assert_eq!(pooled.len(), 1);
            assert_eq!(found.signature, job.signature, "the genuine copy is pooled");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the genuine job never reached the mempool"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
