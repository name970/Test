//! Relay poisoning and share gossip over raw P2P connections.
//!
//! A peer that relays a copy of a block, job or share with a forged signature (or a
//! block with a forged body) first must not stop the genuine copy from being accepted:
//! `block_id`, `tx_id` and `work_id` deliberately exclude signatures, so the node's
//! relay and rejection caches key on the full signed encoding instead. Gossiped shares are pooled and relayed
//! only once verified, and a peer sending an invalid one is banned.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use chain::core_adapter::fw_context_to_core;
use chain::keys::unsigned;
use chain::{BlockTree, OwnerKeypair, SignedShare, build_template};
use futures::{SinkExt, StreamExt};
use node::chain_actor::{ChainCmd, ImportAck, Origin, Source};
use node::cli::RunArgs;
use node::mempool::MempoolCmd;
use node::{NodeHandle, start_node};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use wire::{
    Block, DifficultyMode, Digest, GenesisConfig, JobPayload, JobTransaction, MAX_FRAME_BYTES,
    ShareProof, Target, WIRE_VERSION, WireMessage, codec,
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
        mine_when_synced: false,
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
    mine_on(tree, tree.tip_id(), miner, &[])
}

/// Mines a block on `parent` (any branch of the replica) including `pool` shares.
fn mine_on(
    tree: &BlockTree,
    parent: Digest,
    miner: &Arc<OwnerKeypair>,
    pool: &[ShareProof],
) -> Block {
    let tpl = template_on(tree, parent, miner, pool);
    let ctx = fw_context_to_core(&tpl.context_with_nonce(0)).unwrap();
    let inst = ctx.instance();
    let z = inst.x.product(&inst.y);
    tpl.seal(0, &FreshWorkIds::derive(&ctx, &z)).unwrap()
}

/// Mines a share for `owner` with `nonce` on top of the replica's tip.
fn mine_share(tree: &BlockTree, owner: &Arc<OwnerKeypair>, nonce: u64) -> ShareProof {
    let tpl = template_on(tree, tree.tip_id(), owner, &[]);
    let ctx = fw_context_to_core(&tpl.context_with_nonce(nonce)).unwrap();
    let inst = ctx.instance();
    let z = inst.x.product(&inst.y);
    tpl.share(nonce, &FreshWorkIds::derive(&ctx, &z)).unwrap()
}

fn template_on(
    tree: &BlockTree,
    parent: Digest,
    miner: &Arc<OwnerKeypair>,
    pool: &[ShareProof],
) -> chain::Template {
    template_on_at(tree, parent, miner, pool, now_secs())
}

/// A template whose time bucket is `now` (1-second buckets).
fn template_on_at(
    tree: &BlockTree,
    parent: Digest,
    miner: &Arc<OwnerKeypair>,
    pool: &[ShareProof],
    now: u64,
) -> chain::Template {
    let view = tree.ancestor_view(parent).unwrap();
    let meta = tree.get_meta(&parent).unwrap();
    let pool: Vec<SignedShare> = pool
        .iter()
        .map(|sh| SignedShare::verify(sh.clone()).unwrap())
        .collect();
    build_template(
        1,
        tree.genesis(),
        tree.genesis_id(),
        &view,
        meta.height,
        &tree.get_state(&parent).unwrap(),
        &[],
        &pool,
        miner,
        now,
    )
    .unwrap()
}

/// `count` blocks on `parent` that pass Stage B but fail Stage A: each claims the work
/// outputs of another attempt (cheap to make, a full tile to check).
fn bogus_blocks(
    tree: &BlockTree,
    parent: Digest,
    miner: &Arc<OwnerKeypair>,
    count: u64,
) -> Vec<Block> {
    let tpl = template_on(tree, parent, miner, &[]);
    let ctx0 = fw_context_to_core(&tpl.context_with_nonce(0)).unwrap();
    let inst = ctx0.instance();
    let z0 = inst.x.product(&inst.y);
    (1..=count)
        .map(|nonce| {
            let ctx = fw_context_to_core(&tpl.context_with_nonce(nonce)).unwrap();
            tpl.seal(nonce, &FreshWorkIds::derive(&ctx, &z0)).unwrap()
        })
        .collect()
}

/// Validates and applies `block` on the local replica.
fn extend(tree: &mut BlockTree, block: &Block) {
    tree.pre_validate(block, None).unwrap();
    tree.apply_verified_block(Arc::new(block.clone())).unwrap();
}

/// A raw P2P connection that has completed the handshake.
struct RawPeer {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl RawPeer {
    async fn connect(addr: SocketAddr, genesis: &GenesisConfig, tree: &BlockTree) -> Self {
        Self::connect_claiming(addr, genesis, tree, 0, tree.genesis_id()).await
    }

    /// Connects with a Handshake that claims this tip.
    async fn connect_claiming(
        addr: SocketAddr,
        genesis: &GenesisConfig,
        tree: &BlockTree,
        best_height: u64,
        best_block_id: Digest,
    ) -> Self {
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
            genesis_id: tree.genesis_id(),
            node_nonce: 7,
            best_height,
            best_block_id,
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

    /// The next frame within `wait`, or `None`.
    async fn recv(&mut self, wait: Duration) -> Option<WireMessage> {
        let frame = tokio::time::timeout(wait, self.framed.next())
            .await
            .ok()??;
        Some(codec::decode(&frame.unwrap()).unwrap())
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
    let peers = || node.peer_manager.peer_count();

    // 1. A forged-signature copy of block 1 (same block_id) is still being verified
    //    when the genuine copy arrives from an honest peer.
    let b1 = mine_next(&tree, &miner);
    let mut forged_b1 = b1.clone();
    forged_b1.header.signature.0[0] ^= 0x01;
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut first_honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || peers() == 2).await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forged_b1)))
        .await;
    first_honest
        .send(&WireMessage::GossipBlock(Box::new(b1.clone())))
        .await;
    wait_until("block 1 despite a forged copy in flight", || {
        height(&node) == 1
    })
    .await;
    assert_eq!(node.status_rx.borrow().tip.id, b1.header.block_id);
    wait_until("the first attacker banned", || peers() == 1).await;
    extend(&mut tree, &b1);

    // 2. The forged copy of block 2 is rejected first (its sender is banned and
    //    dropped); the genuine copy then arrives from another peer.
    let b2 = mine_next(&tree, &miner);
    let mut forged_b2 = b2.clone();
    forged_b2.header.signature = unsigned();
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("the attacker to be registered", || peers() == 2).await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forged_b2)))
        .await;
    wait_until("the forged block 2 to be rejected", || peers() == 1).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    honest
        .send(&WireMessage::GossipBlock(Box::new(b2.clone())))
        .await;
    wait_until("block 2 after its forged copy was rejected", || {
        height(&node) == 2
    })
    .await;
    assert_eq!(node.status_rx.borrow().tip.id, b2.header.block_id);
    extend(&mut tree, &b2);

    // 3. Genuine headers with forged bodies. A copy of block 4 arrives while block 3 is
    //    unknown and is parked as an orphan; the genuine block 4 must still be parked
    //    beside it. A copy of block 3 fails its body_core_root check at once (its
    //    sender is banned), and the genuine block 3 must still be accepted.
    let b3 = mine_next(&tree, &miner);
    let mut tree3 = BlockTree::new(genesis.clone()).unwrap();
    extend(&mut tree3, &b1);
    extend(&mut tree3, &b2);
    extend(&mut tree3, &b3);
    let b4 = mine_next(&tree3, &miner);
    let forge_body = |b: &Block| {
        let mut forged = b.clone();
        forged.body.ledger_digest.0[0] ^= 0x01;
        forged
    };
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("the attacker beside both honest peers", || peers() == 3).await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forge_body(&b4))))
        .await;
    // The orphan makes the node ask its sender for the missing blocks.
    loop {
        match attacker.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncRequest { .. }) => break,
            Some(_) => continue,
            None => panic!("the forged orphan was never processed"),
        }
    }
    honest
        .send(&WireMessage::GossipBlock(Box::new(b4.clone())))
        .await;
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forge_body(&b3))))
        .await;
    wait_until("the forged-body sender banned", || peers() == 2).await;
    honest
        .send(&WireMessage::GossipBlock(Box::new(b3.clone())))
        .await;
    wait_until("blocks 3 and 4 despite forged bodies", || {
        height(&node) == 4
    })
    .await;
    assert_eq!(node.status_rx.borrow().tip.id, b4.header.block_id);

    // 4. Jobs: a forged-signature copy (same tx_id) is refused, and the genuine job
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

/// Gossiped shares, one path per step: a forged copy gets its sender banned (after
/// the signature check) without shadowing the genuine share, which is pooled and
/// relayed; a malformed share gets its sender banned at once; a share that arrives
/// while its parent block is in Stage A waits for it; a share whose parent is unknown
/// is ignored without penalty; a validly signed share with bogus work gets its sender
/// banned and is never relayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossiped_shares_are_verified_relayed_and_invalid_ones_banned() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let owner = Arc::new(OwnerKeypair::from_seed(&[3; 32]));
    let miner = Arc::new(OwnerKeypair::from_seed(&[4; 32]));
    let pooled = |n: &NodeHandle| n.status_rx.borrow().share_pool;
    let peers = || node.peer_manager.peer_count();
    let closed = |end: Result<Option<Result<bytes::BytesMut, std::io::Error>>, _>| {
        matches!(end, Ok(None) | Ok(Some(Err(_))))
    };

    let mut listener = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut sender = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("three peers registered", || peers() == 3).await;

    // 1. A forged-signature copy (same work_id) gets the attacker banned and dropped...
    let share = mine_share(&tree, &owner, 11);
    let mut forged = share.clone();
    forged.signature.0[42] ^= 0x01;
    attacker
        .send(&WireMessage::GossipShare(Box::new(forged)))
        .await;
    wait_until("the attacker banned", || peers() == 2).await;
    let end = tokio::time::timeout(Duration::from_secs(5), attacker.framed.next()).await;
    assert!(
        closed(end),
        "the node closes the banned attacker's connection"
    );

    // 2. ...and the genuine share is still pooled and relayed.
    sender
        .send(&WireMessage::GossipShare(Box::new(share.clone())))
        .await;
    match listener.recv(Duration::from_secs(30)).await {
        Some(WireMessage::GossipShare(relayed)) => {
            assert_eq!(*relayed, share, "only the genuine share is relayed")
        }
        other => panic!("expected the relayed share, got {other:?}"),
    }
    wait_until("the share in the pool", || pooled(&node) == 1).await;

    // 3. A malformed share (wrong share_target, validly signed) is caught by the
    //    no-work checks: banned at once, no tile recomputed.
    let mut wrong_target = mine_share(&tree, &owner, 12);
    wrong_target.share_target = Target(u128::MAX - 1);
    owner.sign_share(&mut wrong_target).unwrap();
    let mut second_attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("the second attacker registered", || peers() == 3).await;
    second_attacker
        .send(&WireMessage::GossipShare(Box::new(wrong_target)))
        .await;
    let end = tokio::time::timeout(Duration::from_secs(5), second_attacker.framed.next()).await;
    assert!(
        closed(end),
        "a malformed share closes the connection at once"
    );
    wait_until("the second attacker banned", || peers() == 2).await;

    // 4. Block 1, and right behind it on the same connection a share mined on it: the
    //    share waits while block 1 is in Stage A, then is verified, pooled, relayed.
    let p1 = mine_next(&tree, &miner);
    extend(&mut tree, &p1);
    let on_p1 = mine_share(&tree, &owner, 13);
    sender
        .send(&WireMessage::GossipBlock(Box::new(p1.clone())))
        .await;
    sender
        .send(&WireMessage::GossipShare(Box::new(on_p1.clone())))
        .await;
    loop {
        match listener.recv(Duration::from_secs(30)).await {
            Some(WireMessage::GossipBlock(b)) => assert_eq!(b.header.block_id, p1.header.block_id),
            Some(WireMessage::GossipShare(relayed)) => {
                assert_eq!(*relayed, on_p1);
                break;
            }
            other => panic!("expected block 1 and its share, got {other:?}"),
        }
    }
    wait_until("both shares pooled", || pooled(&node) == 2).await;

    // 5. A share with an unknown parent is ignored: no relay, no ban.
    let mut orphan = mine_share(&tree, &owner, 14);
    orphan.context.parent_canonical_id = Digest([9; 64]);
    owner.sign_share(&mut orphan).unwrap();
    sender
        .send(&WireMessage::GossipShare(Box::new(orphan)))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(peers(), 2, "no penalty for an orphan");

    // 6. Validly signed but with a ticket its work does not produce: the work check
    //    fails, the sender is banned and the share is neither pooled nor relayed.
    let mut bogus = mine_share(&tree, &owner, 15);
    bogus.share_ticket = Digest::ZERO;
    owner.sign_share(&mut bogus).unwrap();
    sender
        .send(&WireMessage::GossipShare(Box::new(bogus)))
        .await;
    wait_until("the sender of the bogus share banned", || peers() == 1).await;
    assert_eq!(pooled(&node), 2);
    assert!(
        listener.recv(Duration::from_millis(500)).await.is_none(),
        "nothing else is relayed"
    );

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A share parked behind a bad copy of its parent is dropped when that copy fails
/// Stage A, without being remembered as seen: relayed again once the genuine parent
/// is in, it is pooled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_share_parked_behind_a_bad_parent_copy_can_return() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let owner = Arc::new(OwnerKeypair::from_seed(&[11; 32]));
    let miner = Arc::new(OwnerKeypair::from_seed(&[12; 32]));
    let pooled = |n: &NodeHandle| n.status_rx.borrow().share_pool;
    let peers = || node.peer_manager.peer_count();

    // The bad copy keeps block 1's block_id and a valid signature (its miner signed
    // it), so it fails only at its tile recomputation, well after the share arrives.
    let p1 = mine_next(&tree, &miner);
    let mut forged_p1 = p1.clone();
    forged_p1.header.seal_ticket.0[0] ^= 0x01;
    miner.sign_header(&mut forged_p1.header).unwrap();
    extend(&mut tree, &p1);
    let on_p1 = mine_share(&tree, &owner, 51);

    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || peers() == 2).await;
    // The share follows the bad copy, so it parks while that copy is in Stage A; the
    // copy fails and the share is dropped with it.
    attacker
        .send(&WireMessage::GossipBlock(Box::new(forged_p1)))
        .await;
    attacker
        .send(&WireMessage::GossipShare(Box::new(on_p1.clone())))
        .await;
    wait_until("the attacker banned", || peers() == 1).await;
    assert_eq!(pooled(&node), 0);

    honest
        .send(&WireMessage::GossipBlock(Box::new(p1.clone())))
        .await;
    wait_until("the genuine block 1", || {
        node.status_rx.borrow().tip.id == p1.header.block_id
    })
    .await;
    honest
        .send(&WireMessage::GossipShare(Box::new(on_p1)))
        .await;
    wait_until("the share pooled once relayed again", || pooled(&node) == 1).await;

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reorg returns the shares of the abandoned blocks to the pool, so they can still
/// be included on the new branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shares_of_abandoned_blocks_return_to_the_pool() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let (miner_a, miner_b, owner) = (
        Arc::new(OwnerKeypair::from_seed(&[5; 32])),
        Arc::new(OwnerKeypair::from_seed(&[6; 32])),
        Arc::new(OwnerKeypair::from_seed(&[7; 32])),
    );
    let genesis_id = tree.genesis_id();
    let pooled = |n: &NodeHandle| n.status_rx.borrow().share_pool;
    let tip = |n: &NodeHandle| n.status_rx.borrow().tip.id;

    // Branch A: a1, then a2 including a share mined at height 1.
    let share = mine_share(&tree, &owner, 21);
    let a1 = mine_on(&tree, genesis_id, &miner_a, &[]);
    extend(&mut tree, &a1);
    let a2 = mine_on(
        &tree,
        a1.header.block_id,
        &miner_a,
        std::slice::from_ref(&share),
    );
    assert_eq!(a2.body.shares, vec![share.clone()]);
    extend(&mut tree, &a2);
    // Branch B: three blocks from genesis without the share, so it is heavier.
    let b1 = mine_on(&tree, genesis_id, &miner_b, &[]);
    extend(&mut tree, &b1);
    let b2 = mine_on(&tree, b1.header.block_id, &miner_b, &[]);
    extend(&mut tree, &b2);
    let b3 = mine_on(&tree, b2.header.block_id, &miner_b, &[]);
    extend(&mut tree, &b3);

    let mut peer = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    for blk in [&a1, &a2] {
        peer.send(&WireMessage::GossipBlock(Box::new(blk.clone())))
            .await;
    }
    wait_until("branch A", || tip(&node) == a2.header.block_id).await;
    assert_eq!(pooled(&node), 0, "the share is registered on branch A");

    for blk in [&b1, &b2, &b3] {
        peer.send(&WireMessage::GossipBlock(Box::new(blk.clone())))
            .await;
    }
    wait_until("the reorg to branch B", || tip(&node) == b3.header.block_id).await;
    wait_until("the abandoned share back in the pool", || {
        pooled(&node) == 1
    })
    .await;

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only the work check ties a work_id to its owner, so pending shares are keyed by
/// (owner, work_id): a validly signed claim to an honest share's work_id under another
/// key cannot shadow the genuine share, and the claimant is banned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_to_another_owners_work_does_not_shadow_it() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let tree = BlockTree::new(genesis.clone()).unwrap();
    let honest_owner = Arc::new(OwnerKeypair::from_seed(&[8; 32]));
    let thief = Arc::new(OwnerKeypair::from_seed(&[9; 32]));
    let peers = || node.peer_manager.peer_count();
    let pooled = |n: &NodeHandle| n.status_rx.borrow().share_pool;

    let share = mine_share(&tree, &honest_owner, 31);
    // The thief's own valid share context, with the honest work_id and ticket copied in.
    let mut claim = mine_share(&tree, &thief, 32);
    claim.work_id = share.work_id;
    claim.share_ticket = share.share_ticket;
    claim.output_root = share.output_root;
    thief.sign_share(&mut claim).unwrap();

    let mut listener = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut sender = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("three peers registered", || peers() == 3).await;
    attacker
        .send(&WireMessage::GossipShare(Box::new(claim)))
        .await;
    sender
        .send(&WireMessage::GossipShare(Box::new(share.clone())))
        .await;
    match listener.recv(Duration::from_secs(30)).await {
        Some(WireMessage::GossipShare(relayed)) => assert_eq!(*relayed, share),
        other => panic!("expected the genuine share, got {other:?}"),
    }
    wait_until("the genuine share pooled", || pooled(&node) == 1).await;
    wait_until("the thief banned", || peers() == 2).await;

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Useless shares (ignored, or duplicates under a new encoding) cost checks but no
/// work to make: past its budget a peer's shares are dropped unread. It is throttled,
/// not banned, since honest peers that are syncing send many useless shares too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_useless_shares_throttles_the_peer_without_a_ban() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let tree = BlockTree::new(genesis.clone()).unwrap();
    let owner = Arc::new(OwnerKeypair::from_seed(&[10; 32]));
    let mut orphan = mine_share(&tree, &owner, 41);
    orphan.context.parent_canonical_id = Digest([7; 64]);
    owner.sign_share(&mut orphan).unwrap();
    let genuine = mine_share(&tree, &owner, 42);
    let pooled = |n: &NodeHandle| n.status_rx.borrow().share_pool;

    let mut flooder = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || {
        node.peer_manager.peer_count() == 2
    })
    .await;
    // Ignored shares are not remembered as seen, so even the same bytes count again.
    for _ in 0..300 {
        flooder
            .send(&WireMessage::GossipShare(Box::new(orphan.clone())))
            .await;
    }
    // Over budget: a valid share from the flooder is dropped unread. The sync reply
    // shows that the node has handled every frame sent before it.
    flooder
        .send(&WireMessage::GossipShare(Box::new(genuine.clone())))
        .await;
    flooder
        .send(&WireMessage::SyncRequest { from_height: 1 })
        .await;
    loop {
        match flooder.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncResponse { .. }) => break,
            Some(_) => continue,
            None => panic!("the flooding peer was disconnected"),
        }
    }

    // The same share from another peer is pooled and relayed to the flooder, which is
    // still connected. Had the flooder's own copy been imported, the node would never
    // relay the share back to it.
    honest
        .send(&WireMessage::GossipShare(Box::new(genuine.clone())))
        .await;
    wait_until("the share pooled from the other peer", || {
        pooled(&node) == 1
    })
    .await;
    loop {
        match flooder.recv(Duration::from_secs(30)).await {
            Some(WireMessage::GossipShare(sh)) => {
                assert_eq!(*sh, genuine);
                break;
            }
            Some(_) => continue,
            None => panic!("the share was not relayed to the throttled peer"),
        }
    }
    assert_eq!(node.peer_manager.peer_count(), 2, "no one was banned");

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Status counters of the node.
fn snapshot(node: &NodeHandle) -> Arc<node::chain_actor::StatusSnapshot> {
    node.status_rx.borrow().clone()
}

/// Waits until the chain actor has handled every command sent before this one.
async fn actor_barrier(node: &NodeHandle) {
    let (tx, rx) = oneshot::channel();
    let cmd = ChainCmd::HasBlock {
        id: Digest::ZERO,
        reply: tx,
    };
    node.chain_tx.send(cmd).await.unwrap();
    rx.await.unwrap();
}

/// A peer streaming blocks that pass Stage B but fail Stage A gets at most two of them
/// verified (the limit per source); the rest wait in its queue or are refused, and are
/// dropped the moment its first block fails, before any of them starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_is_limited_per_source_and_dropped_with_its_ban() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let tree = BlockTree::new(genesis.clone()).unwrap();
    let attacker_key = Arc::new(OwnerKeypair::from_seed(&[15; 32]));
    let miner = Arc::new(OwnerKeypair::from_seed(&[16; 32]));
    let peers = || node.peer_manager.peer_count();

    let bogus = bogus_blocks(&tree, tree.tip_id(), &attacker_key, 20);
    let b1 = mine_next(&tree, &miner);
    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || peers() == 2).await;

    for block in &bogus {
        attacker
            .send(&WireMessage::GossipBlock(Box::new(block.clone())))
            .await;
    }
    wait_until("the attacker banned", || peers() == 1).await;
    // Its other Stage A task is aborted and its queued blocks dropped...
    wait_until("its pending blocks dropped", || {
        let snap = snapshot(&node);
        snap.verifying_blocks == 0 && snap.queued_blocks == 0
    })
    .await;
    // ...before any of those could start.
    let snap = snapshot(&node);
    assert!(
        snap.stage_a_started <= 2,
        "{} of the attacker's blocks entered Stage A",
        snap.stage_a_started
    );
    assert_eq!(snap.tip.height, 0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        snapshot(&node).stage_a_started <= 2,
        "none of them started later"
    );

    // Honest blocks are verified at once.
    honest
        .send(&WireMessage::GossipBlock(Box::new(b1.clone())))
        .await;
    wait_until("block 1 from the honest peer", || {
        snapshot(&node).tip.id == b1.header.block_id
    })
    .await;

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A block delivered first by a peer that then gets banned stays pending when another
/// peer delivered it too, now on that peer's behalf: honest peers relay a block only
/// once, so dropping it would lose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_block_another_peer_delivered_too_survives_its_first_senders_ban() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let miner = Arc::new(OwnerKeypair::from_seed(&[17; 32]));
    let owner = Arc::new(OwnerKeypair::from_seed(&[18; 32]));
    let peers = || node.peer_manager.peer_count();

    let b1 = mine_next(&tree, &miner);
    let mut bad_share = mine_share(&tree, &owner, 71);
    bad_share.share_target = Target(u128::MAX - 1);
    owner.sign_share(&mut bad_share).unwrap();
    extend(&mut tree, &b1);
    let b2 = mine_next(&tree, &miner);

    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || peers() == 2).await;

    // Block 2 arrives first from the attacker while block 1 is unknown: it is parked.
    attacker
        .send(&WireMessage::GossipBlock(Box::new(b2.clone())))
        .await;
    loop {
        match attacker.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncRequest { .. }) => break,
            Some(_) => continue,
            None => panic!("block 2 from the attacker was never processed"),
        }
    }
    // The honest peer relays the same bytes; the sync reply shows it was handled.
    honest
        .send(&WireMessage::GossipBlock(Box::new(b2.clone())))
        .await;
    honest
        .send(&WireMessage::SyncRequest { from_height: 1 })
        .await;
    loop {
        match honest.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncResponse { .. }) => break,
            Some(_) => continue,
            None => panic!("the honest peer was disconnected"),
        }
    }
    // The attacker is banned for an invalid share; block 2 stays parked for the honest peer.
    attacker
        .send(&WireMessage::GossipShare(Box::new(bad_share)))
        .await;
    wait_until("the attacker banned", || peers() == 1).await;
    honest
        .send(&WireMessage::GossipBlock(Box::new(b1.clone())))
        .await;
    wait_until("blocks 1 and 2", || snapshot(&node).tip.height == 2).await;
    assert_eq!(snapshot(&node).tip.id, b2.header.block_id);

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cancelling a source (as a ban does) aborts its Stage A tasks and drops its queued
/// blocks without condemning them: the same blocks from another source are accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_source_drops_its_pending_blocks_without_condemning_them() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let tree = BlockTree::new(genesis.clone()).unwrap();
    let blocks: Vec<Block> = (0..4u8)
        .map(|i| {
            let miner = Arc::new(OwnerKeypair::from_seed(&[20 + i; 32]));
            mine_on(&tree, tree.genesis_id(), &miner, &[])
        })
        .collect();
    let banned = Source::Addr("127.0.0.1:9".parse().unwrap());
    let other = Source::Addr("127.0.0.1:10".parse().unwrap());

    // All five commands reach the actor's mailbox before any Stage A (a full tile) ends.
    let mut acks = Vec::new();
    for block in &blocks {
        let (tx, rx) = oneshot::channel();
        let cmd = ChainCmd::ImportBlock {
            block: Arc::new(block.clone()),
            origin: Origin::Peer(9_001, banned),
            ack: Some(tx),
        };
        node.chain_tx.send(cmd).await.unwrap();
        acks.push(rx);
    }
    let cmd = ChainCmd::CancelSource {
        source: banned,
        ban: Duration::from_secs(60),
    };
    node.chain_tx.send(cmd).await.unwrap();
    for ack in acks {
        assert_eq!(ack.await.unwrap(), ImportAck::Queued);
    }
    actor_barrier(&node).await;
    let snap = snapshot(&node);
    assert_eq!(snap.stage_a_started, 2, "two in Stage A, two queued");
    assert_eq!((snap.verifying_blocks, snap.queued_blocks), (0, 0));

    // Nothing new is admitted from the banned source...
    let (tx, rx) = oneshot::channel();
    let cmd = ChainCmd::ImportBlock {
        block: Arc::new(blocks[0].clone()),
        origin: Origin::Peer(9_001, banned),
        ack: Some(tx),
    };
    node.chain_tx.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), ImportAck::Busy);
    // ...the aborted blocks were not applied, and another source's copies are accepted.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(snapshot(&node).tip.height, 0);
    for block in &blocks {
        let cmd = ChainCmd::ImportBlock {
            block: Arc::new(block.clone()),
            origin: Origin::Peer(9_002, other),
            ack: None,
        };
        node.chain_tx.send(cmd).await.unwrap();
    }
    for block in &blocks {
        let id = block.header.block_id;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (tx, rx) = oneshot::channel();
            node.chain_tx
                .send(ChainCmd::HasBlock { id, reply: tx })
                .await
                .unwrap();
            if rx.await.unwrap() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a re-sent block was never applied"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sends a Handshake with `genesis_id` (and the node's own nonce if `echo_nonce`) after
/// reading the node's; returns whether the node keeps the connection.
async fn handshake_accepted(addr: SocketAddr, genesis_id: Digest, echo_nonce: bool) -> bool {
    let stream = TcpStream::connect(addr).await.unwrap();
    let codec = LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .big_endian()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec();
    let mut framed = Framed::new(stream, codec);
    let first = framed.next().await.unwrap().unwrap();
    let WireMessage::Handshake {
        chain_id,
        node_nonce,
        ..
    } = codec::decode::<WireMessage>(&first).unwrap()
    else {
        panic!("the node did not open with a Handshake");
    };
    let ours = WireMessage::Handshake {
        version: WIRE_VERSION,
        chain_id,
        genesis_id,
        node_nonce: if echo_nonce {
            node_nonce
        } else {
            node_nonce ^ 1
        },
        best_height: 0,
        best_block_id: genesis_id,
    };
    framed
        .send(Bytes::from(codec::encode(&ours).unwrap()))
        .await
        .unwrap();
    matches!(
        tokio::time::timeout(Duration::from_secs(1), framed.next()).await,
        Err(_) | Ok(Some(Ok(_)))
    )
}

/// The handshake carries the genesis id and a per-process nonce: a node on another
/// genesis with the same chain_id, or the node itself, is not kept as a peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshakes_from_another_genesis_or_from_the_node_itself_are_refused() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let genesis_id = BlockTree::new(genesis.clone()).unwrap().genesis_id();
    let peers = || node.peer_manager.peer_count();

    assert!(!handshake_accepted(node.p2p_addr, Digest([1; 64]), false).await);
    assert!(!handshake_accepted(node.p2p_addr, genesis_id, true).await);
    assert_eq!(peers(), 0);
    assert!(handshake_accepted(node.p2p_addr, genesis_id, false).await);

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `--mine-when-synced`, a node that cannot reach its peers does not mine (a
/// private fork it could never leave); a node joining a peer that is ahead starts
/// mining only once it has caught up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mining_waits_for_a_peer_and_for_sync_when_asked_to() {
    let genesis = test_genesis();
    let base = std::env::temp_dir().join(format!("plauthium-gate-{}", now_secs_nanos()));
    std::fs::create_dir_all(&base).unwrap();
    let genesis_path = base.join("genesis.json");
    std::fs::write(&genesis_path, serde_json::to_string(&genesis).unwrap()).unwrap();
    let args = |name: &str, peers: Vec<node::net::PeerAddr>, gated: bool| RunArgs {
        data_dir: base.join(name),
        key_file: None,
        genesis: Some(genesis_path.clone()),
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers,
        mining_threads: 1,
        no_mine: false,
        mine_when_synced: gated,
        verify_threads: Some(2),
        reset: true,
    };
    let height = |n: &NodeHandle| n.status_rx.borrow().tip.height;

    // Its only peer does not exist: it waits instead of mining alone.
    let unreachable = "127.0.0.1:1".parse::<SocketAddr>().unwrap().into();
    let alone = start_node(args("alone", vec![unreachable], true))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(height(&alone), 0);
    assert!(!alone.engine.sync_gate_open());
    alone.stop().await;

    // A peer that is 30 blocks ahead (and then stops mining: an attempt already under
    // way may still seal one more block, so wait until its height settles).
    let ahead = start_node(args("ahead", vec![], false)).await.unwrap();
    wait_until("the peer at height 30", || height(&ahead) >= 30).await;
    ahead.engine.set_enabled(false);
    let mut target = height(&ahead);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if height(&ahead) == target {
            break;
        }
        target = height(&ahead);
    }

    // The joining node opens its gate only at the peer's height, then mines on top.
    let joined = start_node(args("joined", vec![ahead.p2p_addr.into()], true))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !joined.engine.sync_gate_open() {
        assert!(Instant::now() < deadline, "the gate never opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        height(&joined) >= target,
        "the gate opened at height {} before catching up to {target}",
        height(&joined)
    );
    wait_until("a block mined on top by the joined node", || {
        let tip = ahead.status_rx.borrow().tip.clone();
        tip.height > target && tip.miner_key_id == joined.miner_key_id
    })
    .await;

    joined.stop().await;
    ahead.stop().await;
    let _ = std::fs::remove_dir_all(&base);
}

/// A peer that claims a height it never delivers holds `--mine-when-synced` back only
/// until its sync request times out: the claim then drops to what it delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_lying_about_its_height_holds_mining_back_only_briefly() {
    let genesis = test_genesis();
    let base = std::env::temp_dir().join(format!("plauthium-liar-{}", now_secs_nanos()));
    std::fs::create_dir_all(&base).unwrap();
    let genesis_path = base.join("genesis.json");
    std::fs::write(&genesis_path, serde_json::to_string(&genesis).unwrap()).unwrap();
    let node = start_node(RunArgs {
        data_dir: base.join("gated"),
        key_file: None,
        genesis: Some(genesis_path),
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers: vec![],
        mining_threads: 1,
        no_mine: false,
        mine_when_synced: true,
        verify_threads: Some(2),
        reset: true,
    })
    .await
    .unwrap();
    let tree = BlockTree::new(genesis.clone()).unwrap();

    let _liar = RawPeer::connect_claiming(
        node.p2p_addr,
        &genesis,
        &tree,
        1_000_000_000,
        Digest([5; 64]),
    )
    .await;
    let connected = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !node.engine.sync_gate_open(),
        "it first waits for the claimed height"
    );
    assert_eq!(node.status_rx.borrow().tip.height, 0);
    // SYNC_TIMEOUT (30 s) later the claim drops to our tip and mining starts, well before
    // the one-minute stall rule would open the gate.
    while node.status_rx.borrow().tip.height == 0 {
        assert!(
            connected.elapsed() < Duration::from_secs(50),
            "the lying peer held mining back past its sync timeout"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    node.stop().await;
    let _ = std::fs::remove_dir_all(&base);
}

/// Orphans released by their parent count against the deferred limits (8 per source):
/// a source cannot park future-dated blocks for free and have every one of them kept
/// once their parent arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_orphans_count_against_the_deferred_limits() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let miner = Arc::new(OwnerKeypair::from_seed(&[30; 32]));
    let attacker_key = Arc::new(OwnerKeypair::from_seed(&[31; 32]));
    let peers = || node.peer_manager.peer_count();

    let b1 = mine_next(&tree, &miner);
    extend(&mut tree, &b1);
    // Twenty children of block 1, 40 s ahead of the clock: past MAX_FUTURE_SECS, so
    // they are deferred, but within the slack, so they are not refused outright.
    let tpl = template_on_at(
        &tree,
        b1.header.block_id,
        &attacker_key,
        &[],
        now_secs() + 40,
    );
    let ctx0 = fw_context_to_core(&tpl.context_with_nonce(0)).unwrap();
    let inst = ctx0.instance();
    let z0 = inst.x.product(&inst.y);
    let children: Vec<Block> = (1..=20u64)
        .map(|nonce| {
            let ctx = fw_context_to_core(&tpl.context_with_nonce(nonce)).unwrap();
            tpl.seal(nonce, &FreshWorkIds::derive(&ctx, &z0)).unwrap()
        })
        .collect();

    let mut attacker = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &tree).await;
    wait_until("both peers registered", || peers() == 2).await;
    for child in &children {
        attacker
            .send(&WireMessage::GossipBlock(Box::new(child.clone())))
            .await;
    }
    attacker
        .send(&WireMessage::SyncRequest { from_height: 1 })
        .await;
    loop {
        match attacker.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncResponse { .. }) => break,
            Some(_) => continue,
            None => panic!("the attacker was disconnected"),
        }
    }
    assert_eq!(
        snapshot(&node).orphan_count,
        20,
        "parked while block 1 is unknown"
    );

    honest
        .send(&WireMessage::GossipBlock(Box::new(b1.clone())))
        .await;
    // The status is published once more after the released orphans are handled.
    wait_until("block 1 and its released orphans", || {
        let snap = snapshot(&node);
        snap.tip.height == 1 && snap.orphan_count == 0
    })
    .await;
    assert_eq!(
        snapshot(&node).deferred_blocks,
        8,
        "the deferred limit per source"
    );

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Orphans cost nothing to make (nothing is checked before the parent lookup), so each
/// source may park at most a sync batch's worth: a flood from one source evicts its own
/// orphans, not the honest blocks parked behind a missing parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_orphan_flood_does_not_evict_another_sources_parked_blocks() {
    let genesis = test_genesis();
    let (node, dir) = start(&genesis).await;
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let miner = Arc::new(OwnerKeypair::from_seed(&[32; 32]));
    let peers = || node.peer_manager.peer_count();

    let mut chain = Vec::new();
    for _ in 0..41 {
        let block = mine_next(&tree, &miner);
        extend(&mut tree, &block);
        chain.push(block);
    }
    // Fake orphans: a real block under random parent and block ids (not at height 1,
    // where an orphan means another genesis and closes the connection).
    let fakes: Vec<Block> = (0..300u32)
        .map(|i| {
            let mut fake = chain[5].clone();
            let mut id = [0xEE; 64];
            id[..4].copy_from_slice(&i.to_le_bytes());
            fake.header.context.parent_canonical_id = Digest(id);
            id[4] = 0x01;
            fake.header.block_id = Digest(id);
            fake
        })
        .collect();

    let genesis_tree = BlockTree::new(genesis.clone()).unwrap();
    let mut honest = RawPeer::connect(node.p2p_addr, &genesis, &genesis_tree).await;
    let mut flooder = RawPeer::connect(node.p2p_addr, &genesis, &genesis_tree).await;
    wait_until("both peers registered", || peers() == 2).await;
    // Blocks 2..41 arrive before block 1: all forty are parked.
    for block in &chain[1..] {
        honest
            .send(&WireMessage::GossipBlock(Box::new(block.clone())))
            .await;
    }
    for fake in &fakes {
        flooder
            .send(&WireMessage::GossipBlock(Box::new(fake.clone())))
            .await;
    }
    flooder
        .send(&WireMessage::SyncRequest { from_height: 1 })
        .await;
    loop {
        match flooder.recv(Duration::from_secs(30)).await {
            Some(WireMessage::SyncResponse { .. }) => break,
            Some(_) => continue,
            None => panic!("the flooder was disconnected"),
        }
    }
    honest
        .send(&WireMessage::GossipBlock(Box::new(chain[0].clone())))
        .await;
    wait_until("all 41 honest blocks", || snapshot(&node).tip.height == 41).await;

    node.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A peer that answers a sync request with blocks from another height says nothing
/// about the height it claimed: the claim drops at once, and a node waiting for it to
/// catch up starts mining without waiting for a timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_off_height_sync_answer_drops_the_claimed_height() {
    let genesis = test_genesis();
    let base = std::env::temp_dir().join(format!("plauthium-offheight-{}", now_secs_nanos()));
    std::fs::create_dir_all(&base).unwrap();
    let genesis_path = base.join("genesis.json");
    std::fs::write(&genesis_path, serde_json::to_string(&genesis).unwrap()).unwrap();
    let node = start_node(RunArgs {
        data_dir: base.join("gated"),
        key_file: None,
        genesis: Some(genesis_path),
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers: vec![],
        mining_threads: 1,
        no_mine: false,
        mine_when_synced: true,
        verify_threads: Some(2),
        reset: true,
    })
    .await
    .unwrap();
    let mut tree = BlockTree::new(genesis.clone()).unwrap();
    let miner = Arc::new(OwnerKeypair::from_seed(&[33; 32]));
    let b1 = mine_next(&tree, &miner);
    extend(&mut tree, &b1);
    let b2 = mine_next(&tree, &miner);
    let genesis_tree = BlockTree::new(genesis.clone()).unwrap();

    let mut liar = RawPeer::connect_claiming(
        node.p2p_addr,
        &genesis,
        &genesis_tree,
        1_000_000_000,
        Digest([6; 64]),
    )
    .await;
    let connected = Instant::now();
    // Every request is answered with block 2, whatever height it asks for.
    let answer = tokio::spawn(async move {
        while let Some(msg) = liar.recv(Duration::from_secs(60)).await {
            if let WireMessage::SyncRequest { .. } = msg {
                liar.send(&WireMessage::SyncResponse {
                    blocks: vec![b2.clone()],
                })
                .await;
            }
        }
    });
    while node.status_rx.borrow().tip.height == 0 {
        assert!(
            connected.elapsed() < Duration::from_secs(15),
            "the off-height answer did not drop the claimed height"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    answer.abort();
    node.stop().await;
    let _ = std::fs::remove_dir_all(&base);
}
