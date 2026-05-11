use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::eips::eip2930::AccessList;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    config::AppConfig,
    contract::{hash_below_target, hash_nonce, hex_b256, mine_calldata, WorkState},
    db::{self, Db},
    gas::{bump_eip1559, gwei_to_wei, plan_gas, GasPlan},
    metrics::Metrics,
    rpc_pool::RpcPool,
    tx_nonce_manager::TxNonceManager,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionSubmit {
    pub worker_id: String,
    pub device_id: u32,
    pub epoch: u64,
    pub challenge: B256,
    pub nonce: U256,
    pub hash: B256,
    pub attempts: u64,
    pub hashrate: f64,
    pub found_at_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionResult {
    pub accepted: bool,
    pub reason: String,
    pub tx_nonce: Option<u64>,
    pub tx_hash: Option<String>,
}

pub struct Submitter {
    cfg: AppConfig,
    rpc: RpcPool,
    tx_nonce_manager: Option<TxNonceManager>,
    signer: Option<PrivateKeySigner>,
    miner_address: Address,
    metrics: Arc<Metrics>,
    pending_lock: Mutex<()>,
    /// Set to true after first successful broadcast, used to skip eth_call
    /// simulate on subsequent submissions when SKIP_SIMULATE_AFTER_SUCCESS=true.
    had_success: std::sync::atomic::AtomicBool,
}

impl Submitter {
    pub fn new(
        cfg: AppConfig,
        rpc: RpcPool,
        tx_nonce_manager: Option<TxNonceManager>,
        signer: Option<PrivateKeySigner>,
        miner_address: Address,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            cfg,
            rpc,
            tx_nonce_manager,
            signer,
            miner_address,
            metrics,
            pending_lock: Mutex::new(()),
            had_success: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn db(&self) -> Option<Db> {
        self.tx_nonce_manager.as_ref().map(|m| m.db())
    }

    pub async fn validate_solution(&self, sol: &SolutionSubmit, state: &WorkState) -> Result<()> {
        if sol.epoch != state.epoch {
            return Err(eyre!("stale epoch"));
        }
        if sol.challenge != state.challenge {
            return Err(eyre!("stale challenge"));
        }
        let computed = hash_nonce(&sol.challenge, sol.nonce);
        if computed != sol.hash {
            return Err(eyre!("hash mismatch"));
        }
        if !hash_below_target(&sol.hash, state.effective_target) {
            return Err(eyre!("hash above effective target"));
        }
        if !hash_below_target(&sol.hash, state.target) {
            return Err(eyre!("hash above target"));
        }
        Ok(())
    }

    pub async fn submit_solution(
        &self,
        sol: SolutionSubmit,
        state: WorkState,
    ) -> Result<SolutionResult> {
        self.validate_solution(&sol, &state).await?;
        let current_challenge = self
            .rpc
            .get_challenge(self.cfg.contract, self.miner_address)
            .await
            .map_err(|e| eyre!("recheck challenge: {e}"))?;
        if current_challenge != sol.challenge {
            self.metrics.stale_solutions.fetch_add(1, Ordering::Relaxed);
            return Err(eyre!("challenge changed before signing"));
        }
        let mgr = self
            .tx_nonce_manager
            .as_ref()
            .ok_or_else(|| eyre!("tx nonce manager not configured"))?;
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| eyre!("signer not configured (need PRIVATE_KEY)"))?;
        if !self.cfg.enable_parallel_pending_tx {
            let _g = self.pending_lock.lock().await;
            self.do_submit(&sol, &state, mgr, signer).await
        } else {
            self.do_submit(&sol, &state, mgr, signer).await
        }
    }

    async fn do_submit(
        &self,
        sol: &SolutionSubmit,
        state: &WorkState,
        mgr: &TxNonceManager,
        signer: &PrivateKeySigner,
    ) -> Result<SolutionResult> {
        let calldata = mine_calldata(sol.nonce);
        // Simulate only if not yet had success OR explicit safety mode.
        // Skipping after first success drops latency by ~300ms (one RPC roundtrip).
        let skip_sim = self.cfg.skip_simulate_after_success
            && self.had_success.load(Ordering::Relaxed);
        if !skip_sim {
            self.rpc
                .eth_call_tx(self.miner_address, self.cfg.contract, &calldata, 200_000)
                .await
                .map_err(|e| eyre!("eth_call simulate failed: {e}"))?;
        }
        // Fetch base fee + estimate in parallel (saves another roundtrip).
        let (fee, estimated) = tokio::join!(
            self.rpc.fee_history(4),
            self.rpc.estimate_gas(
                self.miner_address,
                self.cfg.contract,
                &calldata,
                self.cfg.gas_limit_cap,
            )
        );
        let base_fee = fee.ok().map(|f| f.base_fee_per_gas);
        let estimated = estimated.ok();
        let mut gas = plan_gas(&self.cfg, base_fee, estimated.or(Some(200_000)));
        // Late-epoch aggressive bump: ensure inclusion before challenge changes.
        if self.cfg.late_epoch_priority_gwei > 0.0
            && state.blocks_left <= self.cfg.late_epoch_threshold_blocks
        {
            let bumped_priority = gwei_to_wei(self.cfg.late_epoch_priority_gwei);
            let bumped_max = bumped_priority.saturating_add(base_fee.unwrap_or(0).saturating_mul(2));
            if bumped_priority > gas.max_priority_fee_per_gas {
                tracing::info!(
                    blocks_left = state.blocks_left,
                    bump_to_gwei = self.cfg.late_epoch_priority_gwei,
                    "late-epoch gas bump"
                );
                gas.max_priority_fee_per_gas = bumped_priority;
                gas.max_fee_per_gas = bumped_max.max(gas.max_fee_per_gas);
            }
        }
        let tx_nonce = mgr
            .allocate(
                &calldata,
                sol.nonce,
                &hex_b256(&sol.challenge),
                sol.epoch,
                gas,
                state.block_number,
            )
            .await?;
        let (raw_tx, tx_hash) =
            sign_eip1559(self.cfg.chain_id, tx_nonce, gas, self.cfg.contract, &calldata, signer)?;
        let outcome = match self.rpc.broadcast_raw_tx(&raw_tx).await {
            Ok(o) => o,
            Err(e) => {
                let _ = mgr.db().update_status(tx_nonce, db::STATUS_FAILED);
                return Err(eyre!("broadcast failed: {e}"));
            }
        };
        let hash_str = format!("0x{}", hex::encode(tx_hash));
        mgr.db().update_tx_hash(tx_nonce, &hash_str)?;
        self.metrics.submitted_txs.fetch_add(1, Ordering::Relaxed);
        self.had_success.store(true, Ordering::Relaxed);
        if !outcome.errors.is_empty() {
            self.metrics
                .broadcast_errors
                .fetch_add(outcome.errors.len() as u64, Ordering::Relaxed);
        }
        tracing::info!(
            tx_nonce,
            tx_hash = %hash_str,
            accepted = outcome.accepted_count,
            already_known = outcome.already_known_count,
            errors = outcome.errors.len(),
            max_fee_gwei = format!("{:.4}", gas.max_fee_per_gas as f64 / 1e9),
            priority_gwei = format!("{:.4}", gas.max_priority_fee_per_gas as f64 / 1e9),
            "broadcasted tx"
        );
        Ok(SolutionResult {
            accepted: true,
            reason: format!(
                "broadcast accepted={} already_known={} errors={}",
                outcome.accepted_count, outcome.already_known_count, outcome.errors.len()
            ),
            tx_nonce: Some(tx_nonce),
            tx_hash: Some(hash_str),
        })
    }

    pub async fn receipt_watcher_loop(self: Arc<Self>) {
        let Some(mgr) = self.tx_nonce_manager.clone() else {
            return;
        };
        let interval = Duration::from_secs(6);
        loop {
            tokio::time::sleep(interval).await;
            let txs = match mgr.db().list_pending() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("list_pending failed: {e}");
                    continue;
                }
            };
            for tx in txs {
                if tx.raw_tx_hash.is_empty() {
                    continue;
                }
                match self.rpc.tx_receipt(&tx.raw_tx_hash).await {
                    Ok(Some(r)) => {
                        let status = if r.success {
                            db::STATUS_SUCCESS
                        } else {
                            db::STATUS_FAILED
                        };
                        if let Err(e) = mgr.db().update_status(tx.tx_nonce, status) {
                            tracing::warn!("update_status: {e}");
                        }
                        if r.success {
                            self.metrics.confirmations.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                tx_nonce = tx.tx_nonce,
                                tx_hash = %tx.raw_tx_hash,
                                block = r.block_number,
                                "tx confirmed"
                            );
                        } else {
                            self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                tx_nonce = tx.tx_nonce,
                                tx_hash = %tx.raw_tx_hash,
                                block = r.block_number,
                                "tx failed (reverted)"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("tx_receipt {}: {e}", tx.raw_tx_hash),
                }
            }
        }
    }

    pub async fn replacement_loop(self: Arc<Self>) {
        let (Some(mgr), Some(signer)) =
            (self.tx_nonce_manager.clone(), self.signer.clone())
        else {
            return;
        };
        let interval = Duration::from_secs(self.cfg.print_interval_sec.max(5));
        loop {
            tokio::time::sleep(interval).await;
            let block = match self.rpc.block_number().await {
                Ok(b) => b,
                Err(_) => continue,
            };
            let txs = match mgr.db().list_pending() {
                Ok(t) => t,
                Err(_) => continue,
            };
            for tx in txs {
                let age = block.saturating_sub(tx.created_at_block);
                if age < self.cfg.pending_replace_after_blocks {
                    continue;
                }
                let old_gas = GasPlan {
                    gas_limit: tx.gas_limit,
                    max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                    max_fee_per_gas: tx.max_fee_per_gas,
                };
                let max_cap_wei = gwei_to_wei(self.cfg.max_fee_gwei_cap);
                let new_gas =
                    bump_eip1559(old_gas, self.cfg.fee_bump_percent, max_cap_wei);
                if new_gas.max_fee_per_gas == old_gas.max_fee_per_gas
                    && new_gas.max_priority_fee_per_gas == old_gas.max_priority_fee_per_gas
                {
                    continue;
                }
                let calldata_bytes = match hex::decode(tx.calldata.trim_start_matches("0x")) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let (raw_tx, tx_hash) = match sign_eip1559(
                    self.cfg.chain_id,
                    tx.tx_nonce,
                    new_gas,
                    self.cfg.contract,
                    &calldata_bytes,
                    &signer,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("sign replacement: {e}");
                        continue;
                    }
                };
                match self.rpc.broadcast_raw_tx(&raw_tx).await {
                    Ok(_) => {
                        let hash_str = format!("0x{}", hex::encode(tx_hash));
                        let _ = mgr.db().update_fee(tx.tx_nonce, new_gas);
                        let _ = mgr.db().update_tx_hash(tx.tx_nonce, &hash_str);
                        self.metrics.replacements.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(
                            tx_nonce = tx.tx_nonce,
                            age_blocks = age,
                            new_max_fee_gwei = format!("{:.4}", new_gas.max_fee_per_gas as f64 / 1e9),
                            new_priority_gwei = format!("{:.4}", new_gas.max_priority_fee_per_gas as f64 / 1e9),
                            "replaced pending tx with bumped fee"
                        );
                    }
                    Err(e) => {
                        let m = e.to_string().to_lowercase();
                        if m.contains("nonce too low") || m.contains("already known") {
                            let _ = mgr.db().update_status(tx.tx_nonce, db::STATUS_REPLACED);
                        }
                        tracing::warn!(tx_nonce = tx.tx_nonce, "replacement broadcast failed: {e}");
                    }
                }
            }
        }
    }
}

fn sign_eip1559(
    chain_id: u64,
    nonce: u64,
    gas: GasPlan,
    to: Address,
    calldata: &[u8],
    signer: &PrivateKeySigner,
) -> Result<(String, B256)> {
    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: gas.gas_limit,
        max_fee_per_gas: gas.max_fee_per_gas,
        max_priority_fee_per_gas: gas.max_priority_fee_per_gas,
        to: TxKind::Call(to),
        value: U256::ZERO,
        access_list: AccessList::default(),
        input: Bytes::from(calldata.to_vec()),
    };
    let sig = signer
        .sign_transaction_sync(&mut tx)
        .map_err(|e| eyre!("sign tx: {e}"))?;
    let signed = tx.into_signed(sig);
    let envelope: TxEnvelope = signed.into();
    let mut buf = Vec::new();
    envelope.encode_2718(&mut buf);
    let raw = format!("0x{}", hex::encode(&buf));
    Ok((raw, *envelope.tx_hash()))
}

pub fn classify_tx_error(msg: &str) -> &'static str {
    let m = msg.to_lowercase();
    if m.contains("nonce too low") {
        "nonce_too_low"
    } else if m.contains("replacement transaction underpriced") || m.contains("underpriced") {
        "replacement_underpriced"
    } else if m.contains("already known") {
        "already_known"
    } else if m.contains("insufficient funds") {
        "insufficient_funds"
    } else if m.contains("execution reverted") {
        "execution_reverted"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_errors() {
        assert_eq!(classify_tx_error("nonce too low"), "nonce_too_low");
        assert_eq!(
            classify_tx_error("replacement transaction underpriced"),
            "replacement_underpriced"
        );
        assert_eq!(classify_tx_error("already known"), "already_known");
    }
}
