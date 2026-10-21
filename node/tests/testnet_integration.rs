//! Three-node integration test covering acceptance criteria A2–A6 (§11).
//!
//! Real 512x512 tiles are mined and verified, so run it with `--release`.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chain::OwnerKeypair;
use chain::keys::unsigned;
use node::chain_actor::{ChainCmd, Origin};
use node::cli::RunArgs;
use node::mempool::{MempoolCmd, MempoolReject};
use node::net::PeerAddr;
use node::{NodeHandle, start_node};
use primitives::TOTAL_SUPPLY;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::Instant;
use wire::{DifficultyMode, Digest, GenesisConfig, JobPayload, JobTransaction, Target};

const PHASE_TIMEOUT: Duration = Duration::from_secs(240);

fn temp_test_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("plauthium-it-{label}-{nanos}"))
}

/// About 64 attempts per seal (~1.5 s blocks with three miners) and roughly two
/// verified shares per block, so validation keeps up with production.
fn test_genesis() -> GenesisConfig {
    let mut g = GenesisConfig::default_testnet(1_700_000_000);
    g.initial_seal_target = Target(u128::MAX / 64);
    g.share_ratio = 2;
    g.difficulty = DifficultyMode::Fixed;
    g.blocks_per_epoch = 5;
    g.settle_delay = 1;
    g.share_maturity_depth = 1;
    g.snapshot_window = 30;
    g.share_inclusion_window = 15;
    g
}

/// Starts node `index` on `dir/n<index>`; its miner key is created there on first start.
async fn start(
    dir: &Path,
    index: u64,
    genesis: &Path,
    peers: Vec<PeerAddr>,
    mine: bool,
    reset: bool,
) -> NodeHandle {
    start_node(RunArgs {
        data_dir: dir.join(format!("n{index}")),
        key_file: None,
        genesis: Some(genesis.to_path_buf()),
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers,
        mining_threads: 1,
        no_mine: !mine,
        mine_when_synced: false,
        verify_threads: Some(2),
        reset,
    })
    .await
    .unwrap()
}

fn height(n: &NodeHandle) -> u64 {
    n.status_rx.borrow().tip.height
}

fn tip_id(n: &NodeHandle) -> Digest {
    n.status_rx.borrow().tip.id
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stops mining and waits until every node has the same tip and balances.
///
/// With mining stopped, two equal-weight tips both survive (a tie keeps the current
/// tip, §6.5), so a tie is broken by letting `tiebreaker` mine one more block.
async fn converge(nodes: &[&NodeHandle], tiebreaker: &NodeHandle) {
    for n in nodes {
        n.engine.set_enabled(false);
    }
    for _round in 0..5 {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let first = tip_id(nodes[0]);
            if nodes.iter().all(|n| tip_id(n) == first) {
                let balances = nodes[0].status_rx.borrow().balances;
                assert!(
                    nodes
                        .iter()
                        .all(|n| n.status_rx.borrow().balances == balances)
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let h = height(tiebreaker);
        tiebreaker.engine.set_enabled(true);
        wait_until("tie-breaking block", || height(tiebreaker) > h).await;
        tiebreaker.engine.set_enabled(false);
    }
    panic!("nodes did not converge on one tip");
}

async fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

async fn status_json(n: &NodeHandle) -> Value {
    let (code, body) = http(n.rpc_addr, "GET", "/api/status", None).await;
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    // A6: the explorer's data source always reports a conserved supply.
    assert_eq!(v["ledger"]["conserved"], Value::Bool(true));
    let sum: u128 = v["ledger"]["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["atoms"].as_str().unwrap().parse::<u128>().unwrap())
        .sum();
    assert_eq!(sum, u128::from(TOTAL_SUPPLY.get()));
    v
}

async fn mempool_size(n: &NodeHandle) -> usize {
    let (tx, rx) = oneshot::channel();
    n.mempool_tx
        .send(MempoolCmd::Stats { reply: tx })
        .await
        .unwrap();
    rx.await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_criteria_a2_to_a6() {
    let base_dir = temp_test_dir("full");
    std::fs::create_dir_all(&base_dir).unwrap();
    let genesis_path = base_dir.join("genesis.json");
    let genesis_json = serde_json::to_string_pretty(&test_genesis()).unwrap();
    std::fs::write(&genesis_path, genesis_json).unwrap();

    let n1 = start(&base_dir, 1, &genesis_path, vec![], true, true).await;
    // Node 2 names node 1 by host name, as nodes name the public bootnodes.
    let n1_by_name = format!("localhost:{}", n1.p2p_addr.port()).parse().unwrap();
    let n2 = start(&base_dir, 2, &genesis_path, vec![n1_by_name], true, true).await;
    wait_until("node 2 to reach node 1 by host name", || {
        n1.peer_manager.peer_count() == 1 && n2.peer_manager.peer_count() == 1
    })
    .await;
    let n3 = start(
        &base_dir,
        3,
        &genesis_path,
        vec![n1.p2p_addr.into(), n2.p2p_addr.into()],
        true,
        true,
    )
    .await;

    // Every node generated its own ML-DSA-87 key in its data directory.
    let miner_keys: HashSet<String> = [&n1, &n2, &n3]
        .iter()
        .map(|n| n.miner_key_id.to_hex())
        .collect();
    assert_eq!(miner_keys.len(), 3, "three distinct miner identities");
    for i in 1..=3 {
        assert!(base_dir.join(format!("n{i}/miner_key.bin")).exists());
    }
    let n3_key_id = n3.miner_key_id;

    // A6: the explorer is served at `/` with the conservation badge.
    let (code, html) = http(n1.rpc_addr, "GET", "/", None).await;
    assert_eq!(code, 200);
    assert!(html.contains("Σ = M = 1,500,000,000,000,000 ✓"));
    let st = status_json(&n1).await;
    assert_eq!(st["node"]["owner_key_id"], n1.miner_key_id.to_hex());

    // A2 (part 1): all nodes advance; the epoch-1 booking is paid into U at h = 10.
    wait_until("height >= 11 on all nodes", || {
        [&n1, &n2, &n3].iter().all(|n| height(n) >= 11)
    })
    .await;

    // A3: submit a job to node 1 over HTTP; it must settle on node 3 with the exact result.
    let xs: Vec<i64> = (1..=512).collect();
    let ys: Vec<i64> = (1..=512).rev().collect();
    let direct_dot: i128 = xs.iter().zip(&ys).map(|(&a, &b)| i128::from(a * b)).sum();
    let body = serde_json::json!({
        "nonce": "777",
        "payload": { "type": "exact_dot", "xs": xs, "ys": ys }
    })
    .to_string();
    // The RPC sets expiry_height = tip + window, which is part of the job's identity, so
    // the tip must not move between the two submissions: pause mining until it settles.
    for n in [&n1, &n2, &n3] {
        n.engine.set_enabled(false);
    }
    let mut settled = (height(&n1), Instant::now());
    while settled.1.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if height(&n1) != settled.0 {
            settled = (height(&n1), Instant::now());
        }
    }
    let (code, resp) = http(n1.rpc_addr, "POST", "/api/submit_job", Some(&body)).await;
    assert_eq!(code, 202, "{resp}");
    let resp: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(resp["expected_result"], direct_dot.to_string());
    // The RPC signs the job with node 1's miner key.
    assert_eq!(resp["client_key_id"], n1.miner_key_id.to_hex());
    assert_eq!(resp["deposit"], "10000000000");
    let job_id = resp["job_id"].as_str().unwrap().to_string();
    let (code, _) = http(n1.rpc_addr, "POST", "/api/submit_job", Some(&body)).await;
    assert_eq!(code, 409, "resubmitting the same job is a duplicate");
    for n in [&n1, &n2, &n3] {
        n.engine.set_enabled(true);
    }

    let supply = TOTAL_SUPPLY.get().to_string();
    let deadline = Instant::now() + PHASE_TIMEOUT;
    let settled_at = loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for A3 settlement"
        );
        status_json(&n3).await;
        let (code, blocks) = http(n3.rpc_addr, "GET", "/api/blocks?limit=100", None).await;
        assert_eq!(code, 200);
        let blocks: Value = serde_json::from_str(&blocks).unwrap();
        let blocks = blocks["blocks"].as_array().unwrap();
        assert!(
            blocks
                .iter()
                .all(|b| b["total_after"].as_str() == Some(supply.as_str()))
        );
        // Every block was mined, and signed, by one of the three node keys.
        assert!(
            blocks
                .iter()
                .all(|b| miner_keys.contains(b["miner_owner"].as_str().unwrap()))
        );
        let receipt = blocks.iter().find_map(|b| {
            b["receipts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["job_id"] == job_id.as_str())
                .map(|r| (b["height"].as_u64().unwrap(), r["result"].clone()))
        });
        if let Some((h, result)) = receipt {
            assert_eq!(result, direct_dot.to_string());
            break h;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(settled_at >= 11);
    // Once on the chain (and out of the mempool), a copy of the job (replayed by a peer,
    // say) is still a duplicate: it cannot take mempool space until it expires.
    let (tx, rx) = oneshot::channel();
    let cmd = ChainCmd::GetCanonicalRange {
        from_height: 1,
        reply: tx,
    };
    n1.chain_tx.send(cmd).await.unwrap();
    let included = rx
        .await
        .unwrap()
        .iter()
        .flat_map(|b| b.body.jobs.clone())
        .find(|j| chain::compute_job_ids(j).map(|(_, id)| id.to_string()) == Ok(job_id.clone()))
        .expect("the job is on the canonical chain");
    let (tx, rx) = oneshot::channel();
    let cmd = MempoolCmd::Insert {
        tx: included,
        origin: Origin::Rpc,
        reply: tx,
    };
    n1.mempool_tx.send(cmd).await.unwrap();
    assert!(
        matches!(rx.await.unwrap(), Err(MempoolReject::Duplicate)),
        "an included job is a duplicate"
    );

    // A2: stop mining; tips and the 8 balances converge.
    converge(&[&n1, &n2, &n3], &n1).await;
    for n in [&n1, &n2, &n3] {
        let st = n.status_rx.borrow().clone();
        assert_eq!(st.total, u128::from(TOTAL_SUPPLY.get()));
        assert_eq!(st.balances[2], 0, "E_esc drains once the job has settled");
    }

    // GossipShare: a share found by node 2 is included in a block mined by node 3. From
    // here on one node mines at a time, so there is no fork and no reorg (which could
    // otherwise return another node's shares to a pool): only gossip can carry a share
    // mined after h0 from node 2 into node 3's blocks.
    let h0 = height(&n1);
    let (n2_key, n3_key) = (n2.miner_key_id.to_hex(), n3.miner_key_id.to_hex());
    let tips_agree = || tip_id(&n1) == tip_id(&n2) && tip_id(&n2) == tip_id(&n3);
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "no share of node 2 reached a block of node 3"
        );
        let found = n2.engine.shares_found();
        n2.engine.set_enabled(true);
        wait_until("node 2 finds a share", || n2.engine.shares_found() > found).await;
        n2.engine.set_enabled(false);
        wait_until("all nodes on node 2's tip", tips_agree).await;
        // Gossip, the share's work check and node 3's template rebuild.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        // A share mined at height s fits blocks from s + 1 on: node 3 mines two.
        let h = height(&n3);
        n3.engine.set_enabled(true);
        wait_until("node 3 mines two blocks", || height(&n3) >= h + 2).await;
        n3.engine.set_enabled(false);
        wait_until("all nodes on node 3's tip", tips_agree).await;

        let (code, blocks) = http(n1.rpc_addr, "GET", "/api/blocks?limit=100", None).await;
        assert_eq!(code, 200);
        let blocks: Value = serde_json::from_str(&blocks).unwrap();
        let gossiped =
            blocks["blocks"].as_array().unwrap().iter().any(|b| {
                b["miner_owner"] == n3_key.as_str()
                    && b["shares"].as_array().unwrap().iter().any(|sh| {
                        sh["owner"] == n2_key.as_str() && sh["height"].as_u64().unwrap() > h0
                    })
            });
        if gossiped {
            break;
        }
    }
    let st = status_json(&n3).await;
    assert!(st["mining"]["share_pool"].is_u64());
    assert_eq!(st["mining"]["waiting_for_sync"], Value::Bool(false));
    assert!(st["verification"]["stage_a_started"].as_u64().unwrap() > 0);
    assert!(st["verification"]["queued_blocks"].is_u64());

    // A4: stop node 3, mine 10 blocks on nodes 1 and 2, restart node 3 on its data dir.
    let pre_stop_h = height(&n3);
    let (flush_tx, flush_rx) = oneshot::channel();
    n3.chain_tx
        .send(ChainCmd::FlushAndClose { reply: flush_tx })
        .await
        .unwrap();
    flush_rx.await.unwrap();
    n3.stop().await;

    n1.engine.set_enabled(true);
    n2.engine.set_enabled(true);
    wait_until("10 more blocks on node 1", || {
        height(&n1) >= pre_stop_h + 10
    })
    .await;
    converge(&[&n1, &n2], &n1).await;

    let n3 = start(
        &base_dir,
        3,
        &genesis_path,
        vec![n1.p2p_addr.into(), n2.p2p_addr.into()],
        false,
        false,
    )
    .await;
    assert!(
        height(&n3) >= pre_stop_h,
        "blocks.log is replayed before start_node returns"
    );
    assert_eq!(
        n3.miner_key_id, n3_key_id,
        "node 3 reloads its persisted miner key"
    );
    wait_until("node 3 resync", || tip_id(&n3) == tip_id(&n1)).await;
    assert_eq!(
        n3.status_rx.borrow().balances,
        n1.status_rx.borrow().balances
    );

    // A5: partition {1} | {2, 3}; mine 1 block on node 1 and 3 on node 2, then reconnect.
    n1.peer_manager.set_network_paused(true);
    let base_h = height(&n1);
    let client = OwnerKeypair::from_seed(&[7; 32]);
    let mut part_tx = JobTransaction {
        client_key: client.public_key().clone(),
        queue_fee: 100_000_000,
        base_fee: 1_000_000_000,
        v_exec: 8_000_000_000,
        v_da: 500_000_000,
        v_ref: 500_000_000,
        expiry_height: base_h + 500,
        nonce: 9_999,
        payload: JobPayload::ExactDot {
            xs: vec![2.0; 512],
            ys: vec![3.0; 512],
        },
        signature: unsigned(),
    };
    let genesis_id = n1.status_rx.borrow().genesis_id;
    client.sign_job(&mut part_tx, &genesis_id).unwrap();

    // A job whose signature does not verify is refused.
    let mut forged = part_tx.clone();
    forged.nonce += 1;
    let (f_tx, f_rx) = oneshot::channel();
    n1.mempool_tx
        .send(MempoolCmd::Insert {
            tx: forged,
            origin: Origin::Rpc,
            reply: f_tx,
        })
        .await
        .unwrap();
    assert!(matches!(
        f_rx.await.unwrap(),
        Err(node::mempool::MempoolReject::Invalid {
            code: "bad_signature",
            ..
        })
    ));
    let (p_tx, p_rx) = oneshot::channel();
    n1.mempool_tx
        .send(MempoolCmd::Insert {
            tx: part_tx,
            origin: Origin::Rpc,
            reply: p_tx,
        })
        .await
        .unwrap();
    p_rx.await.unwrap().unwrap();
    // Let the template pick the job up before mining.
    tokio::time::sleep(Duration::from_millis(800)).await;

    n1.engine.set_enabled(true);
    wait_until("node 1 mines inside the partition", || height(&n1) > base_h).await;
    n1.engine.set_enabled(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n1_branch_tip = tip_id(&n1);

    n2.engine.set_enabled(true);
    wait_until("node 2 mines 3 blocks", || height(&n2) >= base_h + 3).await;
    n2.engine.set_enabled(false);
    wait_until("node 3 follows node 2", || tip_id(&n3) == tip_id(&n2)).await;
    assert_ne!(tip_id(&n2), n1_branch_tip, "the partition holds");

    n1.peer_manager.set_network_paused(false);
    wait_until("node 1 reorgs to the heavier branch", || {
        tip_id(&n1) == tip_id(&n2)
    })
    .await;
    assert_eq!(
        n1.status_rx.borrow().balances,
        n2.status_rx.borrow().balances
    );
    assert!(
        mempool_size(&n1).await >= 1,
        "the losing branch's job is back in node 1's mempool"
    );
    status_json(&n1).await;
    status_json(&n2).await;
    status_json(&n3).await;

    n1.stop().await;
    n2.stop().await;
    n3.stop().await;
    let _ = std::fs::remove_dir_all(&base_dir);
}
