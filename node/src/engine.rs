//! ConsensusEngine: multi-threaded Fresh Work miner (`node::engine`, §8.5).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a miner waits after a seal for the template built on its block.
const SEAL_HANDOFF: Duration = Duration::from_secs(2);

use chain::core_adapter::{fw_context_to_core, hash_to_digest};
use chain::{Template, top128};
use tokio::sync::{mpsc, watch};
use tracing::error;
use work::{DEFAULT_FREIVALDS_ROUNDS, FreshWorkIds, verify_freivalds_rounds};

use crate::chain_actor::ChainCmd;

/// Mining statistics and runtime controls of the `ConsensusEngine` (§8.5).
pub struct ConsensusEngine {
    threads: usize,
    enabled: AtomicBool,
    /// Closed while `--mine-when-synced` waits for peers and sync; miners run only while
    /// both `enabled` and this hold.
    sync_gate: AtomicBool,
    shutdown: AtomicBool,
    attempts: AtomicU64,
    shares_found: AtomicU64,
    seals_found: AtomicU64,
    rate_bits: AtomicU64, // f64::to_bits of the EWMA attempts/sec
}

impl ConsensusEngine {
    /// Spawns `threads` OS miner threads and the EWMA rate task (§8.5). With
    /// `gated`, the miners wait until [`Self::set_sync_gate`] opens the sync gate.
    pub fn spawn(
        threads: usize,
        initially_enabled: bool,
        gated: bool,
        templates: watch::Receiver<Option<Arc<Template>>>,
        chain_tx: mpsc::Sender<ChainCmd>,
    ) -> std::io::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            threads,
            enabled: AtomicBool::new(initially_enabled && threads > 0),
            sync_gate: AtomicBool::new(!gated),
            shutdown: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
            shares_found: AtomicU64::new(0),
            seals_found: AtomicU64::new(0),
            rate_bits: AtomicU64::new(0.0f64.to_bits()),
        });

        for i in 0..threads {
            let eng = engine.clone();
            let tpl_rx = templates.clone();
            let tx = chain_tx.clone();
            std::thread::Builder::new()
                .name(format!("plau-miner-{i}"))
                .spawn(move || miner_thread_loop(i, threads, &eng, &tpl_rx, &tx))?;
        }

        // EWMA of attempts per second with a 10 s time constant.
        let eng_rate = engine.clone();
        tokio::spawn(async move {
            let mut prev_attempts = eng_rate.attempts.load(Ordering::Relaxed);
            let mut ewma = 0.0f64;
            let alpha = 1.0 - (-1.0f64 / 10.0).exp();
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if eng_rate.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let cur = eng_rate.attempts.load(Ordering::Relaxed);
                let delta = cur.saturating_sub(prev_attempts) as f64;
                prev_attempts = cur;
                ewma = if ewma == 0.0 {
                    delta
                } else {
                    alpha * delta + (1.0 - alpha) * ewma
                };
                eng_rate.rate_bits.store(ewma.to_bits(), Ordering::Relaxed);
            }
        });

        Ok(engine)
    }

    /// Enables or pauses mining on every worker thread (§8.5).
    pub fn set_enabled(&self, enabled: bool) {
        if self.threads > 0 {
            self.enabled.store(enabled, Ordering::Relaxed);
        }
    }

    /// Whether mining is enabled (it may still wait for the sync gate).
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Opens or closes the sync gate (`--mine-when-synced`).
    pub fn set_sync_gate(&self, open: bool) {
        self.sync_gate.store(open, Ordering::Relaxed);
    }

    /// Whether the sync gate lets the miners run.
    pub fn sync_gate_open(&self) -> bool {
        self.sync_gate.load(Ordering::Relaxed)
    }

    /// Number of miner threads.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// EWMA of attempts per second.
    pub fn attempts_per_sec(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    /// Total attempts evaluated.
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed)
    }

    /// Shares found.
    pub fn shares_found(&self) -> u64 {
        self.shares_found.load(Ordering::Relaxed)
    }

    /// Seals found.
    pub fn seals_found(&self) -> u64 {
        self.seals_found.load(Ordering::Relaxed)
    }

    /// Stops every miner thread for good.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.enabled.store(false, Ordering::Relaxed);
    }
}

fn miner_thread_loop(
    thread_idx: usize,
    num_threads: usize,
    engine: &ConsensusEngine,
    templates: &watch::Receiver<Option<Arc<Template>>>,
    chain_tx: &mpsc::Sender<ChainCmd>,
) {
    let stride = num_threads.max(1) as u64;
    let mut generation = None;
    let mut nonce = thread_idx as u64;
    let mut sealed_at: Option<Instant> = None;
    while !engine.shutdown.load(Ordering::Relaxed) {
        if !engine.enabled.load(Ordering::Relaxed) || !engine.sync_gate.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let Some(tpl) = templates.borrow().clone() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        if generation != Some(tpl.generation) {
            generation = Some(tpl.generation);
            nonce = thread_idx as u64;
            sealed_at = None;
        }
        // After a seal, give the ChainState actor time to publish the template built on
        // top of it; if the block did not become the tip, keep mining this template.
        if sealed_at.is_some_and(|t| t.elapsed() < SEAL_HANDOFF) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let Ok(ctx) = fw_context_to_core(&tpl.context_with_nonce(nonce)) else {
            error!("Template context does not fit the core types");
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        let inst = ctx.instance();
        let z = inst.x.product(&inst.y);
        let ids = FreshWorkIds::derive(&ctx, &z);
        engine.attempts.fetch_add(1, Ordering::Relaxed);

        let seal = top128(&hash_to_digest(&ids.seal_ticket)) < tpl.seal_target.0;
        let share = top128(&hash_to_digest(&ids.share_ticket)) < tpl.share_target.0;
        if (seal || share)
            && !verify_freivalds_rounds(&inst.x, &inst.y, &z, DEFAULT_FREIVALDS_ROUNDS)
        {
            error!("Freivalds self-check failed: engine or hardware fault");
        } else if seal {
            // The header share rule (§6.7) covers a sealing attempt; no ShareProof.
            engine.seals_found.fetch_add(1, Ordering::Relaxed);
            match tpl.seal(nonce, &ids) {
                Ok(block) => {
                    let _ = chain_tx.blocking_send(ChainCmd::SubmitLocalBlock(Box::new(block)));
                }
                Err(e) => error!(error = %e, "Could not sign a sealed block"),
            }
            sealed_at = Some(Instant::now());
        } else if share {
            engine.shares_found.fetch_add(1, Ordering::Relaxed);
            match tpl.share(nonce, &ids) {
                Ok(proof) => {
                    let _ = chain_tx.blocking_send(ChainCmd::SubmitLocalShare(Box::new(proof)));
                }
                Err(e) => error!(error = %e, "Could not sign a share proof"),
            }
        }
        nonce = nonce.wrapping_add(stride);
    }
}
