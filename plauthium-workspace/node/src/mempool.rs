//! Mempool actor (`node::mempool`, §8.4).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chain::{JobValidationError, compute_job_ids, validate_job_stateless};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::warn;
use wire::{Block, Digest, JobTransaction, MEMPOOL_CAPACITY};

use crate::chain_actor::{ChainCmd, ChainEvent, Origin, StatusSnapshot};

/// FIFO snapshot of pooled transactions, published to the ChainState actor for templates.
#[derive(Clone, Debug, Default)]
pub struct MempoolView {
    /// Oldest first, at most `4 * max_jobs_per_block` entries.
    pub txs: Vec<JobTransaction>,
    /// Total number of pooled transactions.
    pub size: usize,
}

/// Event broadcast by `MempoolActor` when a job is accepted.
#[derive(Clone, Debug)]
pub enum MempoolEvent {
    /// A new job entered the pool (relayed to peers except `origin`).
    Accepted {
        /// The job.
        tx: JobTransaction,
        /// Where it came from.
        origin: Origin,
    },
}

/// Successful insertion receipt returned to RPC and P2P callers.
#[derive(Clone, Debug)]
pub struct MempoolInsertOk {
    /// Transaction identifier.
    pub tx_id: Digest,
    /// Job identifier.
    pub job_id: u64,
    /// Escrow deposit `D`.
    pub deposit: u64,
    /// `D + f_queue`.
    pub total_charge: u64,
    /// Exact dot product the job will settle with.
    pub expected_result: i128,
}

/// Rejection reasons of `MempoolCmd::Insert`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MempoolReject {
    /// Same `tx_id` or `job_id` already pooled.
    Duplicate,
    /// The pool holds `MEMPOOL_CAPACITY` transactions (no fee-based eviction).
    Full,
    /// The transaction is invalid.
    Invalid {
        /// Stable error code for the RPC (§9.5).
        code: &'static str,
        /// Human-readable reason.
        message: String,
    },
}

/// Commands accepted by `MempoolActor`.
pub enum MempoolCmd {
    /// Validates and pools a job.
    Insert {
        /// The job.
        tx: JobTransaction,
        /// Where it came from.
        origin: Origin,
        /// Outcome.
        reply: oneshot::Sender<Result<MempoolInsertOk, MempoolReject>>,
    },
    /// Number of pooled transactions.
    Stats {
        /// Pool size.
        reply: oneshot::Sender<usize>,
    },
    /// The oldest `limit` pooled transactions.
    List {
        /// Maximum entries.
        limit: usize,
        /// Transactions, oldest first.
        reply: oneshot::Sender<Vec<JobTransaction>>,
    },
}

struct PooledEntry {
    tx_id: Digest,
    job_id: u64,
    tx: JobTransaction,
}

/// Asynchronous mempool actor (§8.4).
pub struct MempoolActor {
    cmd_rx: mpsc::Receiver<MempoolCmd>,
    chain_tx: mpsc::Sender<ChainCmd>,
    chain_events: broadcast::Receiver<ChainEvent>,
    event_tx: broadcast::Sender<MempoolEvent>,
    view_tx: watch::Sender<Arc<MempoolView>>,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    fifo: VecDeque<PooledEntry>,
    by_tx_id: HashSet<Digest>,
    by_job_id: HashMap<u64, Digest>,
}

fn rejection_code(err: &JobValidationError) -> &'static str {
    match err {
        JobValidationError::ZeroFee(_) => "zero_fee",
        JobValidationError::AmountOverflow => "amount_overflow",
        JobValidationError::TooLarge(_) => "too_large",
        JobValidationError::InvalidPayload(_) => "invalid_payload",
    }
}

impl MempoolActor {
    /// Creates the actor; call [`MempoolActor::run`] to start it.
    pub fn new(
        cmd_rx: mpsc::Receiver<MempoolCmd>,
        chain_tx: mpsc::Sender<ChainCmd>,
        chain_events: broadcast::Receiver<ChainEvent>,
        event_tx: broadcast::Sender<MempoolEvent>,
        view_tx: watch::Sender<Arc<MempoolView>>,
        status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    ) -> Self {
        Self {
            cmd_rx,
            chain_tx,
            chain_events,
            event_tx,
            view_tx,
            status_rx,
            fifo: VecDeque::new(),
            by_tx_id: HashSet::new(),
            by_job_id: HashMap::new(),
        }
    }

    /// Runs until `shutdown` turns true (or its sender is dropped).
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            tokio::select! {
                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                Some(cmd) = self.cmd_rx.recv() => self.handle_cmd(cmd),
                ev = self.chain_events.recv() => match ev {
                    Ok(ChainEvent::TipChanged { tip, connected, disconnected }) => {
                        self.handle_tip_changed(tip.height, &connected, &disconnected);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "Mempool lagged behind chain events; asking ChainState");
                        self.recover_from_lag().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    fn handle_cmd(&mut self, cmd: MempoolCmd) {
        match cmd {
            MempoolCmd::Insert { tx, origin, reply } => {
                let _ = reply.send(self.insert_tx(tx, origin));
            }
            MempoolCmd::Stats { reply } => {
                let _ = reply.send(self.fifo.len());
            }
            MempoolCmd::List { limit, reply } => {
                let _ = reply.send(self.fifo.iter().take(limit).map(|e| e.tx.clone()).collect());
            }
        }
    }

    fn next_height(&self) -> u64 {
        self.status_rx.borrow().tip.height + 1
    }

    fn insert_tx(
        &mut self,
        tx: JobTransaction,
        origin: Origin,
    ) -> Result<MempoolInsertOk, MempoolReject> {
        let (tx_id, job_id, deposit, expected_result) =
            validate_job_stateless(&tx).map_err(|err| MempoolReject::Invalid {
                code: rejection_code(&err),
                message: err.to_string(),
            })?;
        let total_charge = tx
            .checked_total_charge()
            .ok_or_else(|| MempoolReject::Invalid {
                code: "amount_overflow",
                message: "total charge overflow".to_string(),
            })?;
        let next_height = self.next_height();
        if tx.expiry_height < next_height {
            return Err(MempoolReject::Invalid {
                code: "bad_request",
                message: format!(
                    "job expiry_height {} < next block height {next_height}",
                    tx.expiry_height
                ),
            });
        }
        if self.by_tx_id.contains(&tx_id) || self.by_job_id.contains_key(&job_id) {
            return Err(MempoolReject::Duplicate);
        }
        if self.fifo.len() >= MEMPOOL_CAPACITY {
            return Err(MempoolReject::Full);
        }

        self.push(tx_id, job_id, tx.clone());
        self.publish_view();
        let _ = self.event_tx.send(MempoolEvent::Accepted { tx, origin });
        Ok(MempoolInsertOk {
            tx_id,
            job_id,
            deposit,
            total_charge,
            expected_result,
        })
    }

    fn push(&mut self, tx_id: Digest, job_id: u64, tx: JobTransaction) {
        self.by_tx_id.insert(tx_id);
        self.by_job_id.insert(job_id, tx_id);
        self.fifo.push_back(PooledEntry { tx_id, job_id, tx });
    }

    fn handle_tip_changed(
        &mut self,
        tip_height: u64,
        connected: &[Arc<Block>],
        disconnected: &[Arc<Block>],
    ) {
        let connected_txs: HashSet<Digest> = connected
            .iter()
            .flat_map(|blk| &blk.body.jobs)
            .filter_map(|tx| compute_job_ids(tx).ok().map(|(id, _)| id))
            .collect();
        let next_height = tip_height + 1;

        // Remove txs included by the new branch, and txs that expired.
        self.remove_where(|e| connected_txs.contains(&e.tx_id) || e.tx.expiry_height < next_height);

        // Return the txs of the abandoned branch (already validated statelessly).
        for tx in disconnected.iter().flat_map(|blk| &blk.body.jobs) {
            if tx.expiry_height < next_height || self.fifo.len() >= MEMPOOL_CAPACITY {
                continue;
            }
            let Ok((tx_id, job_id)) = compute_job_ids(tx) else {
                continue;
            };
            if connected_txs.contains(&tx_id)
                || self.by_tx_id.contains(&tx_id)
                || self.by_job_id.contains_key(&job_id)
            {
                continue;
            }
            self.push(tx_id, job_id, tx.clone());
        }
        self.publish_view();
    }

    async fn recover_from_lag(&mut self) {
        let tx_ids: Vec<Digest> = self.by_tx_id.iter().copied().collect();
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = ChainCmd::FilterIncluded {
            tx_ids,
            reply: reply_tx,
        };
        if self.chain_tx.send(request).await.is_err() {
            return;
        }
        let Ok(included) = reply_rx.await else {
            return;
        };
        let included: HashSet<Digest> = included.into_iter().collect();
        let next_height = self.next_height();
        self.remove_where(|e| included.contains(&e.tx_id) || e.tx.expiry_height < next_height);
        self.publish_view();
    }

    fn remove_where(&mut self, pred: impl Fn(&PooledEntry) -> bool) {
        let (by_tx_id, by_job_id) = (&mut self.by_tx_id, &mut self.by_job_id);
        self.fifo.retain(|entry| {
            let remove = pred(entry);
            if remove {
                by_tx_id.remove(&entry.tx_id);
                by_job_id.remove(&entry.job_id);
            }
            !remove
        });
    }

    fn publish_view(&self) {
        let max_jobs = usize::try_from(self.status_rx.borrow().max_jobs_per_block).unwrap_or(32);
        let cap = max_jobs.saturating_mul(4).max(32);
        let txs = self.fifo.iter().take(cap).map(|e| e.tx.clone()).collect();
        let _ = self.view_tx.send(Arc::new(MempoolView {
            txs,
            size: self.fifo.len(),
        }));
    }
}
