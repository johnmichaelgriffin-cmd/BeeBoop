//! BeeBoop Vidarx — Buy-only, maker-only, pair-cost-minimizing accumulator.
//!
//! Strategy: post GTC postOnly bids on BOTH sides, skewed by signal.
//! ~55% cheap / 45% expensive target fill ratio.
//! Hold everything to resolution. Never sell intrawindow.
//!
//! Usage: beeboop-vidarx live

mod config_v2;
mod types_v2;
mod state;
mod feeds;
mod signals;
mod execution_v2;
mod strategy_v2;
mod market;
mod portfolio;
mod risk;
mod logging;

use anyhow::Result;
use tracing::{info, warn, error};
use tokio::sync::{broadcast, mpsc, watch};
use rand::Rng;

use config_v2::Config;
use state::SharedState;
use types_v2::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Install crypto provider for TLS
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("crypto provider");

    tracing_subscriber::fmt()
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .init();

    let mode = std::env::args().nth(1).unwrap_or("dryrun".into());
    let mut config = Config::default();
    config.market = "btc".to_string();

    // Vidarx-specific config overrides
    config.start_delay_s = 5;           // start quoting at T+5s
    config.cancel_at_s = 240;           // stop quoting at T+240s
    config.cooldown_ms = 500;           // fast cooldown between quote refreshes
    config.max_pairs_per_window = 999;  // no limit — accumulate as much as possible
    config.share_base = 20.0;           // ladder sizing base

    if mode == "live" {
        config.mode = BotMode::Live;
        dotenvy::dotenv().ok();
        config.load_credentials();
    }

    info!("=== BEEBOOP VIDARX — BUY-ONLY MAKER BTC 5M ===");
    info!("Mode: {:?}", config.mode);
    info!("Strategy: postOnly GTC bids on BOTH sides, signal-skewed");
    info!("Target mix: 55% cheap / 45% expensive");
    info!("Ladder: cheap 20/15/10sh, exp 15/12/8sh (3 levels each)");
    info!("Timing: T+5s start, T+180s reduce, T+240s stop");
    info!("No selling. Hold to resolution.");

    let shared = SharedState::new();

    // Channels
    let (binance_tx, _) = broadcast::channel::<BinanceTick>(10_000);
    let (polymarket_tx, _) = broadcast::channel::<PolymarketTop>(10_000);
    let (chainlink_tx, _) = broadcast::channel::<ChainlinkTick>(10_000);
    let (market_watch_tx, market_watch_rx) = watch::channel::<MarketDescriptor>(MarketDescriptor::default());
    let (pm_top_watch_tx, pm_top_watch_rx) = watch::channel::<PolymarketTop>(PolymarketTop::default());
    let (chainlink_watch_tx, chainlink_watch_rx) = watch::channel::<Option<ChainlinkTick>>(None);
    let (signal_tx, signal_rx) = mpsc::channel::<Signal>(1_000);
    let (exec_cmd_tx, exec_cmd_rx) = mpsc::channel::<ExecutionCommand>(1_000);
    let (exec_evt_tx, exec_evt_rx) = mpsc::channel::<ExecutionEvent>(1_000);
    let (log_tx, log_rx) = mpsc::channel::<LogEvent>(10_000);

    // 1. Market discovery
    tokio::spawn(market::discovery::run_market_discovery_task(
        config.clone(),
        market_watch_tx,
        log_tx.clone(),
    ));

    // 2. Binance feed
    tokio::spawn(feeds::binance::run_binance_feed_task(
        binance_tx.clone(),
        log_tx.clone(),
    ));

    // 3. Polymarket WebSocket — ride or die, no REST fallback
    tokio::spawn(feeds::market_ws::run_market_ws_task(
        market_watch_rx.clone(),
        polymarket_tx,
        pm_top_watch_tx.clone(),
        log_tx.clone(),
    ));

    // 4. RTDS / Chainlink
    tokio::spawn(feeds::rtds::run_rtds_task(
        chainlink_tx,
        chainlink_watch_tx,
        log_tx.clone(),
    ));

    // 4b. User WebSocket — authenticated feed for fill tracking
    let (fill_tx, fill_rx) = mpsc::channel::<feeds::user_ws::UserFillEvent>(1_000);
    if config.has_credentials() {
        let api_key = config.poly_api_key.clone();
        let api_secret = config.poly_api_secret.clone();
        let api_passphrase = config.poly_api_passphrase.clone();
        tokio::spawn(feeds::user_ws::run_user_ws_task(
            api_key, api_secret, api_passphrase,
            market_watch_rx.clone(),
            fill_tx,
            600,
        ));
    } else {
        info!("No credentials — user WS disabled (fills won't be tracked)");
    }

    // 5. Signal engine (same as before — computes score from Binance/Chainlink/OBI)
    tokio::spawn(signals::signal_engine::run_signal_engine_task(
        config.clone(),
        shared.clone(),
        binance_tx.subscribe(),
        market_watch_rx.clone(),
        pm_top_watch_rx.clone(),
        chainlink_watch_rx,
        signal_tx,
        log_tx.clone(),
    ));

    // 6. Executor — handles GTC postOnly bids + cancel_all
    tokio::spawn(execution_v2::executor::run_executor_task(
        config.clone(),
        exec_cmd_rx,
        exec_evt_tx,
        log_tx.clone(),
    ));

    // 7. Vidarx strategy — the new brain (with user WS fill tracking)
    tokio::spawn(run_vidarx_strategy(
        config.clone(),
        shared.clone(),
        market_watch_rx.clone(),
        pm_top_watch_rx,
        signal_rx,
        exec_cmd_tx,
        exec_evt_rx,
        fill_rx,
        log_tx.clone(),
    ));

    // 8. Logger
    tokio::spawn(logging::trade_log::run_trade_log_task(
        config.clone(),
        log_rx,
    ));

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");
    Ok(())
}

/// Repair pair cost target: completion-first schedule.
/// Mild imbalance: stay picky. Severe imbalance: pay to finish, prevent naked loss.
fn repair_pair_cost_target(match_ratio: f64) -> f64 {
    if match_ratio >= 0.85 { 0.96 }
    else if match_ratio >= 0.70 { 0.97 }
    else if match_ratio >= 0.50 { 0.98 }
    else { 0.99 }
}

/// FLASH LADDER Strategy — post both-side ladders, cancel after random 500–2000ms, repeat.
///
/// Phase 1 (both sides, !phase2_mode):
///   Normal:  offset=3 on both sides (bid-3c/-4c/-5c)
///   Repair:  overweight side offset=5 (bid-5c/-6c/-7c), underweight offset=1 (bid-1c/-2c/-3c)
///   repair_mode enters at weak<60% of full, exits at weak>=95%
///
/// Phase 2 (underweight only, phase2_mode latch):
///   Triggered by: overweight side >= practical_full (74sh), OR T+180s with ratio<0.95
///   offset=1 (bid-1c/-2c/-3c) on underweight side until matched within 5%
async fn run_vidarx_strategy(
    config: Config,
    _shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut signal_rx: mpsc::Receiver<Signal>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    mut exec_evt_rx: mpsc::Receiver<ExecutionEvent>,
    mut fill_rx: mpsc::Receiver<feeds::user_ws::UserFillEvent>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    info!("VIDARX v3: GTC resting orders — 10 rounds × 5sh/side every 25s from T+10s, no cancel");

    // Per-window state
    let mut last_window_ts: i64 = 0;

    // Per-window inventory (fills tracked for logging)
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    let mut fills_this_window: u32 = 0;

    // Round tracking: 10 rounds × 5sh each, one every 25s from T+10s
    let total_rounds: u32 = 10;
    let round_size: f64 = 5.0;
    let mut rounds_posted: u32 = 0;

    let mut latest_obi: f64 = 0.0;
    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        // ═══ WINDOW CHANGE DETECTION ═══
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                if last_window_ts > 0 && (up_shares > 0.0 || dn_shares > 0.0) {
                    let matched = up_shares.min(dn_shares);
                    let pair_cost = if matched > 0.0 {
                        (up_cost / up_shares.max(0.001)) + (dn_cost / dn_shares.max(0.001))
                    } else { 0.0 };
                    info!(">>> WINDOW COMPLETE: UP {:.0}sh (${:.2}) DN {:.0}sh (${:.2}) | matched={:.0} pcost={:.0}c | fills={} | rounds={}",
                        up_shares, up_cost, dn_shares, dn_cost,
                        matched, pair_cost * 100.0, fills_this_window, rounds_posted);
                }

                // Reset
                up_shares = 0.0; dn_shares = 0.0;
                up_cost = 0.0; dn_cost = 0.0;
                fills_this_window = 0;
                rounds_posted = 0;
                last_window_ts = market.window_start_ts;

                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                info!(">>> NEW WINDOW {} — {} rounds × {:.0}sh/side every 25s from T+10s",
                    market.window_start_ts, total_rounds, round_size);
            }
        }

        tokio::select! {
            Some(sig) = signal_rx.recv() => {
                latest_obi = sig.obi_ema;
            }

            Some(_evt) = exec_evt_rx.recv() => {}

            // Track fills
            Some(fill) = fill_rx.recv() => {
                let market = market_rx.borrow().clone();
                let is_up = fill.asset_id == market.up_token_id;
                let is_dn = fill.asset_id == market.down_token_id;
                if !is_up && !is_dn { continue; }
                if fill.side != "BUY" { continue; }

                fills_this_window += 1;
                if is_up { up_shares += fill.size; up_cost += fill.size * fill.price; }
                else      { dn_shares += fill.size; dn_cost += fill.size * fill.price; }

                let matched = up_shares.min(dn_shares);
                let pair_cost = if matched > 0.0 {
                    (up_cost / up_shares.max(0.001)) + (dn_cost / dn_shares.max(0.001))
                } else { 0.0 };
                info!(">>> FILL: {} {:.1}sh @ {:.0}c | UP={:.0} DN={:.0} matched={:.0} pcost={:.0}c",
                    if is_up { "UP" } else { "DN" }, fill.size, fill.price * 100.0,
                    up_shares, dn_shares, matched, pair_cost * 100.0);
            }

            // Heartbeat
            _ = heartbeat_interval.tick() => {
                let top = pm_top_rx.borrow().clone();
                let pair_cost = if up_shares >= 1.0 && dn_shares >= 1.0 {
                    (up_cost / up_shares) + (dn_cost / dn_shares)
                } else { 0.0 };
                info!("HEARTBEAT: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh | pcost={:.0}c | rounds={}/{} | obi={:.2}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, pair_cost * 100.0,
                    rounds_posted, total_rounds, latest_obi);
            }

            // ═══ MAIN LOOP — every 100ms ═══
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();
                if market.up_token_id.is_empty() { continue; }

                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                // How many rounds should have fired by now?
                // Round N fires at T + 10 + N*25  (N=0..9)
                let rounds_due = if elapsed_s < 10 {
                    0u32
                } else {
                    (((elapsed_s - 10) / 25) + 1).min(total_rounds as i64) as u32
                };

                if rounds_posted >= rounds_due { continue; }

                // Get current prices
                let top = pm_top_rx.borrow().clone();
                let (up_bid, up_ask) = match (top.up_bid, top.up_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue,
                };
                let (dn_bid, dn_ask) = match (top.down_bid, top.down_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue,
                };

                // Exp = higher ask, cheap = lower ask
                let exp_is_up   = up_ask >= dn_ask;
                let exp_bid_v   = if exp_is_up { up_bid } else { dn_bid };
                let cheap_ask_v = if exp_is_up { dn_ask } else { up_ask };

                // Prices: exp @ bid, cheap @ 0.97 - exp_bid (capped below cheap ask)
                let exp_price   = (exp_bid_v * 100.0).round() / 100.0;
                let cheap_price = ((0.96 - exp_bid_v).max(0.01).min(cheap_ask_v - 0.01) * 100.0).round() / 100.0;

                let (up_price, dn_price) = if exp_is_up {
                    (exp_price, cheap_price)
                } else {
                    (cheap_price, exp_price)
                };

                let round_num = rounds_posted + 1;
                let mut posted_any = false;

                // UP side
                if up_price > 0.01 && up_price < up_ask {
                    let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                        market_slug: market.slug.clone(),
                        token_id: market.up_token_id.clone(),
                        side: Side::Up,
                        price: up_price,
                        size: round_size,
                        post_only: true,
                        ladder_key: format!("up:r{}", round_num),
                        was_cheap: !exp_is_up,
                    }).await;
                    posted_any = true;
                }

                // DN side
                if dn_price > 0.01 && dn_price < dn_ask {
                    let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                        market_slug: market.slug.clone(),
                        token_id: market.down_token_id.clone(),
                        side: Side::Down,
                        price: dn_price,
                        size: round_size,
                        post_only: true,
                        ladder_key: format!("dn:r{}", round_num),
                        was_cheap: exp_is_up,
                    }).await;
                    posted_any = true;
                }

                if posted_any {
                    rounds_posted += 1;
                    info!(">>> ROUND {}/{}: UP@{:.0}c[{}] DN@{:.0}c[{}] sum={:.0}c | T+{}s",
                        rounds_posted, total_rounds,
                        up_price * 100.0, if exp_is_up { "EXP" } else { "CHEAP" },
                        dn_price * 100.0, if exp_is_up { "CHEAP" } else { "EXP" },
                        (up_price + dn_price) * 100.0, elapsed_s);
                }
            }
        }
    }
}
