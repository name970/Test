//! CLI definition (`node::cli`, §1.2, §10).

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const KNOWN_GAPS_HELP: &str = "\
KNOWN TESTNET GAPS (blueprint §1.2; gaps 1, 2 and 4 are closed in v1):
  3. VerifyFW by recomputation: validators recompute 512x512 INT8 tiles (no Circle STARK, no P-DAL).
  5. Commercial execution is replicated: every validator recomputes Ozaki-II dot products (consensus weight 0).
  6. No refunds or timeouts: every admitted job settles deterministically after settle_delay blocks.
  7. Founder vesting is tied to BLOCKS_PER_DAY = 720 blocks regardless of the target block time.
  8. State only grows: matured fee lots are never pruned by the core ledger.
  Note: Circulating (U) is a shared testnet faucet. The core ledger has no per-account balances, so a
  job's ML-DSA-87 signature authenticates its client, but the deposit is debited from the aggregate U.

CLOSED IN V1:
  1. Hashing is SHA3-512 and SHAKE256 (FIPS 202).
  2. Owners are ML-DSA-87 keys (FIPS 204) that sign every block header, share proof and job.
  4. Shares are gossiped (GossipShare): verified shares from every node enter each miner's blocks.";

/// Plauthium multi-node P2P testnet daemon.
#[derive(Debug, Parser)]
#[command(name = "plauthium-node", version, about, after_help = KNOWN_GAPS_HELP)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// `plauthium-node` subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate a consensus genesis JSON file.
    Genesis(GenesisArgs),
    /// Run a Plauthium testnet node.
    Run(RunArgs),
}

/// Difficulty adjustment mode for `genesis`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DifficultyArg {
    /// Retarget every `--retarget-window` blocks towards `--target-block-secs`.
    Windowed,
    /// Keep the initial seal target forever.
    Fixed,
}

/// Arguments of `plauthium-node genesis`.
#[derive(Clone, Debug, Args)]
pub struct GenesisArgs {
    /// Output file path.
    #[arg(long, default_value = "genesis.json")]
    pub out: PathBuf,
    /// Chain human-readable name.
    #[arg(long, default_value = "plauthium-testnet-0")]
    pub name: String,
    /// Expected Fresh Work attempts per block seal.
    #[arg(long, default_value_t = 32)]
    pub seal_attempts: u64,
    /// Share target multiplier ratio (Θ_S / Θ_B).
    #[arg(long, default_value_t = 4)]
    pub share_ratio: u64,
    /// Difficulty adjustment mode.
    #[arg(long, value_enum, default_value_t = DifficultyArg::Windowed)]
    pub difficulty: DifficultyArg,
    /// Window size in blocks when difficulty is windowed.
    #[arg(long, default_value_t = 32)]
    pub retarget_window: u64,
    /// Target block interval in seconds.
    #[arg(long, default_value_t = 10)]
    pub target_block_secs: u64,
    /// Number of blocks per reward epoch.
    #[arg(long, default_value_t = 30)]
    pub blocks_per_epoch: u64,
    /// Number of blocks after admission when a commercial job settles.
    #[arg(long, default_value_t = 2)]
    pub settle_delay: u64,
}

/// Arguments of `plauthium-node run`.
#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    /// Node data directory (stores blocks.log and, by default, the miner key).
    #[arg(long)]
    pub data_dir: PathBuf,
    /// ML-DSA-87 miner key file (default: <DATA_DIR>/miner_key.bin). Created with a fresh
    /// key on the first run and loaded on every later run; its key signs this node's
    /// blocks, shares and RPC-submitted jobs.
    #[arg(long)]
    pub key_file: Option<PathBuf>,
    /// Optional path to a custom genesis.json file (default: embedded testnet genesis).
    #[arg(long)]
    pub genesis: Option<PathBuf>,
    /// TCP listen address for the P2P protocol.
    #[arg(long, default_value = "0.0.0.0:30333")]
    pub p2p_listen: SocketAddr,
    /// HTTP listen address for the JSON RPC and web explorer.
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub rpc_listen: SocketAddr,
    /// Static peer socket addresses (may be passed multiple times).
    #[arg(long = "peer")]
    pub peers: Vec<SocketAddr>,
    /// Number of OS threads for Fresh Work tile mining.
    #[arg(long, default_value_t = 1)]
    pub mining_threads: usize,
    /// Disable block mining on this node.
    #[arg(long, default_value_t = false)]
    pub no_mine: bool,
    /// Number of concurrent Stage A verification tasks (default: CPUs / 2). Gossiped
    /// shares get half as many again (at least one) for their work checks.
    #[arg(long)]
    pub verify_threads: Option<usize>,
    /// Delete the chain data in the data directory before starting. The miner key file
    /// is kept, so the node keeps its identity.
    #[arg(long, default_value_t = false)]
    pub reset: bool,
}
