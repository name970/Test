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
        now_secs(),
    )
    .unwrap()
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
