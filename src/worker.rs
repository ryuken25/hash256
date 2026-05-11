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

    let reg: RegisterResponse = retry_register(&client, &cfg, &host).await?;
    println!("✅ Registered as worker '{}' (namespace {}...)", reg.assigned_worker_id, &reg.nonce_prefix_namespace[..18]);
    println!("   Coordinator : {}", cfg.coordinator_url);
    println!("   Miner addr  : {}", cfg.miner_address.map(|a| format!("{a:#x}")).unwrap_or_else(|| "<none>".into()));

    let worker_id = Arc::new(tokio::sync::RwLock::new(reg.assigned_worker_id));
    let device_indices = if cfg.gpu_indices.is_empty() {
        vec![0usize]
    } else {
        cfg.gpu_indices.clone()
    };
    // Total attempts counter shared across all device threads on this worker.
    let global_attempts = Arc::new(AtomicU64::new(0));
    let global_started = Instant::now();

    // Heartbeat task: posts current hashrate to coordinator every 5s so the
    // central dashboard can show live aggregate hashrate even between solutions.
    {
        let cfg = cfg.clone();
        let client = client.clone();
        let worker_id = worker_id.clone();
        let attempts = global_attempts.clone();
        tokio::spawn(async move {
            let url = format!("{}/heartbeat", cfg.coordinator_url.trim_end_matches('/'));
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let n = attempts.load(Ordering::Relaxed);
                let secs = global_started.elapsed().as_secs_f64().max(0.001);
                let hps = n as f64 / secs;
                let wid = worker_id.read().await.clone();
                let body = serde_json::json!({
                    "worker_id": wid,
                    "hashrate": hps,
                    "attempts": n,
                });
                let _ = authed(client.post(&url), &cfg).json(&body).send().await;
            }
        });
    }

    let mut handles = Vec::new();
    for (i, dev_idx) in device_indices.iter().enumerate() {
        let cfg = cfg.clone();
        let client = client.clone();
        let worker_id = worker_id.clone();
        let host = host.clone();
        let dev_idx = *dev_idx;
        let logical_id = i as u32 + 1;
        let multi = device_indices.len() > 1;
        let global_attempts = global_attempts.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = run_device_loop(cfg, client, worker_id, host, dev_idx, logical_id, multi, global_attempts).await {
                eprintln!("❌ device {dev_idx} loop exited: {e}");
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

fn is_404_or_unknown_worker(e: &eyre::Report) -> bool {
    let m = e.to_string().to_lowercase();
    m.contains("404") || m.contains("unknown worker")
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
                Err(e) => println!("⚠️  register decode failed: {e}"),
            },
            Err(e) => println!("⚠️  register failed (retry in {backoff}s): {e}"),
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

fn fmt_hashrate(hps: f64) -> String {
    if hps >= 1e9 {
        format!("{:.2} GH/s", hps / 1e9)
    } else if hps >= 1e6 {
        format!("{:.2} MH/s", hps / 1e6)
    } else if hps >= 1e3 {
        format!("{:.2} kH/s", hps / 1e3)
    } else {
        format!("{hps:.2} H/s")
    }
}

#[cfg(feature = "gpu")]
async fn run_device_loop(
    cfg: AppConfig,
    client: Client,
    worker_id: Arc<tokio::sync::RwLock<String>>,
    host: String,
    dev_idx: usize,
    logical_id: u32,
    multi_gpu: bool,
    global_attempts: Arc<AtomicU64>,
) -> Result<()> {
    use crate::gpu::{GpuMiner, MineOutcome};

    let prefix = if multi_gpu {
        format!("[gpu{}] ", dev_idx)
    } else {
        String::new()
    };

    let miner_arc = std::sync::Arc::new(std::sync::Mutex::new(GpuMiner::new_for_index(
        dev_idx,
        Some(cfg.gpu_batch as usize),
        cfg.auto_tune_batch,
        cfg.auto_tune_target_ms,
        cfg.local_size,
    )?));
    let device_name = {
        let m = miner_arc.lock().unwrap();
        let name = m.device_name().to_string();
        let batch = m.batch_size();
        m.self_test().map_err(|e| eyre!("gpu self-test: {e}"))?;
        println!("{prefix}🎮 GPU ready: {name}  batch={batch}");
        name
    };

    let stop_flag = Arc::new(AtomicBool::new(false));
    let watch_token = Arc::new(AtomicU64::new(0));

    // Background poller: bumps watch_token when challenge/epoch changes upstream.
    // Uses shared worker_id (so it sees re-registrations from main loop).
    // Suppresses 404 chatter (main loop is responsible for re-registering).
    let poll_token = watch_token.clone();
    let poll_client = client.clone();
    let poll_cfg = cfg.clone();
    let poll_worker_id = worker_id.clone();
    let last_challenge_arc = Arc::new(std::sync::Mutex::new(String::new()));
    let last_epoch_arc = Arc::new(std::sync::Mutex::new(0u64));
    let lc1 = last_challenge_arc.clone();
    let le1 = last_epoch_arc.clone();
    let prefix_p = prefix.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(poll_cfg.state_check_interval_ms.max(200))).await;
            let wid = poll_worker_id.read().await.clone();
            match fetch_work(&poll_client, &poll_cfg, &wid).await {
                Ok(w) => {
                    let mut lc = lc1.lock().unwrap();
                    let mut le = le1.lock().unwrap();
                    if *lc != w.challenge || *le != w.epoch {
                        *lc = w.challenge.clone();
                        *le = w.epoch;
                        poll_token.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    if is_404_or_unknown_worker(&e) {
                        // Bump token so the mining loop aborts and main loop
                        // can hit fetch_work, detect 404, and re-register.
                        poll_token.fetch_add(1, Ordering::Relaxed);
                    } else {
                        println!("{prefix_p}⚠️  block poll failed: {e}");
                    }
                }
            }
        }
    });

    let mut last_seen_epoch: Option<u64> = None;
    loop {
        let wid = worker_id.read().await.clone();
        let work = match fetch_work(&client, &cfg, &wid).await {
            Ok(w) => w,
            Err(e) if is_404_or_unknown_worker(&e) => {
                println!("{prefix}⚠️  worker_id '{wid}' unknown to coordinator (likely restarted) — re-registering");
                match retry_register(&client, &cfg, &host).await {
                    Ok(new_reg) => {
                        let new_id = new_reg.assigned_worker_id.clone();
                        println!("{prefix}✅ Re-registered as '{new_id}' (namespace {}...)", &new_reg.nonce_prefix_namespace[..18.min(new_reg.nonce_prefix_namespace.len())]);
                        *worker_id.write().await = new_id;
                    }
                    Err(e) => {
                        println!("{prefix}❌ re-register failed: {e}");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
                continue;
            }
            Err(e) => {
                println!("{prefix}⚠️  fetch work failed: {e}");
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
                println!("{prefix}⚠️  bad challenge {}: {e}", work.challenge);
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let effective_target = match U256::from_str_radix(&work.effective_target, 10) {
            Ok(v) => v,
            Err(e) => {
                println!("{prefix}⚠️  bad effective_target: {e}");
                continue;
            }
        };
        let prefix_bytes = match parse_prefix_hex(&work.nonce_prefix_high_192) {
            Ok(p) => p,
            Err(e) => {
                println!("{prefix}⚠️  bad prefix: {e}");
                continue;
            }
        };

        // Round-start banner
        if let Some(prev) = last_seen_epoch {
            if prev != work.epoch {
                println!(
                    "{prefix}🔄 Epoch changed {} -> {} (block {}), restarting round",
                    prev, work.epoch, work.block_number
                );
            }
        }
        last_seen_epoch = Some(work.epoch);
        println!();
        println!("{prefix}📊 Round start:");
        println!("{prefix}   Block      : {}", work.block_number);
        println!("{prefix}   Epoch      : {}", work.epoch);
        println!("{prefix}   Difficulty : {}", work.target);
        println!("{prefix}   Challenge  : {}...", &work.challenge[..18.min(work.challenge.len())]);
        println!("{prefix}⛏️  Mining epoch {} on GPU ({})", work.epoch, device_name);

        let counter_start = work.base_counter64;
        let attempts_local = Arc::new(AtomicU64::new(0));
        let start = Instant::now();

        // Periodic hashrate printer for this round.
        let printer_stop = Arc::new(AtomicBool::new(false));
        {
            let attempts = attempts_local.clone();
            let stop = printer_stop.clone();
            let interval = Duration::from_secs(cfg.print_interval_sec.max(1));
            let prefix_p = prefix.clone();
            let started = start;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    let n = attempts.load(Ordering::Relaxed);
                    let hps = n as f64 / secs;
                    println!(
                        "{prefix_p}⚡ {} | round {:>5}s | attempts {:>15}",
                        fmt_hashrate(hps),
                        secs as u64,
                        n
                    );
                }
            });
        }

        let miner = miner_arc.clone();
        let stop = stop_flag.clone();
        let token_for_mine = watch_token.clone();
        let attempts_for_thread = attempts_local.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut m = miner.lock().unwrap();
            m.mine_until(
                challenge,
                effective_target,
                prefix_bytes,
                counter_start,
                stop,
                attempts_for_thread,
                Some((token_for_mine, token_snap)),
            )
        })
        .await
        .map_err(|e| eyre!("join: {e}"))??;

        printer_stop.store(true, Ordering::Relaxed);

        let secs = start.elapsed().as_secs_f64().max(0.001);
        let attempts = attempts_local.load(Ordering::Relaxed);
        // Feed global counter so heartbeat task can compute aggregate hashrate.
        global_attempts.fetch_add(attempts, Ordering::Relaxed);
        let hashrate = attempts as f64 / secs;

        match outcome {
            MineOutcome::Found { nonce, .. } => {
                let h = hash_nonce(&challenge, nonce);
                if !hash_below_target(&h, effective_target) {
                    println!("{prefix}⚠️  GPU result failed CPU verify; skipping submission");
                    continue;
                }
                println!(
                    "{prefix}🎯 FOUND solution! nonce=0x{:x}",
                    nonce
                );
                println!(
                    "{prefix}   round={}s  attempts={}  hashrate={}",
                    secs as u64,
                    attempts,
                    fmt_hashrate(hashrate)
                );
                let sol = SolutionSubmit {
                    worker_id: worker_id.read().await.clone(),
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
                    Ok(resp) => {
                        let accepted = resp.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false);
                        let reason = resp.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                        let tx_hash = resp.get("tx_hash").and_then(|v| v.as_str()).unwrap_or("");
                        if accepted {
                            println!("{prefix}📤 Submitted ACCEPTED: tx={tx_hash}  reason={reason}");
                        } else {
                            println!("{prefix}❌ Submitted REJECTED: {reason}");
                        }
                    }
                    Err(e) => println!("{prefix}❌ solution post failed: {e}"),
                }
            }
            MineOutcome::Aborted { .. } => {
                println!(
                    "{prefix}🔁 Round aborted at {}s (state changed) — attempts={} avg={}",
                    secs as u64,
                    attempts,
                    fmt_hashrate(hashrate)
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
    worker_id: Arc<tokio::sync::RwLock<String>>,
    host: String,
    dev_idx: usize,
    logical_id: u32,
    _multi_gpu: bool,
    _global_attempts: Arc<AtomicU64>,
) -> Result<()> {
    println!(
        "⚠️  binary built without `gpu` feature; using CPU fallback (very slow), device_index={dev_idx}"
    );
    loop {
        let wid = worker_id.read().await.clone();
        let work = match fetch_work(&client, &cfg, &wid).await {
            Ok(w) => w,
            Err(e) if is_404_or_unknown_worker(&e) => {
                println!("⚠️  worker_id stale, re-registering");
                if let Ok(new_reg) = retry_register(&client, &cfg, &host).await {
                    *worker_id.write().await = new_reg.assigned_worker_id;
                }
                continue;
            }
            Err(e) => {
                println!("⚠️  fetch_work: {e}");
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
                worker_id: worker_id.read().await.clone(),
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
