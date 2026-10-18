//! `plauthium-node`: the Plauthium testnet daemon.

#![forbid(unsafe_code)]

use clap::Parser;
use node::NodeError;
use node::cli::{Cli, Command};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), NodeError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Genesis(args) => node::run_genesis_cmd(args)?,
        Command::Run(args) => {
            let handle = node::start_node(args).await?;
            info!(
                p2p = %handle.p2p_addr,
                rpc = %handle.rpc_addr,
                "plauthium-node started; press Ctrl-C to stop"
            );
            tokio::signal::ctrl_c().await?;
            info!("Ctrl-C received; shutting down gracefully");
            handle.stop().await;
            info!("Shutdown complete");
        }
    }
    Ok(())
}
