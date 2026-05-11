use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::AppConfig,
    contract::{hex_b256, WorkState},
    metrics::Metrics,
    nonce_space::{prefix_hex, NonceAllocator},
    rpc_pool::{RpcHealth, RpcPool},
    state_watcher::StateWatcher,
    submitter::{SolutionResult, SolutionSubmit, Submitter},
};

#[derive(Clone)]
pub struct CoordinatorApp {
    cfg: AppConfig,
    rpc: RpcPool,
    watcher: StateWatcher,
    submitter: Arc<Submitter>,
    metrics: Arc<Metrics>,
    workers: Arc<RwLock<HashMap<String, WorkerInfo>>>,
    seen: Arc<Mutex<SeenState>>,
    latest: Arc<RwLock<Option<WorkState>>>,
    next_worker_seq: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
struct WorkerInfo {
    #[allow(dead_code)]
    assigned_worker_id: String,
    #[allow(dead_code)]
    miner_id: String,
    device_name: String,
    allocator: NonceAllocator,
    last_attempts: u64,
    last_hashrate: f64,
}

#[derive(Default)]
struct SeenState {
    epoch: u64,
    keys: HashSet<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub miner_id: String,
    pub hostname: Option<String>,
    pub device_name: Option<String>,
    pub gpu_uuid: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub assigned_worker_id: String,
    pub nonce_prefix_namespace: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkQuery {
    pub worker_id: String,
}

#[derive(Debug, Serialize)]
pub struct WorkResponse {
    pub miner_address: String,
    pub block_number: u64,
    pub epoch: u64,
    pub challenge: String,
    pub target: String,
    pub effective_target: String,
    pub nonce_prefix_high_192: String,
    pub base_counter64: u64,
    pub batch_hint: u64,
    pub expires_at_block: u64,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub version: String,
    pub block: u64,
    pub epoch: u64,
    pub challenge: String,
    pub target: String,
    pub queue_size: usize,
    pub workers: usize,
    pub rpc_health: Vec<RpcHealth>,
    pub submit_health: Vec<RpcHealth>,
}

impl CoordinatorApp {
    pub fn new(
        cfg: AppConfig,
        rpc: RpcPool,
        watcher: StateWatcher,
        submitter: Submitter,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            cfg,
            rpc,
            watcher,
            submitter: Arc::new(submitter),
            metrics,
            workers: Arc::new(RwLock::new(HashMap::new())),
            seen: Arc::new(Mutex::new(SeenState::default())),
            latest: Arc::new(RwLock::new(None)),
            next_worker_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn run(self) -> Result<()> {
        // State propagation: subscribe and mirror watcher into self.latest.
        let mut rx = self.watcher.subscribe();
        let latest = self.latest.clone();
        let seen_per_epoch = self.seen.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let value = rx.borrow().clone();
                if let Some(ref s) = value {
                    let mut seen = seen_per_epoch.lock().await;
                    if seen.epoch != s.epoch {
                        seen.epoch = s.epoch;
                        seen.keys.clear();
                    }
                }
                *latest.write().await = value;
            }
        });
        // Run state watcher poller.
        let watcher = self.watcher.clone();
        tokio::spawn(async move { watcher.run().await });
        // Background submitter tasks (receipt watch + replacement).
        let s = self.submitter.clone();
        tokio::spawn(async move { s.receipt_watcher_loop().await });
        let s = self.submitter.clone();
        tokio::spawn(async move { s.replacement_loop().await });
        // Metrics HTTP server.
        let metrics = self.metrics.clone();
        let bind = self.cfg.metrics_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::metrics::serve_metrics(bind, metrics).await {
                tracing::warn!("metrics server: {e}");
            }
        });
        // Periodic worker hashrate roll-up to metrics.
        {
            let workers = self.workers.clone();
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    let w = workers.read().await;
                    for (idx, info) in w.values().enumerate() {
                        metrics.record_device(
                            idx,
                            &info.device_name,
                            info.last_attempts,
                            info.last_hashrate,
                        );
                    }
                }
            });
        }

        let addr: SocketAddr = self.cfg.coordinator_bind.parse()?;
        let app = Router::new()
            .route("/health", get(health))
            .route("/register", post(register))
            .route("/work", get(work))
            .route("/solution", post(solution))
            .route("/metrics", get(metrics_handler))
            .with_state(self);
        tracing::info!("coordinator listening on {addr}");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }

    fn auth(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.cfg.worker_auth_token else {
            return true;
        };
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {expected}"))
            .unwrap_or(false)
    }
}

async fn health(State(app): State<CoordinatorApp>) -> Response {
    let latest = app.latest.read().await.clone();
    let rpc_health = app.rpc.read_health().await;
    let submit_health = app.rpc.submit_health().await;
    let workers = app.workers.read().await.len();
    let queue_size = app.seen.lock().await.keys.len();
    let h = HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        block: latest.as_ref().map(|s| s.block_number).unwrap_or(0),
        epoch: latest.as_ref().map(|s| s.epoch).unwrap_or(0),
        challenge: latest
            .as_ref()
            .map(|s| hex_b256(&s.challenge))
            .unwrap_or_default(),
        target: latest
            .as_ref()
            .map(|s| s.target.to_string())
            .unwrap_or_default(),
        queue_size,
        workers,
        rpc_health,
        submit_health,
    };
    Json(h).into_response()
}

async fn register(
    State(app): State<CoordinatorApp>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Response {
    if !app.auth(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let state = match app.latest.read().await.clone() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "state not ready").into_response(),
    };
    let seq = app.next_worker_seq.fetch_add(1, Ordering::Relaxed) + 1;
    let id = format!("w{seq}");
    let seed = format!(
        "{}|{}|{}|{}|{seq}",
        req.miner_id,
        req.hostname.clone().unwrap_or_default(),
        req.device_name.clone().unwrap_or_default(),
        req.gpu_uuid.clone().unwrap_or_default(),
    );
    let allocator = NonceAllocator::new(&seed, seq as u32, state.epoch, &state.challenge);
    let ns = prefix_hex(allocator.prefix());
    let info = WorkerInfo {
        assigned_worker_id: id.clone(),
        miner_id: req.miner_id,
        device_name: req.device_name.unwrap_or_default(),
        allocator,
        last_attempts: 0,
        last_hashrate: 0.0,
    };
    app.workers.write().await.insert(id.clone(), info);
    tracing::info!(worker = %id, namespace = %ns, "worker registered");
    Json(RegisterResponse {
        assigned_worker_id: id,
        nonce_prefix_namespace: ns,
    })
    .into_response()
}

async fn work(
    State(app): State<CoordinatorApp>,
    headers: HeaderMap,
    Query(q): Query<WorkQuery>,
) -> Response {
    if !app.auth(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let state = match app.latest.read().await.clone() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "state not ready").into_response(),
    };
    let mut workers = app.workers.write().await;
    let Some(w) = workers.get_mut(&q.worker_id) else {
        return (StatusCode::NOT_FOUND, "unknown worker").into_response();
    };
    // Re-seed allocator on epoch change so prefix carries fresh epoch hash.
    if w.allocator.epoch_changed(state.epoch, &state.challenge) {
        w.allocator.rebind_epoch(state.epoch, &state.challenge);
    }
    let batch = w.allocator.next_batch(app.cfg.gpu_batch);
    Json(WorkResponse {
        miner_address: format!("{:#x}", state.miner_address),
        block_number: state.block_number,
        epoch: state.epoch,
        challenge: hex_b256(&state.challenge),
        target: state.target.to_string(),
        effective_target: state.effective_target.to_string(),
        nonce_prefix_high_192: prefix_hex(batch.prefix),
        base_counter64: batch.base_counter64,
        batch_hint: batch.batch_size,
        expires_at_block: state.block_number.saturating_add(state.blocks_left.max(1).min(8)),
    })
    .into_response()
}

async fn solution(
    State(app): State<CoordinatorApp>,
    headers: HeaderMap,
    Json(sol): Json<SolutionSubmit>,
) -> Response {
    if !app.auth(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let state = match app.latest.read().await.clone() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "state not ready").into_response(),
    };
    // Update worker hashrate stats.
    {
        let mut workers = app.workers.write().await;
        if let Some(w) = workers.get_mut(&sol.worker_id) {
            w.last_attempts = sol.attempts;
            w.last_hashrate = sol.hashrate;
        }
    }
    let key = format!(
        "{}:{}:{}",
        hex_b256(&sol.challenge),
        sol.nonce,
        state.miner_address
    );
    {
        let mut seen = app.seen.lock().await;
        if !seen.keys.insert(key) {
            app.metrics.rejected_solutions.fetch_add(1, Ordering::Relaxed);
            return Json(SolutionResult {
                accepted: false,
                reason: "duplicate".into(),
                tx_nonce: None,
                tx_hash: None,
            })
            .into_response();
        }
    }
    if sol.epoch != state.epoch || sol.challenge != state.challenge {
        app.metrics.stale_solutions.fetch_add(1, Ordering::Relaxed);
        return Json(SolutionResult {
            accepted: false,
            reason: "stale".into(),
            tx_nonce: None,
            tx_hash: None,
        })
        .into_response();
    }
    match app.submitter.submit_solution(sol, state).await {
        Ok(r) => {
            app.metrics.accepted_solutions.fetch_add(1, Ordering::Relaxed);
            Json(r).into_response()
        }
        Err(e) => {
            app.metrics.rejected_solutions.fetch_add(1, Ordering::Relaxed);
            Json(SolutionResult {
                accepted: false,
                reason: e.to_string(),
                tx_nonce: None,
                tx_hash: None,
            })
            .into_response()
        }
    }
}

async fn metrics_handler(State(app): State<CoordinatorApp>) -> String {
    app.metrics.render_full()
}
