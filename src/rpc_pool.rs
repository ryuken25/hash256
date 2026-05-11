use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::primitives::{Address, B256, U256};
use eyre::{eyre, Result};
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::{config::parse_urls, contract::MiningStateView};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcHealth {
    pub url_redacted: String,
    pub healthy: bool,
    pub latency_ms: u128,
    pub block_number: u64,
    pub chain_id: u64,
    pub error_count: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    health: RpcHealth,
}

#[derive(Debug, Clone)]
pub struct RpcPool {
    client: Client,
    read: Arc<RwLock<Vec<Endpoint>>>,
    submit: Arc<RwLock<Vec<Endpoint>>>,
    chain_id: u64,
    max_block_lag: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FeeSnapshot {
    pub base_fee_per_gas: u128,
    pub priority_suggestion: u128,
}

#[derive(Debug, Clone)]
pub struct ReceiptInfo {
    pub block_number: u64,
    pub success: bool,
    pub tx_hash: B256,
}

#[derive(Debug, Clone)]
pub struct BroadcastOutcome {
    pub tx_hash: String,
    pub already_known_count: usize,
    pub accepted_count: usize,
    pub errors: Vec<String>,
}

impl RpcPool {
    pub fn new(
        read_urls: Vec<String>,
        submit_urls: Vec<String>,
        timeout_sec: u64,
        chain_id: u64,
        max_block_lag: u64,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_sec.max(1)))
            .build()?;
        let read = read_urls.into_iter().map(Endpoint::new).collect();
        let submit = submit_urls.into_iter().map(Endpoint::new).collect();
        Ok(Self {
            client,
            read: Arc::new(RwLock::new(read)),
            submit: Arc::new(RwLock::new(submit)),
            chain_id,
            max_block_lag,
        })
    }

    pub fn spawn_health_loop(&self, interval_sec: u64) {
        let pool = self.clone();
        let interval = Duration::from_secs(interval_sec.max(1));
        tokio::spawn(async move {
            loop {
                pool.refresh_health().await;
                tokio::time::sleep(interval).await;
            }
        });
    }

    pub async fn refresh_health(&self) {
        self.refresh_group(&self.read).await;
        self.refresh_group(&self.submit).await;
    }

    async fn refresh_group(&self, group: &Arc<RwLock<Vec<Endpoint>>>) {
        let urls: Vec<String> = group.read().await.iter().map(|e| e.url.clone()).collect();
        let mut updates = Vec::new();
        for url in urls {
            updates.push((url.clone(), self.check_url(&url).await));
        }
        let freshest = updates
            .iter()
            .filter_map(|(_, h)| h.as_ref().ok().map(|h| h.block_number))
            .max()
            .unwrap_or(0);
        let mut guard = group.write().await;
        for ep in guard.iter_mut() {
            match updates.iter().find(|(u, _)| u == &ep.url).map(|(_, r)| r) {
                Some(Ok(h)) => {
                    let mut h = h.clone();
                    if freshest.saturating_sub(h.block_number) > self.max_block_lag {
                        h.healthy = false;
                        h.last_error = Some(format!(
                            "lagging {} blocks",
                            freshest.saturating_sub(h.block_number)
                        ));
                    }
                    ep.health = h;
                }
                Some(Err(e)) => {
                    ep.health.healthy = false;
                    ep.health.error_count += 1;
                    ep.health.last_error = Some(e.to_string());
                }
                None => {}
            }
        }
        guard.sort_by_key(|e| (!e.health.healthy, e.health.latency_ms));
    }

    async fn check_url(&self, url: &str) -> Result<RpcHealth> {
        let start = Instant::now();
        let block_hex: String = self.call_url(url, "eth_blockNumber", json!([])).await?;
        let chain_hex: String = self.call_url(url, "eth_chainId", json!([])).await?;
        let block_number = parse_hex_u64(&block_hex)?;
        let chain_id = parse_hex_u64(&chain_hex)?;
        Ok(RpcHealth {
            url_redacted: redact_url(url),
            healthy: chain_id == self.chain_id,
            latency_ms: start.elapsed().as_millis(),
            block_number,
            chain_id,
            error_count: 0,
            last_error: if chain_id == self.chain_id {
                None
            } else {
                Some(format!("wrong chain_id {chain_id}"))
            },
        })
    }

    pub async fn read_health(&self) -> Vec<RpcHealth> {
        self.read.read().await.iter().map(|e| e.health.clone()).collect()
    }

    pub async fn submit_health(&self) -> Vec<RpcHealth> {
        self.submit.read().await.iter().map(|e| e.health.clone()).collect()
    }

    pub async fn best_read_url(&self) -> Result<String> {
        let guard = self.read.read().await;
        guard
            .iter()
            .find(|e| e.health.healthy)
            .or_else(|| guard.first())
            .map(|e| e.url.clone())
            .ok_or_else(|| eyre!("no RPC URLs configured"))
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let urls: Vec<String> = self
            .read
            .read()
            .await
            .iter()
            .filter(|e| e.health.healthy)
            .map(|e| e.url.clone())
            .collect();
        let urls = if urls.is_empty() {
            self.read.read().await.iter().map(|e| e.url.clone()).collect()
        } else {
            urls
        };
        let mut last = None;
        for url in urls {
            match self.call_url(&url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| eyre!("no RPC URLs configured")))
    }

    pub async fn call_url<T: DeserializeOwned>(
        &self,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let resp: Value = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(eyre!("rpc {method} error: {err}"));
        }
        serde_json::from_value(resp.get("result").cloned().unwrap_or(Value::Null))
            .map_err(Into::into)
    }

    pub async fn broadcast_raw_tx(&self, raw_tx: &str) -> Result<BroadcastOutcome> {
        let urls: Vec<String> = {
            let guard = self.submit.read().await;
            let healthy: Vec<String> = guard
                .iter()
                .filter(|e| e.health.healthy)
                .map(|e| e.url.clone())
                .collect();
            if healthy.is_empty() {
                guard.iter().map(|e| e.url.clone()).collect()
            } else {
                healthy
            }
        };
        let mut accepted: Option<String> = None;
        let mut already_known = 0usize;
        let mut accepted_count = 0usize;
        let mut errors = Vec::new();
        for url in urls {
            match self
                .call_url::<String>(&url, "eth_sendRawTransaction", json!([raw_tx]))
                .await
            {
                Ok(hash) => {
                    accepted_count += 1;
                    if accepted.is_none() {
                        accepted = Some(hash);
                    }
                }
                Err(e) => {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("already known") || msg.contains("known transaction") {
                        already_known += 1;
                        if accepted.is_none() {
                            accepted = Some("already_known".to_string());
                        }
                    } else {
                        errors.push(format!("{}: {e}", redact_url(&url)));
                    }
                }
            };
        }
        match accepted {
            Some(tx_hash) => Ok(BroadcastOutcome {
                tx_hash,
                already_known_count: already_known,
                accepted_count,
                errors,
            }),
            None => Err(eyre!("broadcast failed: {}", errors.join("; "))),
        }
    }

    pub async fn block_number(&self) -> Result<u64> {
        let h: String = self.call("eth_blockNumber", json!([])).await?;
        parse_hex_u64(&h)
    }

    pub async fn chain_id(&self) -> Result<u64> {
        let h: String = self.call("eth_chainId", json!([])).await?;
        parse_hex_u64(&h)
    }

    pub async fn transaction_count(&self, address: Address, tag: &str) -> Result<u64> {
        let h: String = self
            .call(
                "eth_getTransactionCount",
                json!([format!("{address:#x}"), tag]),
            )
            .await?;
        parse_hex_u64(&h)
    }

    pub async fn balance(&self, address: Address) -> Result<U256> {
        let h: String = self
            .call("eth_getBalance", json!([format!("{address:#x}"), "latest"]))
            .await?;
        parse_hex_u256(&h)
    }

    pub async fn mining_state(&self, contract: Address) -> Result<MiningStateView> {
        let data = "0x392e6678";
        let raw: String = self
            .call(
                "eth_call",
                json!([{ "to": format!("{contract:#x}"), "data": data, "gas": "0x30d40" }, "latest"]),
            )
            .await?;
        decode_mining_state(&raw)
    }

    pub async fn get_challenge(&self, contract: Address, miner: Address) -> Result<B256> {
        let selector_hash = alloy::primitives::keccak256("getChallenge(address)".as_bytes());
        let selector = &selector_hash.as_slice()[..4];
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(selector);
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(miner.as_slice());
        let raw: String = self
            .call(
                "eth_call",
                json!([{ "to": format!("{contract:#x}"), "data": format!("0x{}", hex::encode(data)), "gas": "0x30d40" }, "latest"]),
            )
            .await?;
        let bytes = hex::decode(raw.trim_start_matches("0x"))?;
        if bytes.len() < 32 {
            return Err(eyre!("short getChallenge response"));
        }
        Ok(B256::from_slice(&bytes[0..32]))
    }

    pub async fn eth_call_tx(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        gas: u64,
    ) -> Result<String> {
        self.call(
            "eth_call",
            json!([{
                "from": format!("{from:#x}"),
                "to": format!("{to:#x}"),
                "data": format!("0x{}", hex::encode(data)),
                "gas": format!("0x{gas:x}")
            }, "latest"]),
        )
        .await
    }

    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        gas_cap: u64,
    ) -> Result<u64> {
        let h: String = self
            .call(
                "eth_estimateGas",
                json!([{
                    "from": format!("{from:#x}"),
                    "to": format!("{to:#x}"),
                    "data": format!("0x{}", hex::encode(data)),
                    "gas": format!("0x{gas_cap:x}")
                }]),
            )
            .await?;
        parse_hex_u64(&h)
    }

    pub async fn fee_history(&self, blocks: u64) -> Result<FeeSnapshot> {
        let v: Value = self
            .call(
                "eth_feeHistory",
                json!([format!("0x{:x}", blocks), "latest", [50u64]]),
            )
            .await?;
        let base_fee_arr = v
            .get("baseFeePerGas")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let base_fee_per_gas = base_fee_arr
            .last()
            .and_then(|s| s.as_str())
            .map(parse_hex_u128)
            .transpose()?
            .unwrap_or(0);
        let priority = v
            .get("reward")
            .and_then(|v| v.as_array())
            .and_then(|outer| outer.last())
            .and_then(|inner| inner.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(parse_hex_u128)
            .transpose()?
            .unwrap_or(0);
        Ok(FeeSnapshot {
            base_fee_per_gas,
            priority_suggestion: priority,
        })
    }

    pub async fn tx_receipt(&self, hash: &str) -> Result<Option<ReceiptInfo>> {
        let v: Value = self
            .call("eth_getTransactionReceipt", json!([hash]))
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        let block = v
            .get("blockNumber")
            .and_then(|s| s.as_str())
            .map(parse_hex_u64)
            .transpose()?
            .unwrap_or(0);
        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .map(parse_hex_u64)
            .transpose()?
            .unwrap_or(0);
        let tx_hash = v
            .get("transactionHash")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| hash.to_string());
        let bytes = hex::decode(tx_hash.trim_start_matches("0x")).unwrap_or_default();
        let h = if bytes.len() == 32 {
            B256::from_slice(&bytes)
        } else {
            B256::ZERO
        };
        Ok(Some(ReceiptInfo {
            block_number: block,
            success: status == 1,
            tx_hash: h,
        }))
    }
}

impl Endpoint {
    fn new(url: String) -> Self {
        Self {
            health: RpcHealth {
                url_redacted: redact_url(&url),
                healthy: true,
                latency_ms: u128::MAX,
                block_number: 0,
                chain_id: 0,
                error_count: 0,
                last_error: None,
            },
            url,
        }
    }
}

pub fn parse_rpc_urls(raw: &str) -> Vec<String> {
    parse_urls(raw)
}

pub fn parse_hex_u64(raw: &str) -> Result<u64> {
    Ok(u64::from_str_radix(raw.trim_start_matches("0x"), 16)?)
}

pub fn parse_hex_u128(raw: &str) -> Result<u128> {
    Ok(u128::from_str_radix(raw.trim_start_matches("0x"), 16)?)
}

pub fn parse_hex_u256(raw: &str) -> Result<U256> {
    Ok(U256::from_str_radix(raw.trim_start_matches("0x"), 16)?)
}

pub fn redact_url(url: &str) -> String {
    if let Ok(mut u) = url::Url::parse(url) {
        if u.query().is_some() {
            u.set_query(Some("REDACTED"));
        }
        let path = u.path().to_string();
        if path.len() > 12 {
            u.set_path("/REDACTED");
        }
        u.to_string()
    } else {
        "<invalid-url>".to_string()
    }
}

fn decode_mining_state(raw: &str) -> Result<MiningStateView> {
    let bytes = hex::decode(raw.trim_start_matches("0x"))?;
    if bytes.len() < 224 {
        return Err(eyre!("short miningState response"));
    }
    let word = |i: usize| U256::from_be_slice(&bytes[i * 32..(i + 1) * 32]);
    Ok(MiningStateView {
        era: word(0),
        reward: word(1),
        difficulty: word(2),
        minted: word(3),
        remaining: word(4),
        epoch: word(5),
        epoch_blocks_left: word(6),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing_accepts_comma_semicolon_space() {
        assert_eq!(
            parse_rpc_urls("https://a,https://b; https://c"),
            vec!["https://a", "https://b", "https://c"]
        );
    }

    #[test]
    fn redacts_query() {
        assert!(!redact_url("https://example.com/rpc?apikey=secret").contains("secret"));
    }
}
