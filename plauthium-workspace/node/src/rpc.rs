//! Axum JSON RPC and embedded web explorer (`node::rpc`, §9).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chain::{BlockSummary, compute_client_digest};
use ledger::StateCategory;
use primitives::TOTAL_SUPPLY;
use scivm::CONSENSUS_WEIGHT;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use wire::{
    CategoryDelta, DOT_LEN, Digest, FeeTransition, JobPayload, JobTransaction, WIRE_VERSION,
};

use crate::chain_actor::{ChainCmd, Origin, StatusSnapshot};
use crate::engine::ConsensusEngine;
use crate::mempool::{MempoolCmd, MempoolReject, MempoolView};
use crate::net::PeerManager;

const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
struct RpcState {
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    mempool_view: watch::Receiver<Arc<MempoolView>>,
    chain_tx: mpsc::Sender<ChainCmd>,
    mempool_tx: mpsc::Sender<MempoolCmd>,
    engine: Arc<ConsensusEngine>,
    peer_manager: Arc<PeerManager>,
}

/// Spawns the Axum HTTP server and returns the bound local address (§9.1).
#[allow(clippy::too_many_arguments)]
pub async fn spawn_rpc_server(
    listen_addr: SocketAddr,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    mempool_view: watch::Receiver<Arc<MempoolView>>,
    chain_tx: mpsc::Sender<ChainCmd>,
    mempool_tx: mpsc::Sender<MempoolCmd>,
    engine: Arc<ConsensusEngine>,
    peer_manager: Arc<PeerManager>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> std::io::Result<SocketAddr> {
    let state = RpcState {
        status_rx,
        mempool_view,
        chain_tx,
        mempool_tx,
        engine,
        peer_manager,
    };
    let app = Router::new()
        .route("/", get(handle_index))
        .route("/api/status", get(handle_status))
        .route("/api/blocks", get(handle_blocks))
        .route("/api/submit_job", post(handle_submit_job))
        .layer(DefaultBodyLimit::max(65_536))
        .with_state(state);

    let listener = TcpListener::bind(listen_addr).await?;
    let local_addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !*shutdown_rx.borrow() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
    });
    Ok(local_addr)
}

async fn handle_index() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        Html(INDEX_HTML),
    )
}

// ---------------------------------------------------------------------------
// GET /api/status (§9.3)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    chain: ChainInfoDto,
    node: NodeInfoDto,
    tip: TipInfoDto,
    mining: MiningInfoDto,
    mempool: MempoolInfoDto,
    ledger: LedgerInfoDto,
    commercial_consensus_weight: u64,
}

#[derive(Serialize)]
struct ChainInfoDto {
    name: String,
    chain_id: String,
    genesis_id: String,
    wire_version: u32,
    blocks_per_epoch: u64,
    target_block_secs: u64,
}

#[derive(Serialize)]
struct NodeInfoDto {
    owner_id: String,
    peers: usize,
    uptime_secs: u64,
}

#[derive(Serialize)]
struct TipInfoDto {
    height: u64,
    block_id: String,
    time_bucket: u64,
    epoch: u64,
    cumulative_weight: String,
    seal_target: String,
    share_target: String,
    expected_attempts_per_seal: String,
}

#[derive(Serialize)]
struct MiningInfoDto {
    enabled: bool,
    threads: usize,
    attempts_per_sec: f64,
    shares_found: u64,
    seals_found: u64,
}

#[derive(Serialize)]
struct MempoolInfoDto {
    size: usize,
}

#[derive(Serialize)]
struct CategoryBalanceDto {
    symbol: &'static str,
    name: String,
    atoms: String,
}

#[derive(Serialize)]
struct LedgerInfoDto {
    categories: Vec<CategoryBalanceDto>,
    total: String,
    supply_m: String,
    conserved: bool,
    founder_locked: String,
    segregated_dust: String,
    fee_lots: usize,
    locked_fees: usize,
    pending_payout: String,
    open_jobs: usize,
}

async fn handle_status(State(state): State<RpcState>) -> Json<StatusResponse> {
    let snap = state.status_rx.borrow().clone();
    let mempool_size = state.mempool_view.borrow().size;
    let expected_attempts = u128::MAX / snap.tip.seal_target.0.max(1);
    let categories = StateCategory::ALL
        .iter()
        .map(|&c| CategoryBalanceDto {
            symbol: c.symbol(),
            name: format!("{c:?}"),
            atoms: snap.balances[c.index()].to_string(),
        })
        .collect();
    let supply_m = u128::from(TOTAL_SUPPLY.get());

    Json(StatusResponse {
        chain: ChainInfoDto {
            name: snap.chain_name.clone(),
            chain_id: format!("0x{:08x}", snap.chain_id),
            genesis_id: snap.genesis_id.to_hex(),
            wire_version: WIRE_VERSION,
            blocks_per_epoch: snap.blocks_per_epoch,
            target_block_secs: snap.target_block_secs,
        },
        node: NodeInfoDto {
            owner_id: snap.owner_id.to_string(),
            peers: state.peer_manager.peer_count(),
            uptime_secs: snap.start_instant.elapsed().as_secs(),
        },
        tip: TipInfoDto {
            height: snap.tip.height,
            block_id: snap.tip.id.to_hex(),
            time_bucket: snap.tip.time_bucket,
            epoch: snap.epoch,
            cumulative_weight: snap.tip.cumulative_weight.to_string(),
            seal_target: snap.tip.seal_target.to_hex(),
            share_target: snap.tip.share_target.to_hex(),
            expected_attempts_per_seal: expected_attempts.to_string(),
        },
        mining: MiningInfoDto {
            enabled: state.engine.is_enabled(),
            threads: state.engine.threads(),
            attempts_per_sec: (state.engine.attempts_per_sec() * 10.0).round() / 10.0,
            shares_found: state.engine.shares_found(),
            seals_found: state.engine.seals_found(),
        },
        mempool: MempoolInfoDto { size: mempool_size },
        ledger: LedgerInfoDto {
            categories,
            total: snap.total.to_string(),
            supply_m: supply_m.to_string(),
            conserved: snap.total == supply_m,
            founder_locked: snap.founder_locked.to_string(),
            segregated_dust: snap.segregated_dust.to_string(),
            fee_lots: snap.fee_lots,
            locked_fees: snap.locked_fees,
            pending_payout: snap.pending_payout.to_string(),
            open_jobs: snap.open_jobs,
        },
        commercial_consensus_weight: CONSENSUS_WEIGHT,
    })
}

// ---------------------------------------------------------------------------
// GET /api/blocks (§9.4)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BlocksQuery {
    limit: Option<usize>,
    before: Option<u64>,
}

#[derive(Serialize)]
struct BlocksResponse {
    tip_height: u64,
    blocks: Vec<BlockSummaryDto>,
}

#[derive(Serialize)]
struct BlockSummaryDto {
    height: u64,
    block_id: String,
    parent_id: String,
    anchor_id: String,
    miner_owner: String,
    time_bucket: u64,
    attempt_nonce: String,
    epoch: u64,
    seal_ticket: String,
    seal_weight: String,
    cumulative_weight: String,
    jobs: Vec<JobDto>,
    receipts: Vec<ReceiptDto>,
    shares: Vec<ShareDto>,
    header_share: bool,
    fee_transitions: Vec<FeeTransitionDto>,
    balances_after: BTreeMap<&'static str, String>,
    total_after: String,
}

#[derive(Serialize)]
struct JobDto {
    job_id: String,
    tx_id: String,
    executor_owner: String,
    deposit: String,
    queue_fee: String,
    base_fee: String,
}

#[derive(Serialize)]
struct ReceiptDto {
    job_id: String,
    result: String,
    executor_owner: String,
    admitted_at: u64,
}

#[derive(Serialize)]
struct ShareDto {
    work_id: String,
    owner: String,
    height: u64,
    weight: String,
}

#[derive(Serialize)]
struct FeeTransitionDto {
    op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    delta: BTreeMap<&'static str, String>,
}

async fn handle_blocks(
    State(state): State<RpcState>,
    Query(q): Query<BlocksQuery>,
) -> Json<BlocksResponse> {
    let tip_height = state.status_rx.borrow().tip.height;
    let limit = q.limit.unwrap_or(20).min(100);
    let (tx, rx) = oneshot::channel();
    let cmd = ChainCmd::GetBlocks {
        before: q.before,
        limit,
        reply: tx,
    };
    let summaries = if state.chain_tx.send(cmd).await.is_ok() {
        rx.await.unwrap_or_default()
    } else {
        Vec::new()
    };
    Json(BlocksResponse {
        tip_height,
        blocks: summaries.into_iter().map(summary_to_dto).collect(),
    })
}

fn category_map<T: ToString>(values: &[T]) -> BTreeMap<&'static str, String> {
    StateCategory::ALL
        .iter()
        .zip(values)
        .map(|(c, v)| (c.symbol(), v.to_string()))
        .collect()
}

fn fee_transition_dto(ft: &FeeTransition) -> FeeTransitionDto {
    let delta: &CategoryDelta = ft.delta();
    let (op, job_id, epoch) = match ft {
        FeeTransition::ReserveMature { epoch, .. } => ("ReserveMature", None, Some(*epoch)),
        FeeTransition::FwReward { epoch, .. } => ("FwReward", None, Some(*epoch)),
        FeeTransition::EpochBooked { epoch, .. } => ("EpochBooked", None, Some(*epoch)),
        FeeTransition::ResultSettle { job_id, .. } => ("ResultSettle", Some(*job_id), None),
        FeeTransition::EscrowFundAndQueue { job_id, .. } => {
            ("EscrowFundAndQueue", Some(*job_id), None)
        }
        FeeTransition::JobAdmitAndLease { job_id, .. } => ("JobAdmitAndLease", Some(*job_id), None),
    };
    FeeTransitionDto {
        op,
        job_id: job_id.map(|j| j.to_string()),
        epoch,
        delta: category_map(delta),
    }
}

fn summary_to_dto(s: BlockSummary) -> BlockSummaryDto {
    BlockSummaryDto {
        height: s.height,
        block_id: s.block_id.to_hex(),
        parent_id: s.parent_id.to_hex(),
        anchor_id: s.anchor_id.to_hex(),
        miner_owner: s.miner_owner.to_string(),
        time_bucket: s.time_bucket,
        attempt_nonce: s.attempt_nonce.to_string(),
        epoch: s.epoch,
        seal_ticket: s.seal_ticket.to_hex(),
        seal_weight: s.seal_weight.to_string(),
        cumulative_weight: s.cumulative_weight.to_string(),
        jobs: s
            .jobs
            .into_iter()
            .map(|j| JobDto {
                job_id: j.job_id.to_string(),
                tx_id: j.tx_id.to_hex(),
                executor_owner: j.executor_owner.to_string(),
                deposit: j.deposit.to_string(),
                queue_fee: j.queue_fee.to_string(),
                base_fee: j.base_fee.to_string(),
            })
            .collect(),
        receipts: s
            .receipts
            .into_iter()
            .map(|r| ReceiptDto {
                job_id: r.job_id.to_string(),
                result: r.result.to_string(),
                executor_owner: r.executor_owner.to_string(),
                admitted_at: r.admitted_at,
            })
            .collect(),
        shares: s
            .shares
            .into_iter()
            .map(|sh| ShareDto {
                work_id: sh.work_id.to_hex(),
                owner: sh.owner.to_string(),
                height: sh.height,
                weight: sh.weight.to_string(),
            })
            .collect(),
        header_share: s.header_share,
        fee_transitions: s.fee_transitions.iter().map(fee_transition_dto).collect(),
        balances_after: category_map(&s.balances_after),
        total_after: s.total_after.to_string(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/submit_job (§9.5)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SubmitJobRequest {
    client: Option<String>,
    queue_fee: Option<String>,
    base_fee: Option<String>,
    v_exec: Option<String>,
    v_da: Option<String>,
    v_ref: Option<String>,
    expiry_blocks: Option<u64>,
    nonce: Option<String>,
    payload: SubmitJobPayloadDto,
}

#[derive(Deserialize)]
struct SubmitJobPayloadDto {
    #[serde(rename = "type")]
    payload_type: String,
    xs: Vec<i64>,
    ys: Vec<i64>,
}

#[derive(Serialize)]
struct SubmitJobResponse {
    tx_id: String,
    job_id: String,
    deposit: String,
    total_charge: String,
    expected_result: String,
    expiry_height: u64,
    status: &'static str,
}

#[derive(Serialize)]
struct RpcErrorBody {
    error: &'static str,
    message: String,
}

/// An RPC error: HTTP status, stable error code and message (§9.1).
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn invalid_payload(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_payload", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = RpcErrorBody {
            error: self.code,
            message: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}

fn parse_u64_field(opt: Option<String>, default: u64, name: &str) -> Result<u64, ApiError> {
    match opt {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| ApiError::bad_request(format!("{name} must be a decimal u64 string"))),
        None => Ok(default),
    }
}

/// Largest integer magnitude that converts to `f64` exactly.
const MAX_EXACT_INT: u64 = (1u64 << 53) - 1;

fn to_operands(values: &[i64]) -> Result<Vec<f64>, ApiError> {
    values
        .iter()
        .map(|&v| {
            if v.unsigned_abs() > MAX_EXACT_INT {
                Err(ApiError::invalid_payload("operand magnitude exceeds 2^53"))
            } else {
                Ok(v as f64)
            }
        })
        .collect()
}

fn build_job(req: SubmitJobRequest, tip_height: u64) -> Result<JobTransaction, ApiError> {
    let client = match req.client.as_deref() {
        Some(txt) if !txt.is_empty() => {
            compute_client_digest(txt).map_err(ApiError::bad_request)?
        }
        _ => Digest::ZERO,
    };
    // Defaults: the worked budget of WP (7.17), with 1 PLAU = 10^8 atoms.
    let queue_fee = parse_u64_field(req.queue_fee, 100_000_000, "queue_fee")?;
    let base_fee = parse_u64_field(req.base_fee, 1_000_000_000, "base_fee")?;
    let v_exec = parse_u64_field(req.v_exec, 8_000_000_000, "v_exec")?;
    let v_da = parse_u64_field(req.v_da, 500_000_000, "v_da")?;
    let v_ref = parse_u64_field(req.v_ref, 500_000_000, "v_ref")?;

    let expiry_blocks = req.expiry_blocks.unwrap_or(120);
    if !(1..=10_000).contains(&expiry_blocks) {
        return Err(ApiError::bad_request("expiry_blocks must be in 1..=10000"));
    }
    let nonce = match req.nonce {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| ApiError::bad_request("nonce must be a decimal u64 string"))?,
        None => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            nanos ^ 0x9e37_79b9_7f4a_7c15
        }
    };

    if req.payload.payload_type != "exact_dot" {
        return Err(ApiError::invalid_payload(
            "payload.type must be 'exact_dot'",
        ));
    }
    if req.payload.xs.len() != DOT_LEN || req.payload.ys.len() != DOT_LEN {
        return Err(ApiError::invalid_payload(format!(
            "xs and ys must each have {DOT_LEN} elements"
        )));
    }
    Ok(JobTransaction {
        client,
        queue_fee,
        base_fee,
        v_exec,
        v_da,
        v_ref,
        expiry_height: tip_height.saturating_add(expiry_blocks),
        nonce,
        payload: JobPayload::ExactDot {
            xs: to_operands(&req.payload.xs)?,
            ys: to_operands(&req.payload.ys)?,
        },
    })
}

async fn handle_submit_job(
    State(state): State<RpcState>,
    Json(req): Json<SubmitJobRequest>,
) -> Result<(StatusCode, Json<SubmitJobResponse>), ApiError> {
    let tip_height = state.status_rx.borrow().tip.height;
    let tx = build_job(req, tip_height)?;
    let expiry_height = tx.expiry_height;

    let unavailable = |what: &str| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "mempool_full",
            format!("mempool {what}"),
        )
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = MempoolCmd::Insert {
        tx,
        origin: Origin::Rpc,
        reply: reply_tx,
    };
    state
        .mempool_tx
        .send(cmd)
        .await
        .map_err(|_| unavailable("actor unavailable"))?;
    let ok = match reply_rx
        .await
        .map_err(|_| unavailable("reply channel closed"))?
    {
        Ok(ok) => ok,
        Err(MempoolReject::Duplicate) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "duplicate",
                "job transaction already in mempool",
            ));
        }
        Err(MempoolReject::Full) => return Err(unavailable("is full")),
        Err(MempoolReject::Invalid { code, message }) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, code, message));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitJobResponse {
            tx_id: ok.tx_id.to_hex(),
            job_id: ok.job_id.to_string(),
            deposit: ok.deposit.to_string(),
            total_charge: ok.total_charge.to_string(),
            expected_result: ok.expected_result.to_string(),
            expiry_height,
            status: "pending",
        }),
    ))
}
