//! BeeBoop BTC Mint+Sell — Mint inventory, sell on signal.
//!
//! Strategy:
//! 1. At window start: mint 100 UP + 100 DN tokens via splitPosition ($100)
//! 2. Signal fires (BTC moving UP) → sell 20 DN (about to drop), wait 2s, sell 20 UP (now higher)
//! 3. Combined sell > $1 per pair = profit
//! 4. 5 snipes per window to exhaust 100 token supply
//! 5. Sweeper redeems any leftover tokens

// Keep old modules for backward compatibility
mod binance_l2;
mod execution;
mod leadlag_lab;
mod market_ws;
mod rtds;
mod sim;
mod strategy;
mod telemetry;
mod types;
mod user_ws;

// v2 modules
mod config_v2;
mod state;
mod types_v2;
mod feeds;
mod signals;
mod strategy_v2;
mod execution_v2;
mod portfolio;
mod market;
mod logging;
mod risk;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn, error};
use std::process::Command;

use config_v2::Config;
use state::SharedState;
use types_v2::*;

const MINT_AMOUNT_USDC: f64 = 100.0;
const SHARES_PER_SNIPE: f64 = 20.0;
const MAX_SNIPES_PER_WINDOW: u32 = 5;
const SELL_COOLDOWN_MS: i64 = 1500;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .init();

    dotenvy::dotenv().ok();

    let args: Vec<String> = std::env::args().collect();
    let mode = match args.get(1).map(|s| s.as_str()) {
        Some("live") => BotMode::Live,
        _ => BotMode::DryRun,
    };

    let mut config = Config::default();
    config.mode = mode;
    config.market = "btc".to_string();
    config.load_credentials();

    // Override for mint+sell strategy
    config.cooldown_ms = SELL_COOLDOWN_MS;
    config.cancel_at_s = 180; // stop at T+180s

    info!("=== BeeBoop BTC MINT+SELL v1.0 ===");
    info!("Mode: {:?}", config.mode);
    info!("Strategy: Mint ${:.0} → sell {:.0}sh × {} snipes/window",
        MINT_AMOUNT_USDC, SHARES_PER_SNIPE, MAX_SNIPES_PER_WINDOW);
    info!("Signal: trigger={:.2}bps, score_min={:.2}",
        config.trigger_min_fast_move_bps, config.maker_score_min);

    if config.mode == BotMode::Live && !config.has_credentials() {
        error!("Live mode requires POLY_PRIVATE_KEY, POLY_API_KEY, POLY_SECRET, POLY_PASSPHRASE in .env");
        std::process::exit(1);
    }

    let shared = SharedState::new();

    // Channels
    let (binance_tx, _) = broadcast::channel::<BinanceTick>(10_000);
    let (chainlink_tx, _) = broadcast::channel::<ChainlinkTick>(10_000);
    let (_pm_tx, _) = broadcast::channel::<PolymarketTop>(10_000);

    let initial_market = MarketDescriptor::default();
    let (market_watch_tx, market_watch_rx) = watch::channel(initial_market);
    let (pm_top_watch_tx, pm_top_watch_rx) = watch::channel(PolymarketTop::default());
    let (chainlink_watch_tx, chainlink_watch_rx) = watch::channel::<Option<ChainlinkTick>>(None);

    let (signal_tx, mut signal_rx) = mpsc::channel::<Signal>(1_000);
    let (log_tx, log_rx) = mpsc::channel::<LogEvent>(10_000);

    // 1. Market discovery
    tokio::spawn(market::discovery::run_market_discovery_task(
        config.clone(), market_watch_tx.clone(), log_tx.clone(),
    ));

    // 2. Binance feed (BTC default)
    tokio::spawn(feeds::binance::run_binance_feed_task(
        binance_tx.clone(), log_tx.clone(),
    ));

    // 3. Polymarket price feed (REST polling)
    tokio::spawn(feeds::polymarket_ws::run_polymarket_ws_task(
        market_watch_rx.clone(), _pm_tx.clone(), pm_top_watch_tx.clone(), log_tx.clone(),
    ));

    // 4. RTDS Chainlink (BTC default)
    tokio::spawn(feeds::rtds::run_rtds_task(
        chainlink_tx.clone(), chainlink_watch_tx.clone(), log_tx.clone(),
    ));

    // 5. Signal engine
    tokio::spawn(signals::signal_engine::run_signal_engine_task(
        config.clone(), shared.clone(),
        binance_tx.subscribe(), market_watch_rx.clone(),
        pm_top_watch_rx.clone(), chainlink_watch_rx.clone(),
        signal_tx, log_tx.clone(),
    ));

    // 6. Logger
    tokio::spawn(logging::trade_log::run_trade_log_task(config.clone(), log_rx));

    // 7. Executor — authenticate once
    let exec = execution_v2::executor::ExecutorHandle::new(&config).await;

    info!("All tasks started — entering mint+sell loop");

    // ─── Main mint+sell loop ────────────────────────────────────────────────
    let mut last_window_ts: i64 = 0;
    let mut snipes_this_window: u32 = 0;
    let mut inventory_up: f64 = 0.0;
    let mut inventory_dn: f64 = 0.0;
    let mut window_locked_side: Option<Side> = None;
    let mut minted_this_window = false;
    let mut last_sell_ts: i64 = 0;

    loop {
        // Check for new window
        let market = market_watch_rx.borrow().clone();
        if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
            if last_window_ts > 0 {
                info!(">>> NEW WINDOW {} — leftover inventory: UP={:.0} DN={:.0}",
                    market.window_start_ts, inventory_up, inventory_dn);
            }
            last_window_ts = market.window_start_ts;
            snipes_this_window = 0;
            inventory_up = 0.0;
            inventory_dn = 0.0;
            window_locked_side = None;
            minted_this_window = false;
            shared.set_state(StrategyState::Idle);
        }

        // Mint tokens at window start (once per window)
        if !minted_this_window && market.window_start_ts > 0
            && !market.up_token_id.is_empty()
        {
            let condition_id = market.condition_id.clone();
            if !condition_id.is_empty() {
                info!(">>> MINTING ${:.0} for window {} | conditionId={}...",
                    MINT_AMOUNT_USDC, market.window_start_ts, &condition_id[..16.min(condition_id.len())]);

                if config.mode == BotMode::Live {
                    // Call Python minter
                    let result = tokio::task::spawn_blocking(move || {
                        Command::new("python3")
                            .arg("mint_window.py")
                            .arg(&condition_id)
                            .arg(format!("{:.0}", MINT_AMOUNT_USDC))
                            .output()
                    }).await;

                    match result {
                        Ok(Ok(output)) => {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                                if json.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                                    let elapsed = json.get("elapsed_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                                    info!(">>> MINT SUCCESS: ${:.0} in {}ms | UP={:.0} DN={:.0}",
                                        MINT_AMOUNT_USDC, elapsed, MINT_AMOUNT_USDC, MINT_AMOUNT_USDC);
                                    inventory_up = MINT_AMOUNT_USDC;
                                    inventory_dn = MINT_AMOUNT_USDC;
                                    minted_this_window = true;
                                } else {
                                    let err = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                                    error!(">>> MINT FAILED: {}", err);
                                }
                            } else {
                                error!(">>> MINT: bad JSON response: {}", stdout);
                            }
                        }
                        Ok(Err(e)) => error!(">>> MINT: process error: {}", e),
                        Err(e) => error!(">>> MINT: spawn error: {}", e),
                    }
                } else {
                    info!(">>> DRY RUN — would mint ${:.0}", MINT_AMOUNT_USDC);
                    inventory_up = MINT_AMOUNT_USDC;
                    inventory_dn = MINT_AMOUNT_USDC;
                    minted_this_window = true;
                }
            }
        }

        // Wait for signals
        let signal = tokio::select! {
            s = signal_rx.recv() => match s {
                Some(s) => s,
                None => break,
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => continue,
        };

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Guards
        if !minted_this_window { continue; }
        if snipes_this_window >= MAX_SNIPES_PER_WINDOW { continue; }
        if inventory_up < SHARES_PER_SNIPE || inventory_dn < SHARES_PER_SNIPE { continue; }

        // Cooldown
        if now_ms - last_sell_ts < SELL_COOLDOWN_MS { continue; }

        // Time check — stop at T+180s
        let elapsed_s = now_ms / 1000 - market.window_start_ts;
        if elapsed_s > config.cancel_at_s as i64 { continue; }

        // Direction lock
        if let Some(locked) = window_locked_side {
            if signal.side != locked { continue; }
        }

        // Signal says UP → sell DN first (about to drop), then sell UP (now higher)
        // Signal says DN → sell UP first (about to drop), then sell DN (now higher)
        let (first_sell_token, first_sell_side, second_sell_token, second_sell_side) = match signal.side {
            Side::Up => {
                // BTC going UP → DN drops, UP rises
                // Sell DN first (capture value before it drops), then sell UP (at higher price)
                (&market.down_token_id, "DN", &market.up_token_id, "UP")
            }
            Side::Down => {
                // BTC going DN → UP drops, DN rises
                // Sell UP first (capture value before it drops), then sell DN (at higher price)
                (&market.up_token_id, "UP", &market.down_token_id, "DN")
            }
        };

        // Lock direction on first signal
        if window_locked_side.is_none() {
            info!(">>> DIRECTION LOCKED: {:?} for this window", signal.side);
            window_locked_side = Some(signal.side);
        }

        let side_label = if signal.side == Side::Up { "UP" } else { "DN" };
        info!(">>> SNIPE {}/{}: signal={} score={:.3} | sell {} then {}",
            snipes_this_window + 1, MAX_SNIPES_PER_WINDOW,
            side_label, signal.score, first_sell_side, second_sell_side);

        // ── LEG 1: Sell the side about to DROP ──
        if let Some(ref handle) = exec {
            let result = handle.sell_fok(first_sell_token, SHARES_PER_SNIPE).await;
            match result {
                Ok(fill) => {
                    info!(">>> LEG 1 SOLD: {} {:.0}sh @ {:.0}c | {}ms",
                        first_sell_side, fill.filled_size, fill.filled_price * 100.0,
                        fill.latency_ms);
                }
                Err(e) => {
                    warn!(">>> LEG 1 SELL FAILED: {} — skipping this snipe", e);
                    continue;
                }
            }
        }

        // ── Wait 2s for repricing ──
        info!(">>> Waiting 2s for repricing...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // ── LEG 2: Sell the side that ROSE ──
        if let Some(ref handle) = exec {
            let result = handle.sell_fok(second_sell_token, SHARES_PER_SNIPE).await;
            match result {
                Ok(fill) => {
                    info!(">>> LEG 2 SOLD: {} {:.0}sh @ {:.0}c | {}ms",
                        second_sell_side, fill.filled_size, fill.filled_price * 100.0,
                        fill.latency_ms);
                }
                Err(e) => {
                    warn!(">>> LEG 2 SELL FAILED: {}", e);
                }
            }
        }

        // Update inventory
        inventory_up -= SHARES_PER_SNIPE;
        inventory_dn -= SHARES_PER_SNIPE;
        snipes_this_window += 1;
        last_sell_ts = chrono::Utc::now().timestamp_millis();

        info!(">>> SNIPE {}/{} COMPLETE | inventory: UP={:.0} DN={:.0}",
            snipes_this_window, MAX_SNIPES_PER_WINDOW, inventory_up, inventory_dn);

        if snipes_this_window >= MAX_SNIPES_PER_WINDOW {
            info!(">>> ALL SNIPES DONE — waiting for next window");
        }
    }

    Ok(())
}
