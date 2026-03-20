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
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::execution::ExecutionEngine;
use crate::leadlag_lab::EventRecorder;
use crate::strategy::{
    BinanceReturnSignal, ExecutionMode, MarketState, Position, PositionState, RiskLimits,
    SignalSnapshot, StrategyAction, compute_signal, evaluate, evaluate_scalp,
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

    // Pre-warm: authenticate with Polymarket CLOB at startup
    if mode != Mode::Record {
        info!("Pre-warming CLOB authentication (paper_mode={})...", exec.paper_mode);
        match exec.ensure_auth().await {
            Ok(()) => info!("CLOB authentication SUCCESS — ready to trade"),
            Err(e) => error!("CLOB authentication FAILED: {} — orders will fail!", e),
        }
    }

    // Shared state
    let state = Arc::new(RwLock::new(SharedState::default()));

    // Create channels
    let (market_tx, mut market_rx) = mpsc::unbounded_channel::<market_ws::MarketEvent>();
    let (rtds_tx, mut rtds_rx) = mpsc::unbounded_channel::<rtds::RtdsEvent>();
    let (binance_tx, mut binance_rx) = mpsc::unbounded_channel::<binance_l2::BinanceEvent>();
    let (token_info_tx, mut token_info_rx) = mpsc::unbounded_channel::<Vec<market_ws::TokenInfo>>();

    // Market tokens (empty for now — will be populated per-window)
    let market_tokens: Vec<String> = vec![];

    // Spawn token info receiver — updates shared state with UP/DN token IDs
    let state_tokens = state.clone();
    tokio::spawn(async move {
        while let Some(infos) = token_info_rx.recv().await {
            let mut s = state_tokens.write().await;
            let now = chrono::Utc::now().timestamp();
            let current_wts = now - (now % 300);

            // Find current window's UP and DN tokens
            for ti in &infos {
                if ti.window_ts != current_wts {
                    continue;
                }
                let outcome_lower = ti.outcome.to_lowercase();
                if outcome_lower.contains("up") {
                    if s.up_token_id != ti.token_id {
                        info!("state: UP token = {}...{}", &ti.token_id[..8.min(ti.token_id.len())], &ti.token_id[ti.token_id.len().saturating_sub(8)..]);
                        s.up_token_id = ti.token_id.clone();
                        s.up_best_ask = None;
                        s.up_best_bid = None;
                    }
                } else if outcome_lower.contains("down") {
                    if s.dn_token_id != ti.token_id {
                        info!("state: DN token = {}...{}", &ti.token_id[..8.min(ti.token_id.len())], &ti.token_id[ti.token_id.len().saturating_sub(8)..]);
                        s.dn_token_id = ti.token_id.clone();
                        s.dn_best_ask = None;
                        s.dn_best_bid = None;
                    }
                }
            }
            s.window_start_ts = current_wts;
        }
    });

    // Spawn feed tasks
    let market_tokens_clone = market_tokens.clone();
    tokio::spawn(async move {
        market_ws::run(market_tokens_clone, market_tx, Some(token_info_tx)).await;
    });

    tokio::spawn(async move {
        rtds::run("btc/usd".to_string(), rtds_tx).await;
    });

    tokio::spawn(async move {
        binance_l2::run(binance_tx).await;
    });

    // REST fallback: poll token prices every 500ms for speed
    let state_rest = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            interval.tick().await;
            let s = state_rest.read().await;
            let up_id = s.up_token_id.clone();
            let dn_id = s.dn_token_id.clone();
            drop(s);

            if up_id.is_empty() || dn_id.is_empty() {
                continue;
            }

            // Poll both tokens in parallel
            let (up_result, dn_result) = tokio::join!(
                market_ws::poll_book_rest(&up_id),
                market_ws::poll_book_rest(&dn_id),
            );

            let mut s = state_rest.write().await;
            if let Some((bid, ask)) = up_result {
                s.up_best_bid = Some(bid);
                s.up_best_ask = Some(ask);
            }
            if let Some((bid, ask)) = dn_result {
                s.dn_best_bid = Some(bid);
                s.dn_best_ask = Some(ask);
            }
        }
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
                    let mut s = state1.write().await;
                    if bbo.token_id == s.up_token_id {
                        s.up_best_ask = bbo.best_ask;
                        s.up_best_bid = bbo.best_bid;
                    } else if bbo.token_id == s.dn_token_id {
                        s.dn_best_ask = bbo.best_ask;
                        s.dn_best_bid = bbo.best_bid;
                    }
                }
                market_ws::MarketEvent::TickSizeChange(ts) => {
                    let mut s = state1.write().await;
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
                    let mut s = state2.write().await;
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
                    let mut s = state3.write().await;
                    s.obi = Some(obi.clone());
                }
                binance_l2::BinanceEvent::MidPrice(ref p) => {
                    let mut s = state3.write().await;
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
            info!("Strategy loop starting (100ms cycle — SPEED MODE)...");
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut risk = RiskLimits::default();
            let mut position = Position::new_flat();
            let mut last_scalp_exit_us: i64 = 0;
            let mut last_signal_log = 0i64;
            let mut total_decisions = 0u64;
            let mut maker_count = 0u64;
            let mut taker_count = 0u64;
            let mut hold_count = 0u64;

            loop {
                interval.tick().await;

                let s = state_strat.read().await;

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
                    let s = state_strat.read().await;
                    info!(
                        "SIGNAL: score={:.3} mode={:?} | BN=${:.2} CL=${:.2} lag={:.1}bps ret2s={:.1}bps | OBI={:.3} | UP ask={} DN ask={} | hist={}",
                        signal.score,
                        signal.mode,
                        s.binance_rtds_price,
                        s.chainlink_price,
                        signal.basis_bps,
                        binance_ret.return_2s,
                        obi.as_ref().map(|o| o.value).unwrap_or(0.0),
                        s.up_best_ask.map(|a| format!("{:.0}c", a * 100.0)).unwrap_or("?".into()),
                        s.dn_best_ask.map(|a| format!("{:.0}c", a * 100.0)).unwrap_or("?".into()),
                        s.binance_mid_history.len(),
                    );
                    last_signal_log = now;
                    drop(s);
                }

                // Evaluate scalp strategy
                let actions = evaluate_scalp(
                    &position, &signal, &market_state, &risk, &config_strat, last_scalp_exit_us,
                );

                // Execute actions
                total_decisions += 1;
                for action in &actions {
                    match action {
                        StrategyAction::TakerEntry { token_id, spend_usdc, side_label, reason } => {
                            // CRITICAL: Only enter if we're actually FLAT
                            if position.state != PositionState::Flat {
                                continue;
                            }
                            taker_count += 1;
                            // Transition to Entering BEFORE the async call to prevent re-entry
                            position.state = PositionState::Entering;
                            position.token_id = token_id.clone();
                            position.side_label = side_label.clone();
                            position.entry_time_us = chrono::Utc::now().timestamp_micros();

                            let ask = if *side_label == "UP" {
                                market_state.up_best_ask.unwrap_or(0.5)
                            } else {
                                market_state.dn_best_ask.unwrap_or(0.5)
                            };
                            let shares_est = spend_usdc / ask;
                            info!(">>> ENTRY FOK: {} ${:.2} ~{:.0}sh @ {:.0}c | {}", side_label, spend_usdc, shares_est, ask*100.0, reason);

                            let result = exec_strat.taker_fok_buy(token_id, *spend_usdc).await;
                            match &result {
                                Ok(ref r) if r.success => {
                                    position.state = PositionState::Long;
                                    position.entry_price = ask;
                                    position.entry_order_id = Some(r.order_id.clone());

                                    // Check on-chain balance to know EXACT shares we hold
                                    let wallet = exec_strat.wallet_address().unwrap_or_default();
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await; // wait for chain
                                    match exec_strat.check_token_balance(&wallet, token_id).await {
                                        Ok(actual_shares) if actual_shares > 0.0 => {
                                            position.shares = actual_shares;
                                            info!(">>> BALANCE CHECK: {:.1}sh on-chain (estimated {:.0}sh)", actual_shares, shares_est);
                                        }
                                        Ok(_) => {
                                            // Balance is 0 — might not have settled yet, use estimate
                                            position.shares = shares_est;
                                            warn!(">>> BALANCE CHECK: 0 on-chain, using estimate {:.0}sh", shares_est);
                                        }
                                        Err(e) => {
                                            position.shares = shares_est;
                                            warn!(">>> BALANCE CHECK FAILED: {}, using estimate {:.0}sh", e, shares_est);
                                        }
                                    }

                                    let target_sell = ask * 1.10; // 10% target
                                    info!(
                                        ">>> FILLED! LONG {} {:.1}sh @ {:.0}c | target sell @ {:.0}c (+10%) | order={}",
                                        side_label, position.shares, ask * 100.0, target_sell * 100.0, r.order_id
                                    );
                                }
                                Ok(ref r) => {
                                    warn!(">>> ENTRY REJECTED: status={} err={:?} | back to FLAT", r.status, r.error);
                                    position = Position::new_flat();
                                }
                                Err(ref e) => {
                                    warn!(">>> ENTRY ERROR: {} | back to FLAT", e);
                                    position = Position::new_flat();
                                }
                            }
                            risk.order_count_this_window += 1;
                        }
                        StrategyAction::MakerExit { token_id, shares, price, reason } => {
                            maker_count += 1;
                            info!(">>> EXIT GTC SELL: {:.1}sh @ {:.0}c | {}", shares, price * 100.0, reason);
                            let result = exec_strat.maker_gtc_sell(token_id, *shares, *price, true).await;
                            if let Ok(ref r) = result {
                                position.state = PositionState::Exiting;
                                position.exit_order_id = Some(r.order_id.clone());
                                position.exit_posted_at_us = chrono::Utc::now().timestamp_micros();
                            }
                            risk.order_count_this_window += 1;
                        }
                        StrategyAction::TakerExit { token_id, shares, reason } => {
                            taker_count += 1;
                            let min_sell_price = position.entry_price;
                            info!(">>> EXIT FOK SELL: {:.0}sh floor={:.0}c | {}", shares, min_sell_price*100.0, reason);
                            let sell_result = exec_strat.taker_fok_sell(token_id, *shares, min_sell_price).await;

                            let sold = match &sell_result {
                                Ok(r) if r.success => {
                                    info!(">>> SELL FILLED: order={}", r.order_id);
                                    true
                                }
                                Ok(r) => {
                                    warn!(">>> SELL REJECTED: status={} err={:?} — STAYING LONG, will retry", r.status, r.error);
                                    false
                                }
                                Err(e) => {
                                    warn!(">>> SELL ERROR: {} — STAYING LONG, will retry", e);
                                    false
                                }
                            };

                            if sold {
                                let pnl_est = if let Some(bid) = if position.side_label == "UP" {
                                    market_state.up_best_bid
                                } else {
                                    market_state.dn_best_bid
                                } {
                                    (bid - position.entry_price) * position.shares
                                } else {
                                    0.0
                                };
                                info!(
                                    ">>> POSITION: FLAT (was {} {:.0}sh, est PnL ${:.2}, hold {}ms)",
                                    position.side_label, position.shares, pnl_est, position.hold_time_ms()
                                );
                                position = Position::new_flat();
                                last_scalp_exit_us = chrono::Utc::now().timestamp_micros();
                                risk.scalps_this_window += 1;
                            }
                            // If sell failed, position stays LONG — strategy will retry exit next cycle
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
