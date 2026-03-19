//! BeeBoop — Low-latency event-driven trading bot for Polymarket BTC 5-min markets.
//!
//! Phase 1: Record mode — connect all feeds, record everything, prove the signal.
//! Phase 2: Paper trading — run strategy against live data, log decisions, don't trade.
//! Phase 3: Live — execute trades with risk limits.

mod types;
mod market_ws;
mod user_ws;
mod rtds;
mod binance_l2;
mod strategy;
mod execution;
mod telemetry;
mod leadlag_lab;
mod sim;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::leadlag_lab::EventRecorder;
use crate::telemetry::LatencyTracker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Record,     // Phase 1: just record all feeds
    Paper,      // Phase 2: run strategy, log decisions, no execution
    Live,       // Phase 3: full execution with risk limits
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "beeboop=info".parse().unwrap()),
        )
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .init();

    info!("=== BeeBoop v0.1.0 ===");

    // Load config
    dotenvy::dotenv().ok();

    // For Phase 1, we just record everything
    let mode = Mode::Record;
    info!("Mode: {:?}", mode);

    // Set up event recorder
    let data_dir = PathBuf::from("data");
    let recorder = match EventRecorder::new(&data_dir) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            error!("Failed to create event recorder: {}", e);
            return;
        }
    };

    // Set up telemetry
    let telemetry = Arc::new(LatencyTracker::new());

    // Create channels
    let (market_tx, mut market_rx) = mpsc::unbounded_channel::<market_ws::MarketEvent>();
    let (rtds_tx, mut rtds_rx) = mpsc::unbounded_channel::<rtds::RtdsEvent>();
    let (binance_tx, mut binance_rx) = mpsc::unbounded_channel::<binance_l2::BinanceEvent>();

    // TODO: Fetch current market token IDs from Gamma API
    // For now, these would be set per-window
    let market_tokens: Vec<String> = vec![
        // Will be populated dynamically per window
    ];

    // Spawn feed tasks
    let market_tokens_clone = market_tokens.clone();
    tokio::spawn(async move {
        market_ws::run(market_tokens_clone, market_tx).await;
    });

    tokio::spawn(async move {
        rtds::run("btc/usd".to_string(), rtds_tx).await;
    });

    tokio::spawn(async move {
        binance_l2::run(binance_tx).await;
    });

    // Spawn telemetry logger (every 30s)
    let telem_clone = telemetry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            telem_clone.log_summary();
        }
    });

    // Main event loop — record everything
    let rec1 = recorder.clone();
    let rec2 = recorder.clone();
    let rec3 = recorder.clone();

    info!("Starting event loop — recording all feeds to {}", data_dir.display());

    // Process all channels concurrently
    let _market_handle = tokio::spawn(async move {
        while let Some(event) = market_rx.recv().await {
            if let market_ws::MarketEvent::Raw(ref raw) = event {
                rec1.record(raw);
            }
        }
    });

    let _rtds_handle = tokio::spawn(async move {
        while let Some(event) = rtds_rx.recv().await {
            match &event {
                rtds::RtdsEvent::Raw(raw) => rec2.record(raw),
                rtds::RtdsEvent::Price(ref p) => {
                    info!(
                        "RTDS {:?} {}: ${:.2} (src_ts={}ms)",
                        p.source,
                        p.symbol,
                        p.price,
                        p.timestamp,
                    );
                }
            }
        }
    });

    let _binance_handle = tokio::spawn(async move {
        let mut last_obi_log = 0i64;
        while let Some(event) = binance_rx.recv().await {
            match &event {
                binance_l2::BinanceEvent::Raw(raw) => rec3.record(raw),
                binance_l2::BinanceEvent::Obi(obi) => {
                    let now = chrono::Utc::now().timestamp();
                    if now - last_obi_log >= 5 {
                        info!(
                            "OBI: {:.3} ({} levels) | signal: {}",
                            obi.value,
                            obi.levels_used,
                            if obi.value > 0.3 { "BUY" }
                            else if obi.value < -0.3 { "SELL" }
                            else { "neutral" }
                        );
                        last_obi_log = now;
                    }
                }
                binance_l2::BinanceEvent::MidPrice(ref _p) => {}
                _ => {}
            }
        }
    });

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await.ok();
    info!("Shutting down... recorded {} events", recorder.count());

    // Telemetry final summary
    telemetry.log_summary();
}
