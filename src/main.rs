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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::execution::ExecutionEngine;
use crate::leadlag_lab::EventRecorder;
use crate::strategy::{
    BinanceReturnSignal, ExecutionMode, MarketState, RiskLimits, SignalSnapshot,
    StrategyAction, compute_signal, evaluate,
};
use crate::telemetry::LatencyTracker;
use crate::types::{BotConfig, ObiSignal, OracleLagDirection, OracleLagSignal, RefSource};

const WINDOW_S: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Record, // Phase 1: just record all feeds
    Paper,  // Phase 2: run strategy, log decisions, no execution
    Live,   // Phase 3: full execution with risk limits
}

/// Shared state updated by feed tasks, read by strategy loop.
#[derive(Debug, Default)]
struct SharedState {
    // Binance
    binance_mid: f64,
    binance_mid_history: VecDeque<(i64, f64)>, // (timestamp_us, mid)
    obi: Option<ObiSignal>,

    // Oracle
    chainlink_price: f64,
    chainlink_ts: i64,
    binance_rtds_price: f64,
    binance_rtds_ts: i64,

    // Polymarket token book
    up_best_ask: Option<f64>,
    up_best_bid: Option<f64>,
    dn_best_ask: Option<f64>,
    dn_best_bid: Option<f64>,
    up_token_id: String,
    dn_token_id: String,
    tick_size_up: f64,
    tick_size_dn: f64,

    // Window tracking
    window_start_ts: i64,

    // Filled positions
    up_filled: f64,
    dn_filled: f64,
}

impl SharedState {
    fn binance_return_2s(&self) -> f64 {
        let now_us = chrono::Utc::now().timestamp_micros();
        let target_us = now_us - 2_000_000; // 2 seconds ago
        // Find the price closest to 2s ago
        let old_price = self.binance_mid_history.iter()
            .rev()
            .find(|(ts, _)| *ts <= target_us)
            .map(|(_, p)| *p);

        match old_price {
            Some(old) if old > 0.0 && self.binance_mid > 0.0 => {
                ((self.binance_mid - old) / old) * 10_000.0 // bps
            }
            _ => 0.0,
        }
    }

    fn oracle_lag_signal(&self) -> Option<OracleLagSignal> {
        if self.binance_rtds_price <= 0.0 || self.chainlink_price <= 0.0 {
            return None;
        }
        let lag_bps = ((self.binance_rtds_price - self.chainlink_price) / self.chainlink_price) * 10_000.0;
        let direction = if lag_bps > 0.5 {
            OracleLagDirection::Up
        } else if lag_bps < -0.5 {
            OracleLagDirection::Down
        } else {
            OracleLagDirection::Flat
        };
        Some(OracleLagSignal {
            binance_price: self.binance_rtds_price,
            chainlink_price: self.chainlink_price,
            lag_bps,
            direction,
            timestamp: self.chainlink_ts,
            local_ts: chrono::Utc::now().timestamp_micros(),
        })
    }

    fn market_state(&self) -> MarketState {
        let now = chrono::Utc::now().timestamp();
        let elapsed = if self.window_start_ts > 0 {
            (now - self.window_start_ts) as u64
        } else {
            0
        };
        MarketState {
            up_token_id: self.up_token_id.clone(),
            dn_token_id: self.dn_token_id.clone(),
            up_best_ask: self.up_best_ask,
            up_best_bid: self.up_best_bid,
            dn_best_ask: self.dn_best_ask,
            dn_best_bid: self.dn_best_bid,
            up_filled: self.up_filled,
            dn_filled: self.dn_filled,
            window_start_ts: self.window_start_ts,
            elapsed_s: elapsed,
            tick_size_up: self.tick_size_up,
            tick_size_dn: self.tick_size_dn,
        }
    }
}

fn load_config() -> BotConfig {
    BotConfig {
        poly_private_key: std::env::var("POLY_PRIVATE_KEY").unwrap_or_default(),
        poly_api_key: std::env::var("POLY_API_KEY").unwrap_or_default(),
        poly_api_secret: std::env::var("POLY_SECRET").unwrap_or_default(),
        poly_api_passphrase: std::env::var("POLY_PASSPHRASE").unwrap_or_default(),
        market_slug: "btc-updown-5m".to_string(),
        window_seconds: WINDOW_S,
        start_delay_s: 15,
        cancel_at_s: 240,
        max_position: 200.0,
        max_open_orders: 30,
        stop_loss_usd: -2000.0,
        obi_threshold: 0.3,
        oracle_lag_threshold_bps: 2.0,
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "beeboop=info".parse().unwrap()),
        )
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .init();

    // Install rustls crypto provider (required for TLS WebSocket connections)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load config
    dotenvy::dotenv().ok();
    let config = load_config();

    // Determine mode from CLI args or env
    let mode = match std::env::args().nth(1).as_deref() {
        Some("paper") => Mode::Paper,
        Some("live") => Mode::Live,
        _ => Mode::Record,
    };

    info!("=== BeeBoop v0.2.0 ===");
    info!("Mode: {:?}", mode);

    // Set up event recorder (always active)
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

    // Set up execution engine
    let mut exec = ExecutionEngine::new(telemetry.clone(), mode != Mode::Live);
    if mode != Mode::Record {
        exec.set_credentials(
            config.poly_private_key.clone(),
            config.poly_api_key.clone(),
            config.poly_api_secret.clone(),
            config.poly_api_passphrase.clone(),
        );
    }
    let exec = Arc::new(exec);

    // Shared state
    let state = Arc::new(Mutex::new(SharedState::default()));

    // Create channels
    let (market_tx, mut market_rx) = mpsc::unbounded_channel::<market_ws::MarketEvent>();
    let (rtds_tx, mut rtds_rx) = mpsc::unbounded_channel::<rtds::RtdsEvent>();
    let (binance_tx, mut binance_rx) = mpsc::unbounded_channel::<binance_l2::BinanceEvent>();

    // Market tokens (empty for now — will be populated per-window)
    let market_tokens: Vec<String> = vec![];

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

    // Spawn telemetry logger
    let telem_clone = telemetry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            telem_clone.log_summary();
        }
    });

    // ── Feed processors ──────────────────────────────────────────
    let rec1 = recorder.clone();
    let state1 = state.clone();
    tokio::spawn(async move {
        while let Some(event) = market_rx.recv().await {
            if let market_ws::MarketEvent::Raw(ref raw) = event {
                rec1.record(raw);
            }
            // Update shared state with token BBO
            match &event {
                market_ws::MarketEvent::BestBidAsk(bbo) => {
                    let mut s = state1.lock().await;
                    if bbo.token_id == s.up_token_id {
                        s.up_best_ask = bbo.best_ask;
                        s.up_best_bid = bbo.best_bid;
                    } else if bbo.token_id == s.dn_token_id {
                        s.dn_best_ask = bbo.best_ask;
                        s.dn_best_bid = bbo.best_bid;
                    }
                }
                market_ws::MarketEvent::TickSizeChange(ts) => {
                    let mut s = state1.lock().await;
                    if ts.token_id == s.up_token_id {
                        s.tick_size_up = ts.tick_size;
                    } else if ts.token_id == s.dn_token_id {
                        s.tick_size_dn = ts.tick_size;
                    }
                }
                _ => {}
            }
        }
    });

    let rec2 = recorder.clone();
    let state2 = state.clone();
    tokio::spawn(async move {
        while let Some(event) = rtds_rx.recv().await {
            match &event {
                rtds::RtdsEvent::Raw(raw) => rec2.record(raw),
                rtds::RtdsEvent::Price(ref p) => {
                    let mut s = state2.lock().await;
                    match p.source {
                        RefSource::RtdsChainlink => {
                            s.chainlink_price = p.price;
                            s.chainlink_ts = p.timestamp;
                        }
                        RefSource::RtdsBinance => {
                            s.binance_rtds_price = p.price;
                            s.binance_rtds_ts = p.timestamp;
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    let rec3 = recorder.clone();
    let state3 = state.clone();
    tokio::spawn(async move {
        while let Some(event) = binance_rx.recv().await {
            match &event {
                binance_l2::BinanceEvent::Raw(raw) => rec3.record(raw),
                binance_l2::BinanceEvent::Obi(obi) => {
                    let mut s = state3.lock().await;
                    s.obi = Some(obi.clone());
                }
                binance_l2::BinanceEvent::MidPrice(ref p) => {
                    let mut s = state3.lock().await;
                    s.binance_mid = p.price;
                    let ts = p.local_recv_ts;
                    s.binance_mid_history.push_back((ts, p.price));
                    // Keep last 10 seconds of history
                    let cutoff = ts - 10_000_000;
                    while s.binance_mid_history.front().map_or(false, |(t, _)| *t < cutoff) {
                        s.binance_mid_history.pop_front();
                    }
                }
                _ => {}
            }
        }
    });

    // ── Strategy loop (only in Paper/Live mode) ──────────────────
    if mode == Mode::Paper || mode == Mode::Live {
        let state_strat = state.clone();
        let exec_strat = exec.clone();
        let config_strat = config.clone();

        tokio::spawn(async move {
            info!("Strategy loop starting (500ms cycle)...");
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut risk = RiskLimits::default();
            let mut last_signal_log = 0i64;
            let mut total_decisions = 0u64;
            let mut maker_count = 0u64;
            let mut taker_count = 0u64;
            let mut hold_count = 0u64;

            loop {
                interval.tick().await;

                let s = state_strat.lock().await;

                // Skip if no data yet
                if s.binance_mid <= 0.0 {
                    continue;
                }

                // Compute signals
                let obi = s.obi.clone();
                let oracle_lag = s.oracle_lag_signal();
                let binance_ret = BinanceReturnSignal {
                    return_2s: s.binance_return_2s(),
                    return_500ms: 0.0, // TODO
                    timestamp: chrono::Utc::now().timestamp_micros(),
                };
                let market_state = s.market_state();

                drop(s); // Release lock before executing

                // Compute signal score
                let signal = compute_signal(
                    obi.as_ref(),
                    oracle_lag.as_ref(),
                    Some(&binance_ret),
                );

                // Log signal every 5 seconds
                let now = chrono::Utc::now().timestamp();
                if now - last_signal_log >= 5 {
                    let s = state_strat.lock().await;
                    info!(
                        "SIGNAL: score={:.3} mode={:?} | BN=${:.2} CL=${:.2} lag={:.1}bps ret2s={:.1}bps | OBI={:.3} | UP ask={} DN ask={}",
                        signal.score,
                        signal.mode,
                        s.binance_rtds_price,
                        s.chainlink_price,
                        signal.basis_bps,
                        binance_ret.return_2s,
                        obi.as_ref().map(|o| o.value).unwrap_or(0.0),
                        s.up_best_ask.map(|a| format!("{:.0}c", a * 100.0)).unwrap_or("?".into()),
                        s.dn_best_ask.map(|a| format!("{:.0}c", a * 100.0)).unwrap_or("?".into()),
                    );
                    last_signal_log = now;
                    drop(s);
                }

                // Evaluate strategy
                let actions = evaluate(&market_state, &signal, &risk, &config_strat);

                // Execute actions
                total_decisions += 1;
                for action in &actions {
                    match action {
                        StrategyAction::PlaceOrder(req) => {
                            match signal.mode {
                                ExecutionMode::Taker => {
                                    taker_count += 1;
                                    let spend = req.size * req.price;
                                    info!(
                                        ">>> TAKER FOK: {} ${:.2} on {} | {}",
                                        if req.token_id == market_state.up_token_id { "UP" } else { "DN" },
                                        spend,
                                        &req.token_id[..16.min(req.token_id.len())],
                                        signal.reason,
                                    );
                                    let _ = exec_strat.taker_fok_buy(&req.token_id, spend).await;
                                }
                                ExecutionMode::Maker => {
                                    maker_count += 1;
                                    info!(
                                        ">>> MAKER GTC: {} {:.0}sh @ {:.0}c on {} | {}",
                                        if req.token_id == market_state.up_token_id { "UP" } else { "DN" },
                                        req.size,
                                        req.price * 100.0,
                                        &req.token_id[..16.min(req.token_id.len())],
                                        signal.reason,
                                    );
                                    let _ = exec_strat.maker_gtc_buy(
                                        &req.token_id, req.size, req.price, req.post_only
                                    ).await;
                                }
                                _ => {}
                            }
                            risk.order_count_this_window += 1;
                        }
                        StrategyAction::CancelAll => {
                            info!(">>> CANCEL ALL");
                            let _ = exec_strat.cancel_all().await;
                            risk.cancel_count_this_window = 0;
                            risk.order_count_this_window = 0;
                        }
                        StrategyAction::CancelOrder(id) => {
                            info!(">>> CANCEL {}", id);
                            let _ = exec_strat.cancel_order(id).await;
                            risk.cancel_count_this_window += 1;
                        }
                        StrategyAction::Hold => {
                            hold_count += 1;
                        }
                    }
                }

                // Log summary every 60 decisions (~30s)
                if total_decisions % 60 == 0 {
                    info!(
                        "DECISIONS: total={} maker={} taker={} hold={} open_orders={}",
                        total_decisions, maker_count, taker_count, hold_count,
                        exec_strat.open_order_count().await,
                    );
                }
            }
        });
    }

    info!("All feeds running. Press Ctrl+C to stop.");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await.ok();
    info!("Shutting down... recorded {} events", recorder.count());
    telemetry.log_summary();
}
