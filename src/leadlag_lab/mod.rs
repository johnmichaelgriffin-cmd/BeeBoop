//! Lead/Lag Lab — synchronized event recording and offline analysis.
//!
//! Records all raw WS events with microsecond local timestamps.
//! Provides offline cross-correlation analysis to measure actual
//! lead/lag between Polymarket token prices and BTC reference prices.
//!
//! This is the MOST IMPORTANT module — we prove the signal exists
//! before writing any trading logic.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{error, info};

use crate::types::RecordedEvent;

/// Event recorder — writes all events to append-only JSONL files.
pub struct EventRecorder {
    writer: Arc<Mutex<BufWriter<File>>>,
    path: PathBuf,
    event_count: Arc<Mutex<u64>>,
}

impl EventRecorder {
    /// Create a new recorder. Creates output directory if needed.
    pub fn new(output_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(output_dir)?;

        let filename = format!(
            "events_{}.jsonl",
            Utc::now().format("%Y%m%d_%H%M%S")
        );
        let path = output_dir.join(&filename);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        info!("leadlag_lab: recording to {}", path.display());

        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
            path,
            event_count: Arc::new(Mutex::new(0)),
        })
    }

    /// Record a single event.
    pub fn record(&self, event: &RecordedEvent) {
        if let Ok(json) = serde_json::to_string(event) {
            if let Ok(mut writer) = self.writer.lock() {
                if writeln!(writer, "{}", json).is_ok() {
                    let _ = writer.flush();
                    if let Ok(mut count) = self.event_count.lock() {
                        *count += 1;
                    }
                }
            }
        }
    }

    /// Get current event count.
    pub fn count(&self) -> u64 {
        *self.event_count.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get the output file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ── Offline Analysis (run separately) ───────────────────────────

/// Normalized price event for analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub source: String,        // "poly_up", "poly_dn", "binance_mid", "chainlink"
    pub price: f64,
    pub source_ts_ms: i64,     // venue/exchange timestamp
    pub local_recv_us: i64,    // local receive timestamp
}

/// Cross-correlation result at a specific lag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LagResult {
    pub lag_ms: i64,
    pub correlation: f64,
    pub n_pairs: usize,
}

/// Compute cross-correlation between two time series at multiple lags.
/// Returns correlation at each lag from -max_lag_ms to +max_lag_ms.
pub fn cross_correlate(
    series_a: &[PricePoint],  // e.g. Polymarket token mid
    series_b: &[PricePoint],  // e.g. Binance BTC mid
    max_lag_ms: i64,
    step_ms: i64,
) -> Vec<LagResult> {
    let mut results = Vec::new();

    // Convert to returns (diff of consecutive prices)
    let returns_a = to_returns(series_a);
    let returns_b = to_returns(series_b);

    if returns_a.is_empty() || returns_b.is_empty() {
        return results;
    }

    let mut lag = -max_lag_ms;
    while lag <= max_lag_ms {
        let (corr, n) = correlation_at_lag(&returns_a, &returns_b, lag);
        results.push(LagResult {
            lag_ms: lag,
            correlation: corr,
            n_pairs: n,
        });
        lag += step_ms;
    }

    results
}

/// Convert price points to (timestamp_ms, return) pairs.
fn to_returns(points: &[PricePoint]) -> Vec<(i64, f64)> {
    if points.len() < 2 {
        return Vec::new();
    }

    let mut returns = Vec::with_capacity(points.len() - 1);
    for i in 1..points.len() {
        if points[i - 1].price > 0.0 {
            let ret = (points[i].price - points[i - 1].price) / points[i - 1].price;
            let ts = points[i].local_recv_us / 1000; // convert to ms
            returns.push((ts, ret));
        }
    }
    returns
}

/// Compute Pearson correlation at a specific lag.
/// Positive lag means series_a is shifted forward (series_a leads if peak at positive lag).
fn correlation_at_lag(
    returns_a: &[(i64, f64)],
    returns_b: &[(i64, f64)],
    lag_ms: i64,
) -> (f64, usize) {
    // For each point in A, find the closest point in B at time (a_time + lag)
    let mut pairs: Vec<(f64, f64)> = Vec::new();

    let mut b_idx = 0;
    for &(a_ts, a_ret) in returns_a {
        let target_ts = a_ts + lag_ms;

        // Advance b_idx to closest point
        while b_idx + 1 < returns_b.len()
            && (returns_b[b_idx + 1].0 - target_ts).abs() < (returns_b[b_idx].0 - target_ts).abs()
        {
            b_idx += 1;
        }

        if b_idx < returns_b.len() {
            let time_diff = (returns_b[b_idx].0 - target_ts).abs();
            if time_diff <= 500 { // within 500ms tolerance
                pairs.push((a_ret, returns_b[b_idx].1));
            }
        }
    }

    if pairs.len() < 10 {
        return (0.0, pairs.len());
    }

    // Pearson correlation
    let n = pairs.len() as f64;
    let mean_a: f64 = pairs.iter().map(|(a, _)| a).sum::<f64>() / n;
    let mean_b: f64 = pairs.iter().map(|(_, b)| b).sum::<f64>() / n;

    let mut cov = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;

    for &(a, b) in &pairs {
        let da = a - mean_a;
        let db = b - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }

    let denom = (var_a * var_b).sqrt();
    if denom == 0.0 {
        return (0.0, pairs.len());
    }

    (cov / denom, pairs.len())
}

/// Print a summary of lead/lag analysis results.
pub fn print_analysis(results: &[LagResult], label_a: &str, label_b: &str) {
    println!("\n=== Lead/Lag Analysis: {} vs {} ===", label_a, label_b);
    println!("{:>8} {:>10} {:>8}", "Lag(ms)", "Corr", "N");
    println!("{}", "-".repeat(30));

    let mut peak_corr = 0.0_f64;
    let mut peak_lag = 0_i64;

    for r in results {
        let marker = if r.correlation.abs() > 0.3 { " ***" } else { "" };
        println!("{:>8} {:>10.4} {:>8}{}", r.lag_ms, r.correlation, r.n_pairs, marker);

        if r.correlation.abs() > peak_corr.abs() {
            peak_corr = r.correlation;
            peak_lag = r.lag_ms;
        }
    }

    println!("\nPeak correlation: {:.4} at lag {}ms", peak_corr, peak_lag);
    if peak_lag > 0 {
        println!("→ {} LEADS {} by ~{}ms", label_a, label_b, peak_lag);
    } else if peak_lag < 0 {
        println!("→ {} LEADS {} by ~{}ms", label_b, label_a, -peak_lag);
    } else {
        println!("→ Synchronous (no lead/lag detected)");
    }
}
