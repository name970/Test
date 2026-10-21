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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use wire::{DifficultyMode, Digest, GenesisConfig, Target};

use crate::chain_actor::{ChainActor, ChainCmd, ChainEvent, StatusSnapshot};
use crate::cli::{DifficultyArg, GenesisArgs, KeyArgs, RunArgs};
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
    /// Held until the node is dropped: no second node can use the data directory.
    _data_dir_lock: std::fs::File,
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

/// `plauthium-node key`: loads the miner key, creating it first if it does not exist,
/// and prints where it is and the identity it gives the node.
pub fn run_key_cmd(args: KeyArgs) -> Result<(), NodeError> {
    let key_path = keystore::key_path(&args.data_dir, args.key_file.as_deref());
    let (key, created) = keystore::load_or_create(&key_path)?;
    println!("key_file: {}", key_path.display());
    println!("status: {}", if created { "created" } else { "loaded" });
    println!("key_id: {}", key.key_id().to_hex());
    println!("owner_id: {}", key.owner_id());
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

/// Grace period before warning that no peer is connected, and the interval between
/// such warnings.
const NO_PEER_GRACE: Duration = Duration::from_secs(30);
const NO_PEER_WARNING_INTERVAL: Duration = Duration::from_secs(60);
/// With `--mine-when-synced`, mining also starts once the tip has not moved for this
/// long while a peer is connected: a peer may claim a height it cannot deliver.
const SYNC_STALL: Duration = Duration::from_secs(60);

/// Watches the connection to the network. A node with static peers warns while none is
/// connected. With `--mine-when-synced` (`gated`) it also opens the mining gate once a
/// peer is connected and the node has caught up with the best height peers report, or
/// syncing has stalled; the gate then stays open until no peer is left.
fn spawn_peer_watch(
    engine: Arc<ConsensusEngine>,
    peers: Arc<PeerManager>,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    mut shutdown: watch::Receiver<bool>,
    gated: bool,
    has_static_peers: bool,
) {
    if !gated && !has_static_peers {
        return;
    }
    tokio::spawn(async move {
        let mut alone_since = Instant::now();
        let mut last_warning: Option<Instant> = None;
        let mut progress = (status_rx.borrow().tip.height, Instant::now());
        loop {
            let tip = status_rx.borrow().tip.height;
            if tip != progress.0 {
                progress = (tip, Instant::now());
            }
            let connected = peers.peer_count() > 0;
            if connected {
                alone_since = Instant::now();
            } else {
                // Only time spent connected without progress counts as a stalled sync.
                progress = (tip, Instant::now());
                if has_static_peers
                    && alone_since.elapsed() >= NO_PEER_GRACE
                    && last_warning.is_none_or(|t| t.elapsed() >= NO_PEER_WARNING_INTERVAL)
                {
                    warn!(
                        mining_paused = gated,
                        "No peer connected; check the --peer addresses and the network"
                    );
                    last_warning = Some(Instant::now());
                }
            }
            if gated {
                let caught_up = peers.best_peer_height().is_some_and(|best| tip >= best);
                let stalled = progress.1.elapsed() >= SYNC_STALL;
                let open = connected && (engine.sync_gate_open() || caught_up || stalled);
                if open != engine.sync_gate_open() {
                    engine.set_sync_gate(open);
                    if open {
                        info!(height = tip, "Connected and caught up; mining");
                    } else {
                        info!("No peer connected; mining paused until the node is caught up again");
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

/// Starts all actors, P2P networking, the ConsensusEngine and the Axum RPC for one node.
pub async fn start_node(args: RunArgs) -> Result<NodeHandle, NodeError> {
    let profile_check = chain::testnet_ozaki_profile().validate();
    profile_check.map_err(|e| format!("TESTNET_PROFILE validation failed: {e}"))?;
    let genesis = load_genesis(&args)?;

    let key_path = keystore::key_path(&args.data_dir, args.key_file.as_deref());
    let data_dir_lock = keystore::lock_data_dir(&args.data_dir)?;
    if args.reset {
        keystore::reset_data_dir(&args.data_dir, &key_path)?;
    }
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
    let gated = args.mine_when_synced && mining_threads > 0;
    let engine = ConsensusEngine::spawn(
        mining_threads,
        mining_threads > 0,
        gated,
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
    spawn_peer_watch(
        engine.clone(),
        peer_manager.clone(),
        status_rx.clone(),
        shutdown_rx.clone(),
        gated,
        !args.peers.is_empty(),
    );

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
        _data_dir_lock: data_dir_lock,
    })
}
