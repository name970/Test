//! The public testnet's identity: every node built from this source must derive the
//! same genesis, so a change to `node/genesis.testnet.json` has to be deliberate.

#![forbid(unsafe_code)]

use node::cli::RunArgs;
use node::net::PeerAddr;
use node::{DEFAULT_GENESIS_JSON, start_node};
use wire::GenesisConfig;

/// 2026-10-21T00:00:00Z, the Public Testnet v1 launch, in 1-second buckets.
const GENESIS_TIME_BUCKET: u64 = 1_792_540_800;

/// `genesis_id` of the embedded genesis.
const GENESIS_ID: &str = "2804064dac4e30a4cb47aa0d87c4da05427c6bcdb8d1d5aa45a1fd69c3677d5cd8d441d3beb877317277096139b6205287472d078dedea8e525be60ee5f5114e";

#[test]
fn the_embedded_genesis_is_the_public_testnet_v1_genesis() {
    let genesis: GenesisConfig = serde_json::from_str(DEFAULT_GENESIS_JSON).unwrap();
    chain::validate_genesis(&genesis).unwrap();
    assert_eq!(genesis.time_bucket_secs, 1);
    assert_eq!(genesis.genesis_time_bucket, GENESIS_TIME_BUCKET);
    let id = chain::compute_genesis_id(&genesis).unwrap();
    assert_eq!(id.to_hex(), GENESIS_ID);
}

#[test]
fn peers_are_ip_or_host_name_with_a_port() {
    let socket = |s: &str| matches!(s.parse::<PeerAddr>(), Ok(PeerAddr::Socket(_)));
    let host = |s: &str| matches!(s.parse::<PeerAddr>(), Ok(PeerAddr::Host(_)));
    assert!(socket("203.0.113.7:30333"));
    assert!(socket("[2001:db8::1]:30333"));
    assert!(host("seed1.plauthium.network:30333"));
    assert!(host("localhost:30333"));
    for bad in [
        "seed1.plauthium.network",
        "seed1.plauthium.network:",
        "seed1.plauthium.network:70000",
        ":30333",
        "seed 1:30333",
        "user@seed1:30333",
        "2001:db8::1:30333x",
    ] {
        assert!(bad.parse::<PeerAddr>().is_err(), "{bad:?} must be refused");
    }
}

/// Two nodes on one data directory would corrupt each other's `blocks.log`: the second
/// one refuses to start until the first has stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_data_directory_serves_one_node_at_a_time() {
    let dir = std::env::temp_dir().join(format!(
        "plauthium-lock-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let args = || RunArgs {
        data_dir: dir.clone(),
        key_file: None,
        genesis: None,
        p2p_listen: "127.0.0.1:0".parse().unwrap(),
        rpc_listen: "127.0.0.1:0".parse().unwrap(),
        peers: vec![],
        mining_threads: 0,
        no_mine: true,
        mine_when_synced: false,
        verify_threads: Some(1),
        reset: true,
    };
    let first = start_node(args()).await.unwrap();
    let second = start_node(args()).await;
    let err = second
        .err()
        .expect("a second node on the same data directory");
    assert!(err.to_string().contains("in use"), "{err}");
    first.stop().await;
    let again = start_node(args()).await.unwrap();
    again.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
