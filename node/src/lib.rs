//! Plauthium Testnet node library: actors, P2P networking, miner, RPC and explorer.
//!
//! The `plauthium-node` binary (`src/main.rs`) is a thin CLI over [`start_node`] and
//! [`run_genesis_cmd`]; integration tests start several nodes in one process through
//! the same entry points.

#![forbid(unsafe_code)]

pub mod chain_actor;
pub mod cli;
pub mod engine;
pub mod keystore;
pub mod mempool;
pub mod net;
pub mod rpc;
pub mod share_pool;
pub mod store;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;
use wire::{DifficultyMode, Digest, GenesisConfig, Target};

use crate::chain_actor::{ChainActor, ChainCmd, ChainEvent, StatusSnapshot};
use crate::cli::{DifficultyArg, GenesisArgs, RunArgs};
use crate::engine::ConsensusEngine;
use crate::mempool::{MempoolActor, MempoolCmd, MempoolEvent, MempoolView};
use crate::net::PeerManager;
use crate::rpc::spawn_rpc_server;
use crate::store::BlockStore;

/// Embedded default genesis JSON (§3.4).
pub const DEFAULT_GENESIS_JSON: &str = include_str!("../genesis.testnet.json");

/// Boxed error type of the node entry points.
pub type NodeError = Box<dyn std::error::Error + Send + Sync>;

/// Handle to a running in-process node (used by `main` and the integration tests).
pub struct NodeHandle {
    /// Bound P2P listen address.
    pub p2p_addr: SocketAddr,
    /// Bound RPC / explorer address.
    pub rpc_addr: SocketAddr,
    /// Key id of this node's ML-DSA-87 miner key.
    pub miner_key_id: Digest,
    /// Command channel of the ChainState actor.
    pub chain_tx: mpsc::Sender<ChainCmd>,
    /// Command channel of the mempool actor.
    pub mempool_tx: mpsc::Sender<MempoolCmd>,
    /// Latest chain status.
    pub status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    /// The Fresh Work miner.
    pub engine: Arc<ConsensusEngine>,
    /// The P2P layer.
    pub peer_manager: Arc<PeerManager>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl NodeHandle {
    /// Signals graceful shutdown of every task, connection and miner thread.
    pub fn shutdown(&self) {
        self.engine.shutdown();
        let _ = self.shutdown_tx.send(true);
    }

    /// Signals shutdown and waits until the ChainState and mempool actors have exited
    /// (the ChainState actor flushes `blocks.log` on exit).
    pub async fn stop(self) {
        self.shutdown();
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Builds the genesis configuration for `plauthium-node genesis` with
/// `genesis_time_bucket = now`.
pub fn genesis_from_args(args: &GenesisArgs) -> Result<GenesisConfig, NodeError> {
    let mut cfg = GenesisConfig::default_testnet(0);
    cfg.genesis_time_bucket = unix_now_secs() / cfg.time_bucket_secs;
    cfg.name = args.name.clone();
    cfg.initial_seal_target = Target(u128::MAX / u128::from(args.seal_attempts.max(1)));
    cfg.share_ratio = args.share_ratio;
    cfg.difficulty = match args.difficulty {
        DifficultyArg::Fixed => DifficultyMode::Fixed,
        DifficultyArg::Windowed => DifficultyMode::Windowed {
            window: args.retarget_window,
        },
    };
    cfg.target_block_secs = args.target_block_secs;
    cfg.blocks_per_epoch = args.blocks_per_epoch;
    cfg.settle_delay = args.settle_delay;
    chain::validate_genesis(&cfg)?;
    Ok(cfg)
}

/// `plauthium-node genesis`: writes a validated genesis JSON file.
pub fn run_genesis_cmd(args: GenesisArgs) -> Result<(), NodeError> {
    let cfg = genesis_from_args(&args)?;
    let genesis_id = chain::compute_genesis_id(&cfg)?;
    if let Some(parent) = args.out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&cfg)?)?;
    info!(path = %args.out.display(), %genesis_id, "Wrote genesis configuration");
    Ok(())
}

/// Loads the genesis file named by `--genesis`, or the embedded default.
pub fn load_genesis(args: &RunArgs) -> Result<GenesisConfig, NodeError> {
    let genesis: GenesisConfig = match &args.genesis {
        Some(path) => serde_json::from_str(&std::fs::read_to_string(path)?)?,
        None => serde_json::from_str(DEFAULT_GENESIS_JSON)?,
    };
    chain::validate_genesis(&genesis)?;
    Ok(genesis)
}

/// Starts all actors, P2P networking, the ConsensusEngine and the Axum RPC for one node.
pub async fn start_node(args: RunArgs) -> Result<NodeHandle, NodeError> {
    let profile_check = chain::testnet_ozaki_profile().validate();
    profile_check.map_err(|e| format!("TESTNET_PROFILE validation failed: {e}"))?;
    let genesis = load_genesis(&args)?;

    let key_path = args
        .key_file
        .clone()
        .unwrap_or_else(|| args.data_dir.join(keystore::KEY_FILE_NAME));
    if args.reset && args.data_dir.exists() {
        keystore::reset_data_dir(&args.data_dir, &key_path)?;
    }
    std::fs::create_dir_all(&args.data_dir)?;
    let (miner, created) = keystore::load_or_create(&key_path)?;
    let miner = Arc::new(miner);
    info!(
        key_id = %miner.key_id().short_hex(16),
        owner_id = miner.owner_id(),
        path = %key_path.display(),
        "{} ML-DSA-87 miner key",
        if created { "Generated a new" } else { "Loaded the" }
    );
    let (log_writer, disk_blocks) = BlockStore::open(&args.data_dir.join("blocks.log"))?;

    let (chain_tx, chain_rx) = mpsc::channel::<ChainCmd>(1_024);
    let (mempool_tx, mempool_rx) = mpsc::channel::<MempoolCmd>(1_024);
    let (chain_event_tx, _) = broadcast::channel::<ChainEvent>(1_024);
    let (mempool_event_tx, _) = broadcast::channel::<MempoolEvent>(1_024);
    let (mempool_view_tx, mempool_view_rx) = watch::channel(Arc::new(MempoolView::default()));
    let (template_tx, template_rx) = watch::channel(None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let verify_threads = args.verify_threads.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1))
    });
    let verify_semaphore = Arc::new(Semaphore::new(verify_threads.max(1)));

    let (mut chain_actor, status_rx) = ChainActor::new(
        genesis.clone(),
        miner.clone(),
        log_writer,
        chain_tx.clone(),
        chain_rx,
        chain_event_tx.clone(),
        template_tx,
        mempool_view_rx.clone(),
        verify_semaphore,
    )?;
    // Persisted blocks are replayed before any peer connects (Origin::Disk, §8.7).
    chain_actor.replay_disk_blocks(disk_blocks);

    let mining_threads = if args.no_mine { 0 } else { args.mining_threads };
    let engine = ConsensusEngine::spawn(
        mining_threads,
        mining_threads > 0,
        template_rx,
        chain_tx.clone(),
    )?;

    let mempool_actor = MempoolActor::new(
        mempool_rx,
        chain_tx.clone(),
        chain_event_tx.subscribe(),
        mempool_event_tx.clone(),
        mempool_view_tx,
        status_rx.clone(),
    );
    let mut tasks = vec![tokio::spawn(mempool_actor.run(shutdown_rx.clone()))];
    tasks.push(tokio::spawn(chain_actor.run(shutdown_rx.clone())));

    let peer_manager = PeerManager::start(
        args.p2p_listen,
        args.peers.clone(),
        genesis.chain_id,
        chain_tx.clone(),
        mempool_tx.clone(),
        chain_event_tx.subscribe(),
        mempool_event_tx.subscribe(),
        status_rx.clone(),
        shutdown_rx.clone(),
    )
    .await?;
    let p2p_addr = peer_manager.local_addr();

    let rpc_addr = spawn_rpc_server(
        args.rpc_listen,
        status_rx.clone(),
        mempool_view_rx,
        chain_tx.clone(),
        mempool_tx.clone(),
        engine.clone(),
        peer_manager.clone(),
        miner.clone(),
        shutdown_rx,
    )
    .await?;

    Ok(NodeHandle {
        p2p_addr,
        rpc_addr,
        miner_key_id: miner.key_id(),
        chain_tx,
        mempool_tx,
        status_rx,
        engine,
        peer_manager,
        shutdown_tx,
        tasks,
    })
}
