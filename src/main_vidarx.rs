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
    info!("VIDARX v3: single bid per side, exp@bid / cheap@bid-5c, repair at T+95s targeting 97c pair cost");

    // Per-window state
    let mut last_window_ts: i64 = 0;
    let mut pair_done = false;

    // Per-window inventory
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    let mut fills_this_window: u32 = 0;
    let max_shares_per_side: f64 = 200.0;
    let match_tolerance = 0.05; // within 5% = matched

    // OBI — stored for spike cancel only
    let mut latest_obi: f64 = 0.0;
    let mut window_skip = false;

    // Timing: random 500–2500ms cancel / random 500–2500ms repost
    let mut cancel_delay_ms: i64 = 2000;
    let mut next_post_interval_ms: i64 = 0;
    let mut last_cancel_ts: i64 = 0;
    let mut last_post_ts: i64 = 0;
    let mut orders_live = false;

    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        // ═══ WINDOW CHANGE DETECTION ═══
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                // Log previous window
                if last_window_ts > 0 && (up_shares > 0.0 || dn_shares > 0.0) {
                    let matched = up_shares.min(dn_shares);
                    let pair_cost = if matched > 0.0 {
                        (up_cost / up_shares.max(0.001)) + (dn_cost / dn_shares.max(0.001))
                    } else { 0.0 };
                    info!(">>> WINDOW COMPLETE: UP {:.0}sh (${:.2}) DN {:.0}sh (${:.2}) | matched={:.0} pcost={:.0}c | fills={}",
                        up_shares, up_cost, dn_shares, dn_cost,
                        matched, pair_cost * 100.0, fills_this_window);
                }

                // Reset
                up_shares = 0.0; dn_shares = 0.0;
                up_cost = 0.0; dn_cost = 0.0;
                fills_this_window = 0;
                pair_done = false;
                window_skip = false;
                orders_live = false;
                last_post_ts = 0;
                last_cancel_ts = 0;
                next_post_interval_ms = rand::thread_rng().gen_range(500..=2500);
                last_window_ts = market.window_start_ts;

                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                info!(">>> NEW WINDOW {} — PLAYING max {:.0}sh/side", market.window_start_ts, max_shares_per_side);
            }
        }

        tokio::select! {
            // Store latest OBI for adverse selection gating
            Some(sig) = signal_rx.recv() => {
                latest_obi = sig.obi_ema;
            }

            // Process executor events — confirm matching GTC was accepted
            Some(_evt) = exec_evt_rx.recv() => {
                // executor events — no action needed
            }

            // Track fills from user WS
            Some(fill) = fill_rx.recv() => {
                let market = market_rx.borrow().clone();
                let is_up = fill.asset_id == market.up_token_id;
                let is_dn = fill.asset_id == market.down_token_id;
                if !is_up && !is_dn { continue; }
                if fill.side != "BUY" { continue; }

                fills_this_window += 1;
                if is_up {
                    up_shares += fill.size;
                    up_cost += fill.size * fill.price;
                } else {
                    dn_shares += fill.size;
                    dn_cost += fill.size * fill.price;
                }

                let side_label = if is_up { "UP" } else { "DN" };
                let matched = up_shares.min(dn_shares);
                let pair_cost = if matched > 0.0 {
                    (up_cost / up_shares.max(0.001)) + (dn_cost / dn_shares.max(0.001))
                } else { 0.0 };

                info!(">>> FILL: {} {:.1}sh @ {:.2} | UP={:.0} DN={:.0} matched={:.0} pcost={:.0}c",
                    side_label, fill.size, fill.price, up_shares, dn_shares, matched, pair_cost * 100.0);

                // Markout logging: spawn task to capture mid at +250ms / +500ms / +1s
                {
                    let fill_price_snap = fill.price;
                    let fill_side_str: &'static str = if is_up { "UP" } else { "DN" };
                    let obi_at_fill = latest_obi;
                    let pm_rx = pm_top_rx.clone();
                    tokio::spawn(async move {
                        for (delay_ms, label) in &[(250u64, "250ms"), (500u64, "500ms"), (1000u64, "1s")] {
                            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
                            let top = pm_rx.borrow().clone();
                            let (bid_now, ask_now) = if fill_side_str == "UP" {
                                (top.up_bid.unwrap_or(0.0), top.up_ask.unwrap_or(0.0))
                            } else {
                                (top.down_bid.unwrap_or(0.0), top.down_ask.unwrap_or(0.0))
                            };
                            let mid = (bid_now + ask_now) / 2.0;
                            let markout = mid - fill_price_snap;
                            info!(">>> MARKOUT@{}: {} fill={:.0}c mid={:.0}c Δ={:+.1}c obi@fill={:.2}",
                                label, fill_side_str,
                                fill_price_snap * 100.0, mid * 100.0,
                                markout * 100.0, obi_at_fill);
                        }
                    });
                }

                // FIX #2: Only check match AFTER one side is full (>= 20sh)
                // Don't stop early when both sides are half-full
                if !pair_done && (up_shares >= max_shares_per_side || dn_shares >= max_shares_per_side) {
                    let bigger = up_shares.max(dn_shares);
                    let smaller = up_shares.min(dn_shares);
                    if smaller >= bigger * (1.0 - match_tolerance) {
                        info!(">>> MATCHED! UP={:.0} DN={:.0} — one side full + within 10%, holding to resolution",
                            up_shares, dn_shares);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "matched_pair_done".into(),
                        }).await;
                    }
                }
            }

            // Heartbeat
            _ = heartbeat_interval.tick() => {
                let top = pm_top_rx.borrow().clone();
                let pair_cost = if up_shares >= 1.0 && dn_shares >= 1.0 {
                    (up_cost / up_shares) + (dn_cost / dn_shares)
                } else { 0.0 };
                info!("HEARTBEAT: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh | pcost={:.0}c | pair_done={} | fills={}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, pair_cost * 100.0, pair_done, fills_this_window);
            }

            // ═══ MAIN STRATEGY LOOP — every 100ms ═══
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if pair_done { continue; }

                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();
                if market.up_token_id.is_empty() { continue; }

                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                // Wait for T+35s
                if elapsed_s < 35 { continue; }

                // Stop at T+240s
                if elapsed_s >= 240 {
                    if !pair_done {
                        info!(">>> T+240s — window over, holding to resolution");
                        pair_done = true;
                    }
                    continue;
                }

                // OBI spike cancel
                if orders_live && latest_obi.abs() > 0.55 {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "obi_spike".into(),
                    }).await;
                    orders_live = false;
                    last_cancel_ts = now_ms;
                    next_post_interval_ms = rand::thread_rng().gen_range(500..=2500);
                    info!(">>> OBI SPIKE CANCEL: obi={:.3}", latest_obi);
                }

                // Random cancel
                if orders_live && (now_ms - last_post_ts) >= cancel_delay_ms {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "flash_cancel".into(),
                    }).await;
                    orders_live = false;
                    last_cancel_ts = now_ms;
                    next_post_interval_ms = rand::thread_rng().gen_range(500..=2500);
                }

                // Get prices
                let top = pm_top_rx.borrow().clone();
                let (up_bid, up_ask) = match (top.up_bid, top.up_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue,
                };
                let (dn_bid, dn_ask) = match (top.down_bid, top.down_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue,
                };

                // Expensive side = higher ask; cheap side = lower ask
                let exp_is_up  = up_ask >= dn_ask;
                let exp_bid_v  = if exp_is_up { up_bid } else { dn_bid };
                // Phase 1 prices: exp @ bid, cheap @ 0.97 - exp_bid → pair always sums to 97c
                let up_price_p1 = if exp_is_up { exp_bid_v } else { 0.97 - exp_bid_v };
                let dn_price_p1 = if exp_is_up { 0.97 - exp_bid_v } else { exp_bid_v };

                // ═══ T+95s+: REPAIR — post underweight side @ 0.97 - overweight_avg_cost ═══
                if elapsed_s >= 95 {
                    let full_v = up_shares.max(dn_shares);
                    let weak_v = up_shares.min(dn_shares);

                    // Matched? Hold to window close.
                    if full_v > 0.0 && weak_v >= full_v * (1.0 - match_tolerance) {
                        if !pair_done {
                            info!(">>> REPAIRED: full={:.0} weak={:.0} ({:.0}%) — holding to close",
                                full_v, weak_v, weak_v / full_v * 100.0);
                            pair_done = true;
                            let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                                reason: "repaired".into(),
                            }).await;
                        }
                        continue;
                    }

                    // Determine underweight side
                    let under_is_up = up_shares <= dn_shares;
                    let (under_ask, under_shares, over_cost_v, over_shares_v) =
                        if under_is_up {
                            (up_ask, up_shares, dn_cost, dn_shares)
                        } else {
                            (dn_ask, dn_shares, up_cost, up_shares)
                        };

                    let still_need = (max_shares_per_side - under_shares).max(0.0);
                    if still_need < 5.0 { continue; }

                    // Price = 0.97 - overweight_avg_cost (actual cost of the filled opposite side)
                    // Falls back to 1.0 cap if overweight side is empty (no fills yet)
                    let price = if over_shares_v >= 1.0 {
                        ((0.97 - over_cost_v / over_shares_v) * 100.0).round() / 100.0
                    } else {
                        // No fills yet on either side — use market formula as fallback
                        let under_is_exp = under_is_up == exp_is_up;
                        ((if under_is_exp { exp_bid_v } else { 0.97 - exp_bid_v }) * 100.0).round() / 100.0
                    };

                    if !orders_live && (now_ms - last_cancel_ts) >= next_post_interval_ms {
                        let size     = 24.0_f64.min(still_need);
                        let token_id = if under_is_up { market.up_token_id.clone() } else { market.down_token_id.clone() };
                        let side_v   = if under_is_up { Side::Up } else { Side::Down };
                        let label    = if under_is_up { "UP" } else { "DN" };
                        let over_avg = if over_shares_v >= 1.0 { over_cost_v / over_shares_v } else { 0.0 };

                        if size >= 5.0 && price > 0.01 && price < under_ask {
                            let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                                market_slug: market.slug.clone(),
                                token_id,
                                side: side_v,
                                price,
                                size,
                                post_only: true,
                                ladder_key: format!("repair_{}", label.to_lowercase()),
                                was_cheap: under_is_up != exp_is_up,
                            }).await;
                            orders_live = true;
                            last_post_ts = now_ms;
                            cancel_delay_ms = rand::thread_rng().gen_range(500..=2500);
                            info!(">>> REPAIR [{}]: {:.0}sh @ {:.0}c | over_avg={:.0}c | sum={:.0}c | full={:.0} weak={:.0} cancel={}ms",
                                label, size, price * 100.0, over_avg * 100.0,
                                (price + over_avg) * 100.0, full_v, weak_v, cancel_delay_ms);
                        }
                    }
                    continue; // skip phase1 posting
                }

                // ═══ T+35s to T+95s: PHASE 1 — single bid per side, pair sums to 97c ═══
                // Exp @ exp_bid, cheap @ 0.97 - exp_bid
                if !window_skip && !orders_live && (now_ms - last_cancel_ts) >= next_post_interval_ms {
                    let up_needs = (max_shares_per_side - up_shares).max(0.0);
                    let dn_needs = (max_shares_per_side - dn_shares).max(0.0);
                    let up_price = (up_price_p1 * 100.0).round() / 100.0;
                    let dn_price = (dn_price_p1 * 100.0).round() / 100.0;
                    let mut posted_any = false;

                    // UP side
                    if up_needs >= 5.0 && up_price > 0.01 && up_price < up_ask {
                        let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                            market_slug: market.slug.clone(),
                            token_id: market.up_token_id.clone(),
                            side: Side::Up,
                            price: up_price,
                            size: 24.0_f64.min(up_needs),
                            post_only: true,
                            ladder_key: "up:0".into(),
                            was_cheap: !exp_is_up,
                        }).await;
                        posted_any = true;
                    }

                    // DN side
                    if dn_needs >= 5.0 && dn_price > 0.01 && dn_price < dn_ask {
                        let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                            market_slug: market.slug.clone(),
                            token_id: market.down_token_id.clone(),
                            side: Side::Down,
                            price: dn_price,
                            size: 24.0_f64.min(dn_needs),
                            post_only: true,
                            ladder_key: "dn:0".into(),
                            was_cheap: exp_is_up,
                        }).await;
                        posted_any = true;
                    }

                    if posted_any {
                        orders_live = true;
                        last_post_ts = now_ms;
                        cancel_delay_ms = rand::thread_rng().gen_range(500..=2500);
                        info!(">>> POST P1: UP@{:.0}c[{}] DN@{:.0}c[{}] sum={:.0}c | UP={:.0}sh DN={:.0}sh cancel={}ms",
                            up_price * 100.0, if exp_is_up { "EXP" } else { "CHEAP" },
                            dn_price * 100.0, if exp_is_up { "CHEAP" } else { "EXP" },
                            (up_price + dn_price) * 100.0,
                            up_shares, dn_shares, cancel_delay_ms);
                    }
                }
            }
        }
    }
}
