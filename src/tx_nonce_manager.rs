use alloy::primitives::{Address, U256};
use eyre::Result;
use tokio::sync::Mutex;

use crate::{db::Db, gas::GasPlan, rpc_pool::RpcPool};

#[derive(Clone)]
pub struct TxNonceManager {
    inner: std::sync::Arc<Mutex<Inner>>,
    db: Db,
}

struct Inner {
    next_nonce: u64,
}

impl TxNonceManager {
    pub async fn new(address: Address, rpc: &RpcPool, db: Db) -> Result<Self> {
        let pending = rpc.transaction_count(address, "pending").await.unwrap_or(0);
        let latest = rpc.transaction_count(address, "latest").await.unwrap_or(0);
        let sqlite = db.load_next_nonce()?.unwrap_or(0);
        let next_nonce = pending.max(latest).max(sqlite);
        db.store_next_nonce(next_nonce)?;
        Ok(Self {
            inner: std::sync::Arc::new(Mutex::new(Inner { next_nonce })),
            db,
        })
    }

    pub async fn allocate(
        &self,
        calldata: &[u8],
        pow_nonce: U256,
        challenge: &str,
        epoch: u64,
        gas: GasPlan,
        block: u64,
    ) -> Result<u64> {
        let mut guard = self.inner.lock().await;
        let n = guard.next_nonce;
        guard.next_nonce += 1;
        self.db.store_next_nonce(guard.next_nonce)?;
        self.db
            .insert_allocated_tx(n, calldata, pow_nonce, challenge, epoch, gas, block)?;
        Ok(n)
    }

    pub fn db(&self) -> Db {
        self.db.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gas::GasPlan;

    #[tokio::test]
    async fn allocation_is_serialized() {
        let db = Db::open(":memory:").unwrap();
        db.store_next_nonce(10).unwrap();
        let mgr = TxNonceManager {
            inner: std::sync::Arc::new(Mutex::new(Inner { next_nonce: 10 })),
            db,
        };
        let gas = GasPlan {
            gas_limit: 1,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        };
        let mut handles = Vec::new();
        for _ in 0..20 {
            let m = mgr.clone();
            handles.push(tokio::spawn(async move {
                m.allocate(&[1], U256::from(1), "0x00", 1, gas, 1)
                    .await
                    .unwrap()
            }));
        }
        let mut got = Vec::new();
        for h in handles {
            got.push(h.await.unwrap());
        }
        got.sort_unstable();
        assert_eq!(got, (10u64..30u64).collect::<Vec<_>>());
    }
}
