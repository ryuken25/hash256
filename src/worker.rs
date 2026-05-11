use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use alloy::primitives::{B256, U256};
use eyre::{eyre, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    contract::{hash_below_target, hash_nonce},
    nonce_space::{parse_prefix_hex, pow_nonce_from_parts, NoncePrefix},
    submitter::SolutionSubmit,
};

#[derive(Debug, Clone, Deserialize)]
struct RegisterResponse {
    assigned_worker_id: String,
    nonce_prefix_namespace: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkResponse {
    miner_address: String,
    block_number: u64,
    epoch: u64,
    challenge: String,
    target: String,
    effective_target: String,
    nonce_prefix_high_192: String,
    base_counter64: u64,
    batch_hint: u64,
    expires_at_block: u64,
}

#[derive(Debug, Serialize)]
struct RegisterRequest<'a> {
    miner_id: &'a str,
    hostname: String,
    device_name: String,
    gpu_uuid: String,
    version: &'a str,
}

pub async fn run_worker(cfg: AppConfig) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into());

    // Register once.
    let reg: RegisterResponse = retry_register(&client, &cfg, &host).await?;
    tracing::info!(
        worker = %reg.assigned_worker_id,
        namespace = %reg.nonce_prefix_namespace,
        "worker registered with coordinator"
    );

    // For multi-GPU, spawn one task per device index. Each task has its own GpuMiner.
    let device_indices = if cfg.gpu_indices.is_empty() {
        vec![0usize]
    } else {
        cfg.gpu_indices.clone()
    };
    let mut handles = Vec::new();
    for (i, dev_idx) in device_indices.iter().enumerate() {
        let cfg = cfg.clone();
        let client = client.clone();
        let worker_id = reg.assigned_worker_id.clone();
        let dev_idx = *dev_idx;
        let logical_id = i as u32 + 1;
        handles.push(tokio::spawn(async move {
            if let Err(e) = run_device_loop(cfg, client, worker_id, dev_idx, logical_id).await {
                tracing::error!(device_index = dev_idx, "device loop exited: {e}");
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

async fn retry_register(
    client: &Client,
    cfg: &AppConfig,
    host: &str,
) -> Result<RegisterResponse> {
    let url = format!("{}/register", cfg.coordinator_url.trim_end_matches('/'));
    let body = RegisterRequest {
        miner_id: &cfg.miner_id,
        hostname: host.to_string(),
        device_name: "opencl".into(),
        gpu_uuid: "".into(),
        version: env!("CARGO_PKG_VERSION"),
    };
    let mut backoff = 1u64;
    loop {
        let req = authed(client.post(&url), cfg).json(&body);
        match req.send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.json::<RegisterResponse>().await {
                Ok(r) => return Ok(r),
                Err(e) => tracing::warn!("register decode failed: {e}"),
            },
            Err(e) => tracing::warn!("register failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn fetch_work(client: &Client, cfg: &AppConfig, worker_id: &str) -> Result<WorkResponse> {
    let url = format!(
        "{}/work?worker_id={}",
        cfg.coordinator_url.trim_end_matches('/'),
        worker_id
    );
    let resp = authed(client.get(&url), cfg)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

async fn post_solution(
    client: &Client,
    cfg: &AppConfig,
    sol: &SolutionSubmit,
) -> Result<serde_json::Value> {
    let url = format!("{}/solution", cfg.coordinator_url.trim_end_matches('/'));
    let resp = authed(client.post(&url), cfg).json(sol).send().await?;
    Ok(resp.json().await?)
}

fn authed(builder: reqwest::RequestBuilder, cfg: &AppConfig) -> reqwest::RequestBuilder {
    if let Some(t) = &cfg.worker_auth_token {
        builder.bearer_auth(t)
    } else {
        builder
    }
}

#[cfg(feature = "gpu")]
async fn run_device_loop(
    cfg: AppConfig,
    client: Client,
    worker_id: String,
    dev_idx: usize,
    logical_id: u32,
) -> Result<()> {
    use crate::gpu::{GpuMiner, MineOutcome};

    // Build GPU miner once and reuse across iterations.
    let miner_arc = std::sync::Arc::new(std::sync::Mutex::new(GpuMiner::new_for_index(
        dev_idx,
        Some(cfg.gpu_batch as usize),
        cfg.auto_tune_batch,
        cfg.auto_tune_target_ms,
        cfg.local_size,
    )?));
    {
        let m = miner_arc.lock().unwrap();
        tracing::info!(
            device_index = dev_idx,
            device_name = m.device_name(),
            batch = m.batch_size(),
            "gpu device ready"
        );
        m.self_test().map_err(|e| eyre!("gpu self-test: {e}"))?;
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let attempts_total = Arc::new(AtomicU64::new(0));
    let watch_token = Arc::new(AtomicU64::new(0));

    // Spawn a poller that bumps watch_token when challenge/epoch changes.
    let poll_token = watch_token.clone();
    let poll_client = client.clone();
    let poll_cfg = cfg.clone();
    let poll_worker = worker_id.clone();
    let last_challenge_arc = Arc::new(std::sync::Mutex::new(String::new()));
    let last_epoch_arc = Arc::new(std::sync::Mutex::new(0u64));
    let lc1 = last_challenge_arc.clone();
    let le1 = last_epoch_arc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(poll_cfg.state_check_interval_ms.max(200))).await;
            if let Ok(w) = fetch_work(&poll_client, &poll_cfg, &poll_worker).await {
                let mut lc = lc1.lock().unwrap();
                let mut le = le1.lock().unwrap();
                if *lc != w.challenge || *le != w.epoch {
                    *lc = w.challenge.clone();
                    *le = w.epoch;
                    poll_token.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });

    loop {
        let work = match fetch_work(&client, &cfg, &worker_id).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("fetch_work: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        {
            let mut lc = last_challenge_arc.lock().unwrap();
            let mut le = last_epoch_arc.lock().unwrap();
            if *lc != work.challenge || *le != work.epoch {
                *lc = work.challenge.clone();
                *le = work.epoch;
            }
        }
        let token_snap = watch_token.load(Ordering::Relaxed);
        let challenge: B256 = match work.challenge.parse() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("bad challenge {}: {e}", work.challenge);
                continue;
            }
        };
        let effective_target = match U256::from_str_radix(&work.effective_target, 10) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("bad effective_target: {e}");
                continue;
            }
        };
        let prefix = match parse_prefix_hex(&work.nonce_prefix_high_192) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("bad prefix: {e}");
                continue;
            }
        };
        let counter_start = work.base_counter64;
        let attempts_local = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let miner = miner_arc.clone();
        let stop = stop_flag.clone();
        let token_for_mine = watch_token.clone();
        let attempts_total_clone = attempts_total.clone();
        let attempts_for_thread = attempts_local.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut m = miner.lock().unwrap();
            m.mine_until(
                challenge,
                effective_target,
                prefix,
                counter_start,
                stop,
                attempts_for_thread,
                Some((token_for_mine, token_snap)),
            )
        })
        .await
        .map_err(|e| eyre!("join: {e}"))??;
        let secs = start.elapsed().as_secs_f64().max(0.001);
        let attempts = attempts_local.load(Ordering::Relaxed);
        attempts_total_clone.fetch_add(attempts, Ordering::Relaxed);
        let hashrate = attempts as f64 / secs;
        match outcome {
            MineOutcome::Found { nonce, .. } => {
                let h = hash_nonce(&challenge, nonce);
                if !hash_below_target(&h, effective_target) {
                    tracing::warn!("self-check failed before submission; skipping");
                    continue;
                }
                tracing::info!(
                    device_index = dev_idx,
                    logical_id,
                    nonce = %nonce,
                    hashrate = format!("{hashrate:.2}"),
                    "found solution; submitting"
                );
                let sol = SolutionSubmit {
                    worker_id: worker_id.clone(),
                    device_id: logical_id,
                    epoch: work.epoch,
                    challenge,
                    nonce,
                    hash: h,
                    attempts,
                    hashrate,
                    found_at_block: work.block_number,
                };
                match post_solution(&client, &cfg, &sol).await {
                    Ok(resp) => tracing::info!("solution response: {resp}"),
                    Err(e) => tracing::warn!("solution post failed: {e}"),
                }
            }
            MineOutcome::Aborted { .. } => {
                tracing::debug!(
                    device_index = dev_idx,
                    "mining aborted (stale work); fetching new"
                );
            }
        }

        let _ = (work.miner_address, work.target, work.batch_hint, work.expires_at_block);
    }
}

#[cfg(not(feature = "gpu"))]
async fn run_device_loop(
    cfg: AppConfig,
    client: Client,
    worker_id: String,
    dev_idx: usize,
    logical_id: u32,
) -> Result<()> {
    tracing::warn!(
        device_index = dev_idx,
        "binary built without `gpu` feature; using CPU fallback (very slow)"
    );
    loop {
        let work = match fetch_work(&client, &cfg, &worker_id).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("fetch_work: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let challenge: B256 = work.challenge.parse()?;
        let effective_target = U256::from_str_radix(&work.effective_target, 10)?;
        let prefix = parse_prefix_hex(&work.nonce_prefix_high_192)?;
        let base = work.base_counter64;
        let batch = work.batch_hint.min(cfg.gpu_batch).min(2_000_000);
        let mut found = None;
        let mut attempts = 0u64;
        let start = Instant::now();
        for i in 0..batch {
            let n = pow_nonce_from_parts(prefix, base.wrapping_add(i));
            attempts += 1;
            let h = hash_nonce(&challenge, n);
            if hash_below_target(&h, effective_target) {
                found = Some((n, h));
                break;
            }
        }
        let secs = start.elapsed().as_secs_f64().max(0.001);
        if let Some((nonce, h)) = found {
            let sol = SolutionSubmit {
                worker_id: worker_id.clone(),
                device_id: logical_id,
                epoch: work.epoch,
                challenge,
                nonce,
                hash: h,
                attempts,
                hashrate: attempts as f64 / secs,
                found_at_block: work.block_number,
            };
            let _ = post_solution(&client, &cfg, &sol).await;
        }
    }
}
