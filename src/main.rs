mod cli;
mod config;
mod contract;
mod coordinator;
mod db;
mod errors;
mod gas;
mod metrics;
mod nonce_space;
mod rpc_pool;
mod state_watcher;
mod submitter;
mod tx_nonce_manager;
mod worker;

#[cfg(feature = "gpu")]
mod gpu;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use clap::Parser;
use eyre::{eyre, Result};

use crate::cli::{Cli, Command};
use crate::config::{AppConfig, Mode};
use crate::contract::{hash_nonce, hex_b256, proof_is_valid};
use crate::coordinator::CoordinatorApp;
use crate::db::Db;
use crate::metrics::Metrics;
use crate::rpc_pool::RpcPool;
use crate::state_watcher::StateWatcher;
use crate::submitter::{SolutionSubmit, Submitter};
use crate::tx_nonce_manager::TxNonceManager;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = AppConfig::from_env()?;
    init_tracing();
    let cli = Cli::parse();
    match cli.command.unwrap_or(match cfg.mode {
        Mode::Standalone => Command::Mine,
        Mode::Coordinator => Command::Coordinator,
        Mode::Worker => Command::Worker,
    }) {
        Command::Devices => devices().await,
        Command::State => state(cfg).await,
        Command::Bench => bench(cfg).await,
        Command::Verify { nonce } => verify(cfg, &nonce).await,
        Command::Mine => standalone_mine(cfg).await,
        Command::Coordinator => run_coordinator(cfg).await,
        Command::Worker => worker::run_worker(cfg).await,
        Command::Txs => txs(cfg).await,
        Command::Doctor => doctor(cfg).await,
    }
}

fn init_tracing() {
    let json = std::env::var("JSON_LOGS").ok().as_deref() == Some("true");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn make_pool(cfg: &AppConfig) -> Result<RpcPool> {
    let pool = RpcPool::new(
        cfg.rpc_urls.clone(),
        cfg.rpc_submit_urls.clone(),
        cfg.rpc_timeout_sec,
        cfg.chain_id,
        cfg.rpc_max_block_lag,
    )?;
    pool.refresh_health().await;
    pool.spawn_health_loop(cfg.rpc_health_interval_sec);
    Ok(pool)
}

fn signer_from_cfg(cfg: &AppConfig) -> Result<PrivateKeySigner> {
    let raw = cfg
        .private_key
        .clone()
        .ok_or_else(|| eyre!("PRIVATE_KEY or PRIVATE_KEY_FILE required"))?;
    let key = raw.trim().trim_start_matches("0x");
    if key.len() != 64 {
        return Err(eyre!("invalid private key length"));
    }
    Ok(key.parse()?)
}

async fn devices() -> Result<()> {
    #[cfg(feature = "gpu")]
    {
        gpu::list_devices()?;
    }
    #[cfg(not(feature = "gpu"))]
    {
        println!("binary built without gpu feature");
    }
    Ok(())
}

async fn state(cfg: AppConfig) -> Result<()> {
    let pool = make_pool(&cfg).await?;
    let miner = if let Some(addr) = cfg.miner_address {
        addr
    } else {
        signer_from_cfg(&cfg)?.address()
    };
    let block = pool.block_number().await?;
    let st = pool.mining_state(cfg.contract).await?;
    let challenge = pool.get_challenge(cfg.contract, miner).await?;
    let eff = cfg.effective_target(st.difficulty);
    let bal = pool.balance(miner).await.unwrap_or(U256::ZERO);
    println!("miner_address={miner:#x}");
    println!("balance_wei={bal}");
    println!("block={block}");
    println!("epoch={}", st.epoch);
    println!("blocks_left={}", st.epoch_blocks_left);
    println!("challenge={}", hex_b256(&challenge));
    println!("target={}", st.difficulty);
    println!("effective_target={eff}");
    Ok(())
}

async fn bench(cfg: AppConfig) -> Result<()> {
    let challenge = B256::from([7u8; 32]);
    let target = U256::MAX / U256::from(1024u64);
    let attempts = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    #[cfg(feature = "gpu")]
    if cfg.gpu {
        let mut g = gpu::GpuMiner::new_for_index(
            cfg.gpu_indices.first().copied().unwrap_or(0),
            Some(cfg.gpu_batch as usize),
            cfg.auto_tune_batch,
            cfg.auto_tune_target_ms,
            cfg.local_size,
        )?;
        let _stop_thread = std::thread::spawn({
            let stop = stop.clone();
            move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                stop.store(true, Ordering::Relaxed);
            }
        });
        let _ = g.mine(challenge, target, 0, stop, attempts.clone())?;
        let rate = attempts.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64();
        println!(
            "device={} attempts={} hashrate={:.2} H/s",
            g.device_name(),
            attempts.load(Ordering::Relaxed),
            rate
        );
        return Ok(());
    }
    cpu_bench(challenge, target, attempts, start);
    Ok(())
}

fn cpu_bench(challenge: B256, target: U256, attempts: Arc<AtomicU64>, start: Instant) {
    let until = std::time::Duration::from_secs(5);
    let mut nonce = U256::ZERO;
    while start.elapsed() < until {
        let _ = proof_is_valid(&challenge, nonce, target);
        nonce += U256::from(1u64);
        attempts.fetch_add(1, Ordering::Relaxed);
    }
    let rate = attempts.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64();
    println!(
        "cpu attempts={} hashrate={rate:.2} H/s",
        attempts.load(Ordering::Relaxed)
    );
}

async fn verify(cfg: AppConfig, nonce: &str) -> Result<()> {
    let pool = make_pool(&cfg).await?;
    let miner = cfg
        .miner_address
        .or_else(|| signer_from_cfg(&cfg).ok().map(|s| s.address()))
        .ok_or_else(|| eyre!("set MINER_ADDRESS or PRIVATE_KEY"))?;
    let st = pool.mining_state(cfg.contract).await?;
    let challenge = pool.get_challenge(cfg.contract, miner).await?;
    let pow_nonce = parse_u256(nonce)?;
    let hash = hash_nonce(&challenge, pow_nonce);
    println!("hash={}", hex_b256(&hash));
    println!(
        "valid_target={}",
        proof_is_valid(&challenge, pow_nonce, st.difficulty)
    );
    println!(
        "valid_effective_target={}",
        proof_is_valid(&challenge, pow_nonce, cfg.effective_target(st.difficulty))
    );
    Ok(())
}

async fn standalone_mine(cfg: AppConfig) -> Result<()> {
    let signer = signer_from_cfg(&cfg)?;
    let miner = signer.address();
    println!("standalone miner={miner:#x} contract={:#x}", cfg.contract);
    let pool = make_pool(&cfg).await?;
    let db = Db::open(&cfg.sqlite_path)?;
    let tx_mgr = TxNonceManager::new(miner, &pool, db).await?;
    let metrics = Arc::new(Metrics::default());
    let watcher = StateWatcher::new(cfg.clone(), pool.clone(), miner);
    let submitter = Arc::new(Submitter::new(
        cfg.clone(),
        pool.clone(),
        Some(tx_mgr),
        Some(signer),
        miner,
        metrics.clone(),
    ));

    // Spawn watcher.
    {
        let w = watcher.clone();
        tokio::spawn(async move { w.run().await });
    }
    // Background tasks.
    {
        let s = submitter.clone();
        tokio::spawn(async move { s.receipt_watcher_loop().await });
        let s = submitter.clone();
        tokio::spawn(async move { s.replacement_loop().await });
    }
    // Metrics server.
    {
        let m = metrics.clone();
        let bind = cfg.metrics_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::metrics::serve_metrics(bind, m).await {
                tracing::warn!("metrics server: {e}");
            }
        });
    }

    // Wait for first state.
    {
        let mut rx = watcher.subscribe();
        tracing::info!("waiting for initial chain state...");
        loop {
            if rx.borrow().is_some() {
                break;
            }
            if rx.changed().await.is_err() {
                return Err(eyre!("state watcher closed"));
            }
        }
    }

    let device_indices = if cfg.gpu_indices.is_empty() {
        vec![0usize]
    } else {
        cfg.gpu_indices.clone()
    };

    #[cfg(feature = "gpu")]
    if cfg.gpu {
        let (sol_tx, mut sol_rx) =
            tokio::sync::mpsc::unbounded_channel::<SolutionSubmit>();
        // Submitter consumer task.
        {
            let submitter = submitter.clone();
            let watcher = watcher.clone();
            tokio::spawn(async move {
                while let Some(sol) = sol_rx.recv().await {
                    let Some(state) = watcher.current() else {
                        tracing::warn!("no state when consuming solution");
                        continue;
                    };
                    match submitter.submit_solution(sol, state).await {
                        Ok(r) => tracing::info!("submit result: {r:?}"),
                        Err(e) => tracing::warn!("submit_solution failed: {e}"),
                    }
                }
            });
        }
        // Spawn one OS thread per device.
        let token = watcher.token();
        let mut handles = Vec::new();
        for (i, dev_idx) in device_indices.iter().enumerate() {
            let cfg = cfg.clone();
            let watcher = watcher.clone();
            let metrics = metrics.clone();
            let token = token.clone();
            let sol_tx = sol_tx.clone();
            let dev_idx = *dev_idx;
            let logical_id = (i as u32) + 1;
            handles.push(std::thread::spawn(move || {
                if let Err(e) = standalone_device_loop(
                    cfg, watcher, metrics, token, sol_tx, dev_idx, logical_id,
                ) {
                    tracing::error!(device_index = dev_idx, "device loop exited: {e}");
                }
            }));
        }
        // Print interval summary.
        {
            let metrics = metrics.clone();
            let interval = cfg.print_interval_sec.max(1);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    let total = metrics.attempts.load(Ordering::Relaxed);
                    tracing::info!(total_attempts = total, "miner heartbeat");
                }
            });
        }
        for h in handles {
            let _ = h.join();
        }
        return Ok(());
    }

    // Pure CPU fallback path (no GPU feature or GPU=0).
    tracing::warn!("running CPU mining fallback (very slow)");
    cpu_standalone_loop(cfg, watcher, submitter).await
}

#[cfg(feature = "gpu")]
fn standalone_device_loop(
    cfg: AppConfig,
    watcher: StateWatcher,
    metrics: Arc<Metrics>,
    token: Arc<AtomicU64>,
    sol_tx: tokio::sync::mpsc::UnboundedSender<SolutionSubmit>,
    dev_idx: usize,
    logical_id: u32,
) -> Result<()> {
    use crate::gpu::{GpuMiner, MineOutcome};
    use crate::nonce_space::NonceAllocator;

    let mut miner = GpuMiner::new_for_index(
        dev_idx,
        Some(cfg.gpu_batch as usize),
        cfg.auto_tune_batch,
        cfg.auto_tune_target_ms,
        cfg.local_size,
    )?;
    tracing::info!(
        device_index = dev_idx,
        device_name = miner.device_name(),
        batch = miner.batch_size(),
        "gpu device ready"
    );
    miner.self_test()?;

    let stop = Arc::new(AtomicBool::new(false));
    let mut allocator: Option<NonceAllocator> = None;
    let mut snapshot_token = u64::MAX;
    loop {
        let state = match watcher.current() {
            Some(s) => s,
            None => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let cur_tok = token.load(Ordering::Relaxed);
        if snapshot_token != cur_tok || allocator.is_none() {
            allocator = Some(NonceAllocator::new(
                &cfg.miner_id,
                logical_id,
                state.epoch,
                &state.challenge,
            ));
            snapshot_token = cur_tok;
            tracing::info!(
                device_index = dev_idx,
                epoch = state.epoch,
                "rebound nonce allocator for new epoch/challenge"
            );
        }
        let alloc = allocator.as_mut().unwrap();
        let batch = alloc.next_batch(cfg.gpu_batch);
        let attempts_local = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let outcome = miner.mine_until(
            state.challenge,
            state.effective_target,
            batch.prefix,
            batch.base_counter64,
            stop.clone(),
            attempts_local.clone(),
            Some((token.clone(), cur_tok)),
        )?;
        let secs = start.elapsed().as_secs_f64().max(0.001);
        let attempts = attempts_local.load(Ordering::Relaxed);
        metrics.attempts.fetch_add(attempts, Ordering::Relaxed);
        let hashrate = attempts as f64 / secs;
        let device_name = miner.device_name().to_string();
        metrics.record_device(dev_idx, &device_name, attempts, hashrate);
        match outcome {
            MineOutcome::Found { nonce, .. } => {
                let h = hash_nonce(&state.challenge, nonce);
                let sol = SolutionSubmit {
                    worker_id: cfg.miner_id.clone(),
                    device_id: logical_id,
                    epoch: state.epoch,
                    challenge: state.challenge,
                    nonce,
                    hash: h,
                    attempts,
                    hashrate,
                    found_at_block: state.block_number,
                };
                tracing::info!(
                    device_index = dev_idx,
                    nonce = %nonce,
                    hashrate = format!("{hashrate:.2}"),
                    "found solution"
                );
                if sol_tx.send(sol).is_err() {
                    tracing::warn!("submitter channel closed; exiting device loop");
                    return Ok(());
                }
            }
            MineOutcome::Aborted { .. } => {
                tracing::debug!(device_index = dev_idx, "mining aborted (state changed)");
            }
        }
    }
}

async fn cpu_standalone_loop(
    cfg: AppConfig,
    watcher: StateWatcher,
    submitter: Arc<Submitter>,
) -> Result<()> {
    use crate::contract::hash_below_target;
    use crate::nonce_space::NonceAllocator;
    let mut allocator: Option<NonceAllocator> = None;
    let mut last_token = u64::MAX;
    let token = watcher.token();
    loop {
        let state = match watcher.current() {
            Some(s) => s,
            None => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let cur_tok = token.load(Ordering::Relaxed);
        if last_token != cur_tok || allocator.is_none() {
            allocator = Some(NonceAllocator::new(
                &cfg.miner_id,
                1,
                state.epoch,
                &state.challenge,
            ));
            last_token = cur_tok;
        }
        let alloc = allocator.as_mut().unwrap();
        let batch = alloc.next_batch(cfg.gpu_batch.min(2_000_000));
        let mut found = None;
        for i in 0..batch.batch_size {
            if token.load(Ordering::Relaxed) != cur_tok {
                break;
            }
            let n = batch.pow_nonce(i);
            let h = hash_nonce(&state.challenge, n);
            if hash_below_target(&h, state.effective_target) {
                found = Some((n, h));
                break;
            }
        }
        if let Some((nonce, hash)) = found {
            let sol = SolutionSubmit {
                worker_id: cfg.miner_id.clone(),
                device_id: 0,
                epoch: state.epoch,
                challenge: state.challenge,
                nonce,
                hash,
                attempts: batch.batch_size,
                hashrate: 0.0,
                found_at_block: state.block_number,
            };
            if let Err(e) = submitter.submit_solution(sol, state).await {
                tracing::warn!("submit_solution failed: {e}");
            }
        }
    }
}

async fn run_coordinator(cfg: AppConfig) -> Result<()> {
    let signer = signer_from_cfg(&cfg)?;
    let miner = signer.address();
    let pool = make_pool(&cfg).await?;
    let db = Db::open(&cfg.sqlite_path)?;
    let tx_mgr = TxNonceManager::new(miner, &pool, db).await?;
    let metrics = Arc::new(Metrics::default());
    let watcher = StateWatcher::new(cfg.clone(), pool.clone(), miner);
    let submitter = Submitter::new(
        cfg.clone(),
        pool.clone(),
        Some(tx_mgr),
        Some(signer),
        miner,
        metrics.clone(),
    );
    println!(
        "coordinator wallet={miner:#x} contract={:#x} bind={}",
        cfg.contract, cfg.coordinator_bind
    );
    CoordinatorApp::new(cfg, pool, watcher, submitter, metrics)
        .run()
        .await
}

async fn txs(cfg: AppConfig) -> Result<()> {
    let db = Db::open(&cfg.sqlite_path)?;
    for tx in db.list_txs()? {
        println!(
            "nonce={} status={} hash={} pow_nonce={} epoch={} max_fee={} tip={} gas_limit={}",
            tx.tx_nonce,
            tx.status,
            tx.raw_tx_hash,
            tx.pow_nonce,
            tx.epoch,
            tx.max_fee_per_gas,
            tx.max_priority_fee_per_gas,
            tx.gas_limit
        );
    }
    Ok(())
}

async fn doctor(cfg: AppConfig) -> Result<()> {
    let pool = make_pool(&cfg).await?;
    println!("chain_id={}", pool.chain_id().await.unwrap_or(0));
    println!("read_rpc_health={:#?}", pool.read_health().await);
    println!("submit_rpc_health={:#?}", pool.submit_health().await);
    if let Ok(signer) = signer_from_cfg(&cfg) {
        let bal = pool.balance(signer.address()).await.unwrap_or(U256::ZERO);
        println!("wallet={:#x} balance_wei={bal}", signer.address());
    } else if let Some(addr) = cfg.miner_address {
        println!("miner_address={addr:#x} private_key_present=false");
    }
    println!(
        "gas_mode={:?} priority_gwei={} max_fee_cap_gwei={} gas_limit_cap={}",
        cfg.gas_mode, cfg.priority_gwei, cfg.max_fee_gwei_cap, cfg.gas_limit_cap
    );
    println!(
        "gpu_indices={:?} gpu_batch={} auto_tune={} local_size={}",
        cfg.gpu_indices, cfg.gpu_batch, cfg.auto_tune_batch, cfg.local_size
    );
    devices().await.ok();
    Ok(())
}

fn parse_u256(raw: &str) -> Result<U256> {
    if let Some(hex) = raw.strip_prefix("0x") {
        Ok(U256::from_str_radix(hex, 16)?)
    } else {
        Ok(U256::from_str_radix(raw, 10)?)
    }
}

#[allow(dead_code)]
fn _force_address_used(_a: Address) {}
