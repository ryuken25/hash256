use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use eyre::Result;
use tokio::sync::watch;

use crate::{config::AppConfig, contract::WorkState, rpc_pool::RpcPool};

#[derive(Clone)]
pub struct StateWatcher {
    cfg: AppConfig,
    rpc: RpcPool,
    miner_address: Address,
    tx: watch::Sender<Option<WorkState>>,
    epoch_token: Arc<AtomicU64>,
}

impl StateWatcher {
    pub fn new(cfg: AppConfig, rpc: RpcPool, miner_address: Address) -> Self {
        let (tx, _) = watch::channel(None);
        Self {
            cfg,
            rpc,
            miner_address,
            tx,
            epoch_token: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<WorkState>> {
        self.tx.subscribe()
    }

    pub fn token(&self) -> Arc<AtomicU64> {
        self.epoch_token.clone()
    }

    pub fn current(&self) -> Option<WorkState> {
        self.tx.borrow().clone()
    }

    pub async fn poll_once(&self) -> Result<WorkState> {
        let block = self.rpc.block_number().await?;
        let state = self.rpc.mining_state(self.cfg.contract).await?;
        let challenge = self
            .rpc
            .get_challenge(self.cfg.contract, self.miner_address)
            .await?;
        let target = state.difficulty;
        let effective_target = self.cfg.effective_target(target);
        Ok(WorkState {
            miner_address: self.miner_address,
            block_number: block,
            epoch: state.epoch.to::<u64>(),
            challenge,
            target,
            effective_target,
            blocks_left: state.epoch_blocks_left.to::<u64>(),
        })
    }

    pub async fn run(self) {
        let interval = Duration::from_millis(self.cfg.state_check_interval_ms.max(50));
        let mut last_challenge = B256::ZERO;
        let mut last_epoch = u64::MAX;
        loop {
            match self.poll_once().await {
                Ok(s) => {
                    if s.challenge != last_challenge || s.epoch != last_epoch {
                        last_challenge = s.challenge;
                        last_epoch = s.epoch;
                        self.epoch_token.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(
                            block = s.block_number,
                            epoch = s.epoch,
                            blocks_left = s.blocks_left,
                            challenge = %crate::contract::hex_b256(&s.challenge),
                            target = %s.target,
                            "state changed"
                        );
                    }
                    let _ = self.tx.send(Some(s));
                }
                Err(e) => tracing::warn!("state watcher poll failed: {e}"),
            }
            tokio::time::sleep(interval).await;
        }
    }
}
