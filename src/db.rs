use std::sync::{Arc, Mutex};

use alloy::primitives::U256;
use eyre::Result;
use rusqlite::{params, Connection};

use crate::gas::GasPlan;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct TxRecord {
    pub tx_nonce: u64,
    pub raw_tx_hash: String,
    pub calldata: String,
    pub pow_nonce: String,
    pub challenge: String,
    pub epoch: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub gas_limit: u64,
    pub status: String,
    pub created_at_block: u64,
    pub created_at_time: i64,
}

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_SUCCESS: &str = "success";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_REPLACED: &str = "replaced";
pub const STATUS_CANCELLED: &str = "cancelled";
pub const STATUS_STALE: &str = "stale";

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS txs (
                tx_nonce INTEGER PRIMARY KEY,
                raw_tx_hash TEXT,
                calldata TEXT NOT NULL,
                pow_nonce TEXT NOT NULL,
                challenge TEXT NOT NULL,
                epoch INTEGER NOT NULL,
                max_fee_per_gas TEXT NOT NULL,
                max_priority_fee_per_gas TEXT NOT NULL,
                gas_limit INTEGER NOT NULL DEFAULT 200000,
                status TEXT NOT NULL,
                created_at_block INTEGER NOT NULL,
                created_at_time INTEGER NOT NULL,
                last_broadcast_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS txs_status_idx ON txs(status);",
        )?;
        // Add gas_limit column for older DBs (best effort).
        let _ = conn.execute("ALTER TABLE txs ADD COLUMN gas_limit INTEGER NOT NULL DEFAULT 200000", []);
        let _ = conn.execute("ALTER TABLE txs ADD COLUMN last_broadcast_at INTEGER NOT NULL DEFAULT 0", []);
        Ok(())
    }

    pub fn load_next_nonce(&self) -> Result<Option<u64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM meta WHERE key='next_nonce'")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            Ok(s.parse().ok())
        } else {
            Ok(None)
        }
    }

    pub fn store_next_nonce(&self, nonce: u64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO meta(key,value) VALUES('next_nonce',?1) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![nonce.to_string()],
        )?;
        Ok(())
    }

    pub fn insert_allocated_tx(
        &self,
        tx_nonce: u64,
        calldata: &[u8],
        pow_nonce: U256,
        challenge: &str,
        epoch: u64,
        gas: GasPlan,
        block: u64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO txs(tx_nonce, raw_tx_hash, calldata, pow_nonce, challenge, epoch, \
                max_fee_per_gas, max_priority_fee_per_gas, gas_limit, status, created_at_block, created_at_time, last_broadcast_at) \
             VALUES(?1,'',?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10,?10)",
            params![
                tx_nonce,
                format!("0x{}", hex::encode(calldata)),
                pow_nonce.to_string(),
                challenge,
                epoch,
                gas.max_fee_per_gas.to_string(),
                gas.max_priority_fee_per_gas.to_string(),
                gas.gas_limit,
                block,
                now
            ],
        )?;
        Ok(())
    }

    pub fn update_tx_hash(&self, tx_nonce: u64, hash: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.lock().unwrap().execute(
            "UPDATE txs SET raw_tx_hash=?2, last_broadcast_at=?3 WHERE tx_nonce=?1",
            params![tx_nonce, hash, now],
        )?;
        Ok(())
    }

    pub fn update_status(&self, tx_nonce: u64, status: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE txs SET status=?2 WHERE tx_nonce=?1",
            params![tx_nonce, status],
        )?;
        Ok(())
    }

    pub fn update_fee(&self, tx_nonce: u64, gas: GasPlan) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.lock().unwrap().execute(
            "UPDATE txs SET max_fee_per_gas=?2, max_priority_fee_per_gas=?3, gas_limit=?4, last_broadcast_at=?5 WHERE tx_nonce=?1",
            params![
                tx_nonce,
                gas.max_fee_per_gas.to_string(),
                gas.max_priority_fee_per_gas.to_string(),
                gas.gas_limit,
                now
            ],
        )?;
        Ok(())
    }

    pub fn list_pending(&self) -> Result<Vec<TxRecord>> {
        self.list_filtered("status='pending'")
    }

    pub fn list_txs(&self) -> Result<Vec<TxRecord>> {
        self.list_filtered("1=1")
    }

    fn list_filtered(&self, where_clause: &str) -> Result<Vec<TxRecord>> {
        let conn = self.conn.lock().unwrap();
        let q = format!(
            "SELECT tx_nonce,raw_tx_hash,calldata,pow_nonce,challenge,epoch,\
                    max_fee_per_gas,max_priority_fee_per_gas,gas_limit,status,\
                    created_at_block,created_at_time \
             FROM txs WHERE {where_clause} ORDER BY tx_nonce DESC LIMIT 500"
        );
        let mut stmt = conn.prepare(&q)?;
        let rows = stmt.query_map([], |r| {
            Ok(TxRecord {
                tx_nonce: r.get(0)?,
                raw_tx_hash: r.get(1)?,
                calldata: r.get(2)?,
                pow_nonce: r.get(3)?,
                challenge: r.get(4)?,
                epoch: r.get(5)?,
                max_fee_per_gas: r.get::<_, String>(6)?.parse().unwrap_or(0),
                max_priority_fee_per_gas: r.get::<_, String>(7)?.parse().unwrap_or(0),
                gas_limit: r.get::<_, i64>(8).unwrap_or(200_000) as u64,
                status: r.get(9)?,
                created_at_block: r.get(10)?,
                created_at_time: r.get(11)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}
