use std::{env, fs};

use alloy::primitives::{Address, U256};
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTRACT: &str = "0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Mode {
    Standalone,
    Coordinator,
    Worker,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GasMode {
    Cheap,
    Balanced,
    Turbo,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub mode: Mode,
    pub private_key: Option<String>,
    pub miner_address: Option<Address>,
    pub rpc_urls: Vec<String>,
    pub rpc_submit_urls: Vec<String>,
    pub chain_id: u64,
    pub contract: Address,
    pub gas_mode: GasMode,
    pub priority_gwei: f64,
    pub max_fee_gwei_cap: f64,
    pub gas_limit_override: Option<u64>,
    pub gas_limit_cap: u64,
    pub fee_bump_percent: u64,
    pub pending_replace_after_blocks: u64,
    pub target_safety_divisor: u64,
    pub state_check_interval_ms: u64,
    pub print_interval_sec: u64,
    pub gpu: bool,
    pub gpu_indices: Vec<usize>,
    pub gpu_batch: u64,
    pub auto_tune_batch: bool,
    pub local_size: usize,
    pub miner_id: String,
    pub coordinator_url: String,
    pub worker_auth_token: Option<String>,
    pub coordinator_bind: String,
    pub sqlite_path: String,
    pub rpc_health_interval_sec: u64,
    pub rpc_timeout_sec: u64,
    pub rpc_max_block_lag: u64,
    pub metrics_bind: String,
    pub enable_parallel_pending_tx: bool,
    pub auto_tune_target_ms: u64,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let mode = match env_s("MODE", "standalone").to_lowercase().as_str() {
            "standalone" | "mine" => Mode::Standalone,
            "coordinator" | "submitter" => Mode::Coordinator,
            "worker" => Mode::Worker,
            other => return Err(eyre!("invalid MODE={other}")),
        };
        let private_key = read_private_key()?;
        let miner_address = env::var("MINER_ADDRESS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| parse_address(&s))
            .transpose()?;
        let rpc_urls = parse_urls(&env_s(
            "RPC_URLS",
            &env_s("RPC_URL", "https://eth.llamarpc.com"),
        ));
        let rpc_submit_urls = parse_urls(&env_s("RPC_SUBMIT_URLS", &rpc_urls.join(",")));
        let gas_mode = parse_gas_mode(&env_s("GAS_MODE", "balanced"))?;
        let (mode_priority, mode_cap) = gas_mode.defaults();
        Ok(Self {
            mode,
            private_key,
            miner_address,
            rpc_urls,
            rpc_submit_urls,
            chain_id: env_parse("CHAIN_ID", 1u64),
            contract: parse_address(&env_s("CONTRACT", DEFAULT_CONTRACT))?,
            gas_mode,
            priority_gwei: env_parse("PRIORITY_GWEI", mode_priority),
            max_fee_gwei_cap: env_parse("MAX_FEE_GWEI_CAP", mode_cap),
            gas_limit_override: env::var("GAS_LIMIT_OVERRIDE")
                .ok()
                .and_then(|s| s.parse().ok()),
            gas_limit_cap: env_parse("GAS_LIMIT_CAP", 300_000u64),
            fee_bump_percent: env_parse("FEE_BUMP_PERCENT", 15u64),
            pending_replace_after_blocks: env_parse("PENDING_REPLACE_AFTER_BLOCKS", 2u64),
            target_safety_divisor: env_parse("TARGET_SAFETY_DIVISOR", 4u64).max(1),
            state_check_interval_ms: env_parse("STATE_CHECK_INTERVAL_MS", 500u64),
            print_interval_sec: env_parse("PRINT_INTERVAL_SEC", 5u64),
            gpu: env_bool("GPU", true),
            gpu_indices: parse_gpu_indices(),
            gpu_batch: env_parse("GPU_BATCH", 134_217_728u64),
            auto_tune_batch: env_bool("AUTO_TUNE_BATCH", true),
            local_size: env_parse("LOCAL_SIZE", 256usize),
            miner_id: env_s("MINER_ID", &default_miner_id()),
            coordinator_url: env_s("COORDINATOR_URL", "http://127.0.0.1:8787"),
            worker_auth_token: env::var("WORKER_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            coordinator_bind: env_s("COORDINATOR_BIND", "127.0.0.1:8787"),
            sqlite_path: env_s("SQLITE_PATH", "hash256.sqlite"),
            rpc_health_interval_sec: env_parse("RPC_HEALTH_INTERVAL_SEC", 10u64),
            rpc_timeout_sec: env_parse("RPC_TIMEOUT_SEC", 8u64),
            rpc_max_block_lag: env_parse("RPC_MAX_BLOCK_LAG", 2u64),
            metrics_bind: env_s("METRICS_BIND", "0.0.0.0:9898"),
            enable_parallel_pending_tx: env_bool("ENABLE_PARALLEL_PENDING_TX", false),
            auto_tune_target_ms: env_parse("AUTO_TUNE_TARGET_MS", 250u64),
        })
    }

    pub fn miner_address_required(&self) -> Result<Address> {
        self.miner_address
            .ok_or_else(|| eyre!("MINER_ADDRESS is required in worker mode"))
    }

    pub fn effective_target(&self, target: U256) -> U256 {
        target / U256::from(self.target_safety_divisor)
    }
}

impl GasMode {
    pub fn defaults(&self) -> (f64, f64) {
        match self {
            GasMode::Cheap => (0.02, 0.5),
            GasMode::Balanced => (0.2, 1.0),
            GasMode::Turbo => (2.5, 5.0),
        }
    }
}

fn read_private_key() -> Result<Option<String>> {
    if let Ok(path) = env::var("PRIVATE_KEY_FILE") {
        if !path.trim().is_empty() {
            let key = fs::read_to_string(&path)
                .map_err(|e| eyre!("PRIVATE_KEY_FILE {path}: {e}"))?
                .trim()
                .to_string();
            if !key.is_empty() {
                return Ok(Some(key));
            }
        }
    }
    Ok(env::var("PRIVATE_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty()))
}

pub fn parse_urls(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_gpu_indices() -> Vec<usize> {
    if let Ok(raw) = env::var("GPU_INDICES") {
        let v: Vec<usize> = raw
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    vec![env_parse("GPU_INDEX", 0usize)]
}

fn env_s(k: &str, default: &str) -> String {
    env::var(k).unwrap_or_else(|_| default.to_string())
}

fn env_bool(k: &str, default: bool) -> bool {
    env::var(k)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_parse<T: std::str::FromStr>(k: &str, default: T) -> T {
    env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_gas_mode(raw: &str) -> Result<GasMode> {
    Ok(match raw.to_lowercase().as_str() {
        "cheap" => GasMode::Cheap,
        "balanced" => GasMode::Balanced,
        "turbo" => GasMode::Turbo,
        other => return Err(eyre!("invalid GAS_MODE={other}")),
    })
}

pub fn parse_address(raw: &str) -> Result<Address> {
    raw.parse::<Address>()
        .map_err(|e| eyre!("invalid address {raw}: {e}"))
}

fn default_miner_id() -> String {
    let host = env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string());
    format!("{host}-{}", rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_separators() {
        assert_eq!(parse_urls("a,b; c\td"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn gas_mode_defaults() {
        assert_eq!(GasMode::Cheap.defaults(), (0.02, 0.5));
        assert_eq!(GasMode::Balanced.defaults(), (0.2, 1.0));
        assert_eq!(GasMode::Turbo.defaults(), (2.5, 5.0));
    }
}
