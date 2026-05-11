use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::{routing::get, Router};
use eyre::Result;

#[derive(Default)]
pub struct Metrics {
    pub attempts: AtomicU64,
    pub accepted_solutions: AtomicU64,
    pub rejected_solutions: AtomicU64,
    pub submitted_txs: AtomicU64,
    pub stale_solutions: AtomicU64,
    pub broadcast_errors: AtomicU64,
    pub replacements: AtomicU64,
    pub confirmations: AtomicU64,
    pub failures: AtomicU64,
    devices: Mutex<HashMap<usize, DeviceStat>>,
}

#[derive(Default, Clone)]
pub struct DeviceStat {
    pub name: String,
    pub attempts: u64,
    pub last_hashrate: f64,
}

impl Metrics {
    pub fn record_device(&self, index: usize, name: &str, attempts: u64, hashrate: f64) {
        let mut g = self.devices.lock().unwrap();
        g.insert(
            index,
            DeviceStat {
                name: name.to_string(),
                attempts,
                last_hashrate: hashrate,
            },
        );
    }

    pub fn render_full(&self) -> String {
        let devices = self.devices.lock().unwrap().clone();
        let mut s = self.render_counters();
        for (idx, d) in devices {
            s.push_str(&format!(
                "hash256_device_attempts_total{{device=\"{}\",name=\"{}\"}} {}\n",
                idx, d.name, d.attempts
            ));
            s.push_str(&format!(
                "hash256_device_hashrate{{device=\"{}\",name=\"{}\"}} {}\n",
                idx, d.name, d.last_hashrate
            ));
        }
        s
    }

    fn render_counters(&self) -> String {
        format!(
            "hash256_attempts_total {}\n\
             hash256_solutions_accepted_total {}\n\
             hash256_solutions_rejected_total {}\n\
             hash256_txs_submitted_total {}\n\
             hash256_stale_solutions_total {}\n\
             hash256_broadcast_errors_total {}\n\
             hash256_replacements_total {}\n\
             hash256_confirmations_total {}\n\
             hash256_failures_total {}\n",
            self.attempts.load(Ordering::Relaxed),
            self.accepted_solutions.load(Ordering::Relaxed),
            self.rejected_solutions.load(Ordering::Relaxed),
            self.submitted_txs.load(Ordering::Relaxed),
            self.stale_solutions.load(Ordering::Relaxed),
            self.broadcast_errors.load(Ordering::Relaxed),
            self.replacements.load(Ordering::Relaxed),
            self.confirmations.load(Ordering::Relaxed),
            self.failures.load(Ordering::Relaxed),
        )
    }

    pub fn render(&self) -> String {
        // Synchronous render (no per-device snapshot).
        self.render_counters()
    }
}

pub async fn serve_metrics(addr_str: String, metrics: Arc<Metrics>) -> Result<()> {
    let addr: SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("invalid METRICS_BIND={addr_str}: {e}; metrics server disabled");
            return Ok(());
        }
    };
    let app = Router::new()
        .route(
            "/metrics",
            get({
                let m = metrics.clone();
                move || async move { m.render_full() }
            }),
        )
        .route("/", get(|| async { "hash256 metrics: GET /metrics" }));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("metrics bind failed on {addr}: {e}; metrics server disabled");
            return Ok(());
        }
    };
    tracing::info!("metrics listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
