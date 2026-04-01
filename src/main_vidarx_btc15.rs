//! BeeBoop Vidarx BTC 15M — BUY-ONLY MAKER BTC 15M accumulator.
//!
//! Strategy: post GTC postOnly bids on BOTH sides, skewed by signal.
//! ~55% cheap / 45% expensive target fill ratio.
//! Hold everything to resolution. Never sell intrawindow.
//!
//! 15-minute variant: 200sh cap, T-60s start, T+840s stop, :14/:29/:44/:59 WS reconnect.
//!
//! Usage: beeboop-btc15 live

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
    config.market = "btc15".to_string();

    // Vidarx BTC 15M-specific config overrides
    config.start_delay_s = 5;           // start quoting at T+5s (strategy overrides with T-60s check)
    config.cancel_at_s = 840;           // stop quoting at T+840s (14 minutes)
    config.cooldown_ms = 500;           // fast cooldown between quote refreshes
    config.max_pairs_per_window = 999;  // no limit — accumulate as much as possible
    config.share_base = 20.0;           // ladder sizing base

    if mode == "live" {
        config.mode = BotMode::Live;
        dotenvy::dotenv().ok();
        config.load_credentials();
    }

    info!("=== BEEBOOP VIDARX — BUY-ONLY MAKER BTC 15M ===");
    info!("Mode: {:?}", config.mode);
    info!("Strategy: postOnly GTC bids on BOTH sides, signal-skewed");
    info!("Target mix: 55% cheap / 45% expensive");
    info!("Ladder: 8/6/6sh per side (3 levels each)");
    info!("Timing: T-60s start, T+840s stop (14 minutes)");
    info!("Max shares: 200/side (practical_full=196)");
    info!("WS reconnect: :14/:29/:44/:59 (900s interval)");
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

    // 4b. User WebSocket — authenticated feed for fill tracking (900s interval for 15m windows)
    let (fill_tx, fill_rx) = mpsc::channel::<feeds::user_ws::UserFillEvent>(1_000);
    if config.has_credentials() {
        let api_key = config.poly_api_key.clone();
        let api_secret = config.poly_api_secret.clone();
        let api_passphrase = config.poly_api_passphrase.clone();
        tokio::spawn(feeds::user_ws::run_user_ws_task(
            api_key, api_secret, api_passphrase,
            market_watch_rx.clone(),
            fill_tx,
            900,
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

    // 7. Vidarx BTC 15M strategy
    tokio::spawn(run_vidarx_btc15_strategy(
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
    if match_ratio >= 0.85 { 0.97 }
    else if match_ratio >= 0.70 { 0.99 }
    else if match_ratio >= 0.50 { 1.00 }
    else { 1.02 }
}

/// FLASH LADDER Strategy (BTC 15M) — post both-side ladders, cancel after random 500–2000ms, repeat.
///
/// Phase 1 (both sides, !phase2_mode):
///   Normal:  offset=3 on both sides (bid-3c/-4c/-5c)
///   Repair:  overweight side offset=5 (bid-5c/-6c/-7c), underweight offset=1 (bid-1c/-2c/-3c)
///   repair_mode enters at weak<60% of full, exits at weak>=95%
///
/// Phase 2 (underweight only, phase2_mode latch):
///   Triggered by: overweight side >= practical_full (198sh), OR T+630s with ratio<0.95
///   offset=1 (bid-1c/-2c/-3c) on underweight side until matched within 5%
///   Stop at T+840s
async fn run_vidarx_btc15_strategy(
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
    info!("BTC 15M LADDER: post both sides bid-3/-4/-5c, hold until market drops to bid+1c, max 200sh/side");

    // Per-window state
    let mut last_window_ts: i64 = 0;
    let mut pair_done = false;

    // Per-window inventory
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    let mut fills_this_window: u32 = 0;
    let mut matching_gtc_posted = false; // true once we've posted the single matching GTC
    let max_shares_per_side: f64 = 200.0;
    let match_tolerance = 0.05; // within 5% = considered matched (tighter completion target)
    let mut repair_mode = false; // reversible: enters at weak<60% of full, exits at weak>=95%
    let mut phase2_mode = false; // one-way latch: overweight side capped OR T+630s emergency
    let practical_full: f64 = 198.0; // triggers phase2_mode when either side reaches this

    // OBI — stored for spike cancel only (no directional skew in Phase 1)
    let mut latest_obi: f64 = 0.0;
    let mut window_skip = false;

    // Timing — Phase 1: random 500–2000ms cancel / random 500–2000ms repost
    //          Phase 2: 1000–1500ms cancel / random 500–2000ms repost
    let mut cancel_delay_ms: i64 = 2000;
    let mut next_post_interval_ms: i64 = 0;
    let mut last_cancel_ts: i64 = 0;
    let mut last_post_ts: i64 = 0;
    let mut orders_live = false;             // true only if at least one order was sent

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
                matching_gtc_posted = false;
                repair_mode = false;
                phase2_mode = false;
                window_skip = false;
                orders_live = false;
                last_post_ts = 0;
                last_cancel_ts = 0;
                next_post_interval_ms = rand::thread_rng().gen_range(500..=2000);
                last_window_ts = market.window_start_ts;

                let roll = rand::thread_rng().gen_range(1u32..=100);
                window_skip = roll <= 25;

                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                if window_skip {
                    info!(">>> NEW WINDOW {} — SKIPPING (roll={}/25)", market.window_start_ts, roll);
                } else {
                    info!(">>> NEW WINDOW {} — PLAYING (roll={}) max {:.0}sh/side BTC 15M", market.window_start_ts, roll, max_shares_per_side);
                }
            }
        }

        tokio::select! {
            // Store latest OBI for adverse selection gating
            Some(sig) = signal_rx.recv() => {
                latest_obi = sig.obi_ema;
            }

            // Process executor events — confirm matching GTC was accepted
            Some(evt) = exec_evt_rx.recv() => {
                match evt {
                    ExecutionEvent::MakerOrderPosted { ladder_key, order_id, .. } => {
                        if ladder_key == "matching_gtc" {
                            matching_gtc_posted = true;
                            info!(">>> MATCHING GTC CONFIRMED: order_id={}", &order_id[..16.min(order_id.len())]);
                        }
                    }
                    _ => {}
                }
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

                // Only check match AFTER one side is full (>= fill_full_threshold)
                let fill_full_threshold = max_shares_per_side - 4.0; // 196 when cap = 200
                if !pair_done && (up_shares >= fill_full_threshold || dn_shares >= fill_full_threshold) {
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
                info!("HEARTBEAT BTC 15M: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh | pair_done={} gtc_match={} | fills={}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, pair_done, matching_gtc_posted, fills_this_window);
            }

            // ═══ MAIN STRATEGY LOOP — every 100ms ═══
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if pair_done { continue; }

                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();
                if market.up_token_id.is_empty() { continue; }

                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                // Start at T-60s (1 minute BEFORE window opens)
                if elapsed_s < -60 { continue; }
                // Late acceptance at T+735 (~87.5% of window): if lagging ≥85% of leader, accept
                if elapsed_s >= 735 && !pair_done {
                    let full_s = up_shares.max(dn_shares);
                    let weak_s = up_shares.min(dn_shares);
                    if full_s > 0.0 && weak_s >= full_s * 0.85 {
                        info!(">>> T+735 LATE ACCEPT (15M): full={:.0} weak={:.0} ({:.0}%) — holding",
                            full_s, weak_s, weak_s / full_s * 100.0);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "late_accept".into(),
                        }).await;
                        continue;
                    }
                }

                // Stop at T+840s (14 minutes)
                if elapsed_s >= 840 {
                    if !pair_done {
                        info!(">>> T+840s — window over (BTC 15M), holding to resolution");
                        pair_done = true;
                    }
                    continue;
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

                // OBI spike cancel — cancel immediately if signal crosses hard threshold
                if orders_live && latest_obi.abs() > 0.55 {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "obi_spike".into(),
                    }).await;
                    orders_live = false;
                    last_cancel_ts = now_ms;
                    next_post_interval_ms = rand::thread_rng().gen_range(500..=2000);
                    info!(">>> OBI SPIKE CANCEL: obi={:.3}", latest_obi);
                }

                // ═══ CANCEL (random lifetime) — sample next repost interval at cancel time ═══
                if orders_live && (now_ms - last_post_ts) >= cancel_delay_ms {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "flash_cancel".into(),
                    }).await;
                    orders_live = false;
                    last_cancel_ts = now_ms;
                    next_post_interval_ms = rand::thread_rng().gen_range(500..=2000);
                }

                // ═══ PHASE 1: Both sides need tokens — post flash ladders ═══
                let up_needs = (max_shares_per_side - up_shares).max(0.0);
                let dn_needs = (max_shares_per_side - dn_shares).max(0.0);

                // Repair mode: REVERSIBLE — enters when weak side falls below 60% of strong side,
                // exits when weak side recovers to ≥95% of strong side.
                {
                    let full_v = up_shares.max(dn_shares);
                    let weak_v = up_shares.min(dn_shares);
                    if !repair_mode {
                        if full_v >= 5.0 && weak_v < full_v * 0.60 {
                            repair_mode = true;
                            info!(">>> REPAIR MODE ON (15M): full={:.0} weak={:.0} ({:.0}%)",
                                full_v, weak_v, weak_v / full_v * 100.0);
                        }
                    } else if weak_v >= full_v * 0.95 {
                        repair_mode = false;
                        info!(">>> REPAIR MODE OFF (15M): full={:.0} weak={:.0} — rebalanced",
                            full_v, weak_v);
                    }
                }

                // Phase 2 mode: ONE-WAY latch — overweight side hit practical cap,
                // OR T+630s emergency (75% of 840s window) if still not matched within 5%.
                // Once latched: only quote underweight side at offset=1.
                if !phase2_mode {
                    let full_v = up_shares.max(dn_shares);
                    let weak_v = up_shares.min(dn_shares);
                    let ratio   = weak_v / full_v.max(0.001);
                    let capped    = up_shares >= practical_full || dn_shares >= practical_full;
                    let emergency = elapsed_s >= 630 && full_v >= 5.0 && ratio < 0.95;
                    if capped || emergency {
                        phase2_mode = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "phase2_start".into(),
                        }).await;
                        orders_live = false;
                        last_cancel_ts = now_ms;
                        info!(">>> PHASE 2 START (15M): capped={} emergency={} full={:.0} ratio={:.0}%",
                            capped, emergency, full_v, ratio * 100.0);
                    }
                }

                // ═══ PHASE 1: Both sides, repair-aware offsets ═══
                if !window_skip && !phase2_mode {
                    if !orders_live && (now_ms - last_cancel_ts) >= next_post_interval_ms {
                        // Repair:        overweight offset=5 (bid-5c/-6c/-7c), underweight offset=1 (bid-1c/-2c/-3c)
                        // Normal:        random base 2/3/4 → ladders -2/-3/-4, -3/-4/-5, or -4/-5/-6
                        // Late (T+630s): force underweight to offset=1, overweight stays random
                        let (up_offset, dn_offset) = if repair_mode {
                            if up_shares >= dn_shares {
                                (5usize, 1usize)   // UP overweight → throttle UP, aggressive DN
                            } else {
                                (1usize, 5usize)   // DN overweight → throttle DN, aggressive UP
                            }
                        } else {
                            let rand_base = rand::thread_rng().gen_range(2usize..=4);
                            if elapsed_s >= 630 {
                                // Late aggression: push underweight to offset=1, overweight stays random
                                if up_shares >= dn_shares { (rand_base, 1usize) } else { (1usize, rand_base) }
                            } else {
                                (rand_base, rand_base)
                            }
                        };
                        let mut posted_any = false;

                        // UP side
                        if up_needs >= 5.0 {
                            let base_sizes = [rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64];
                            let mut up_remaining = up_needs;
                            for (level, &base_size) in base_sizes.iter().enumerate() {
                                let size = base_size.min(up_remaining);
                                if size < 5.0 { break; }
                                let offset = (up_offset + level) as f64 * 0.01;
                                let price = ((up_bid - offset) * 100.0).round() / 100.0;
                                if price <= 0.01 || price >= up_ask { continue; }

                                let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                                    market_slug: market.slug.clone(),
                                    token_id: market.up_token_id.clone(),
                                    side: Side::Up,
                                    price,
                                    size,
                                    post_only: true,
                                    ladder_key: format!("up:{}", level),
                                    was_cheap: up_ask < dn_ask,
                                }).await;
                                posted_any = true;
                                up_remaining -= size;
                                if up_remaining < 5.0 { break; }
                            }
                        }

                        // DN side
                        if dn_needs >= 5.0 {
                            let base_sizes = [rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64];
                            let mut dn_remaining = dn_needs;
                            for (level, &base_size) in base_sizes.iter().enumerate() {
                                let size = base_size.min(dn_remaining);
                                if size < 5.0 { break; }
                                let offset = (dn_offset + level) as f64 * 0.01;
                                let price = ((dn_bid - offset) * 100.0).round() / 100.0;
                                if price <= 0.01 || price >= dn_ask { continue; }

                                let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                                    market_slug: market.slug.clone(),
                                    token_id: market.down_token_id.clone(),
                                    side: Side::Down,
                                    price,
                                    size,
                                    post_only: true,
                                    ladder_key: format!("dn:{}", level),
                                    was_cheap: dn_ask < up_ask,
                                }).await;
                                posted_any = true;
                                dn_remaining -= size;
                                if dn_remaining < 5.0 { break; }
                            }
                        }

                        if posted_any {
                            orders_live = true;
                            last_post_ts = now_ms;
                            cancel_delay_ms = rand::thread_rng().gen_range(500..=2000);
                            info!(">>> POST P1{} (15M): up_off={} dn_off={} obi={:.2} cancel={}ms",
                                if repair_mode { " [REPAIR]" } else { "" },
                                up_offset, dn_offset, latest_obi, cancel_delay_ms);
                        }
                    }
                }

                // ═══ PHASE 2: Underweight side only, aggressive offset=1 ═══
                if !window_skip && phase2_mode && !pair_done {
                    let (full_side, full_shares_v, full_cost_v) =
                        if up_shares >= dn_shares {
                            ("UP", up_shares, up_cost)
                        } else {
                            ("DN", dn_shares, dn_cost)
                        };

                    let (need_side, need_token, need_bid, need_ask, already) =
                        if up_shares >= dn_shares {
                            (Side::Down, &market.down_token_id, dn_bid, dn_ask, dn_shares)
                        } else {
                            (Side::Up, &market.up_token_id, up_bid, up_ask, up_shares)
                        };

                    let still_need = (full_shares_v - already).max(0.0);

                    // Repair max bid: pair cost target relaxes as imbalance worsens.
                    let match_ratio = already / full_shares_v.max(0.001);
                    let repair_target = repair_pair_cost_target(match_ratio);
                    let full_avg_cost = full_cost_v / full_shares_v.max(0.001);
                    let repair_max_bid = (repair_target - full_avg_cost).max(0.30);

                    // Within 5% tolerance? Done.
                    if already >= full_shares_v * (1.0 - match_tolerance) {
                        info!(">>> PHASE 2 MATCHED (15M): {} {:.0}sh, other {:.0}sh — done",
                            full_side, full_shares_v, already);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "matched_within_tolerance".into(),
                        }).await;
                    } else if still_need < 5.0 {
                        info!(">>> PHASE 2 CLOSE (15M): {} {:.0}sh, other {:.0}sh, need {:.1} < 5 — done",
                            full_side, full_shares_v, already, still_need);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "close_enough_sub5".into(),
                        }).await;
                    } else if !orders_live && (now_ms - last_cancel_ts) >= next_post_interval_ms {
                        // Aggressive: offset_base=1 → bid-1c/-2c/-3c
                        let offset_base = 1usize;
                        let base_sizes = [rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64, rand::thread_rng().gen_range(5u32..=8) as f64];
                        let mut remaining = still_need.min(max_shares_per_side - already);
                        let need_label = if need_side == Side::Up { "UP" } else { "DN" };
                        let mut posted_any = false;

                        for (level, &base_size) in base_sizes.iter().enumerate() {
                            let size = base_size.min(remaining);
                            if size < 5.0 { break; }
                            let offset = (offset_base + level) as f64 * 0.01;
                            let price = ((need_bid - offset) * 100.0).round() / 100.0;
                            if price <= 0.01 || price >= need_ask || price > repair_max_bid { continue; }

                            let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                                market_slug: market.slug.clone(),
                                token_id: need_token.clone(),
                                side: need_side,
                                price,
                                size,
                                post_only: true,
                                ladder_key: format!("p2_{}:{}", need_label.to_lowercase(), level),
                                was_cheap: price < 0.50,
                            }).await;
                            posted_any = true;
                            remaining -= size;
                            if remaining < 5.0 { break; }
                        }

                        if posted_any {
                            orders_live = true;
                            last_post_ts = now_ms;
                            cancel_delay_ms = rand::thread_rng().gen_range(500..=2000);
                        }
                        info!(">>> PHASE 2 (15M): {} full {:.0}sh | {} needs {:.0} @ bid-1c | cap={:.0}c",
                            full_side, full_shares_v, need_label, still_need, repair_max_bid * 100.0);
                    }
                }
            }
        }
    }
}
