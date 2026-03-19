//! Latency telemetry and health monitoring.
//!
//! Tracks p50/p95/p99/p99.9 for each pipeline stage.
//! Emits periodic summaries to tracing.

use hdrhistogram::Histogram;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Named latency histogram.
pub struct LatencyTracker {
    histograms: Arc<Mutex<HashMap<String, Histogram<u64>>>>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self {
            histograms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a latency sample in microseconds.
    pub fn record(&self, stage: &str, latency_us: u64) {
        let mut map = self.histograms.lock().unwrap();
        let hist = map.entry(stage.to_string()).or_insert_with(|| {
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap() // 1us to 60s
        });
        let _ = hist.record(latency_us);
    }

    /// Get current percentiles for a stage.
    pub fn percentiles(&self, stage: &str) -> Option<Percentiles> {
        let map = self.histograms.lock().unwrap();
        let hist = map.get(stage)?;
        Some(Percentiles {
            count: hist.len(),
            p50: hist.value_at_quantile(0.50),
            p95: hist.value_at_quantile(0.95),
            p99: hist.value_at_quantile(0.99),
            p999: hist.value_at_quantile(0.999),
        })
    }

    /// Log all histograms.
    pub fn log_summary(&self) {
        let map = self.histograms.lock().unwrap();
        for (stage, hist) in map.iter() {
            if hist.len() == 0 {
                continue;
            }
            info!(
                "telemetry [{}]: n={} p50={}us p95={}us p99={}us p99.9={}us",
                stage,
                hist.len(),
                hist.value_at_quantile(0.50),
                hist.value_at_quantile(0.95),
                hist.value_at_quantile(0.99),
                hist.value_at_quantile(0.999),
            );
        }
    }

    /// Reset all histograms.
    pub fn reset(&self) {
        let mut map = self.histograms.lock().unwrap();
        for hist in map.values_mut() {
            hist.reset();
        }
    }
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct Percentiles {
    pub count: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub p999: u64,
}

/// Health check state.
pub struct HealthCheck {
    pub market_ws_connected: bool,
    pub user_ws_connected: bool,
    pub rtds_connected: bool,
    pub binance_connected: bool,
    pub last_market_ws_msg: i64,
    pub last_rtds_msg: i64,
    pub last_binance_msg: i64,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            market_ws_connected: false,
            user_ws_connected: false,
            rtds_connected: false,
            binance_connected: false,
            last_market_ws_msg: 0,
            last_rtds_msg: 0,
            last_binance_msg: 0,
        }
    }
}
