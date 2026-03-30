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

/// FLASH LADDER Strategy (BTC 15M) — post both-side ladders, cancel after 2000ms, repeat.
///
/// 1. T-60s: start posting 3-level ladders on BOTH sides (bid-3c, -4c, -5c)
/// 2. Cancel all after 2000ms (avoid catching falling knives)
/// 3. Re-post every 2s with fresh prices
/// 4. Max 200sh per side (practical_full=196). Once one side hits 196sh:
///    - If other side within 10% → done, hold to resolution
///    - If not → keep laddering the underweight side
///    - Stop at T+840s
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
    let match_tolerance = 0.10; // within 10% = considered matched
    let target_pair_cost = 0.95; // lockprofit target

    // Timing — randomized post interval 500–2000ms, fixed 2s cancel
    let cancel_delay_ms: i64 = 2000;
    let mut next_post_interval_ms: i64 = 2000; // randomized each cycle
    let mut last_post_ts: i64 = 0;
    let mut orders_live = false;             // true while orders are on the book

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
                orders_live = false;
                last_post_ts = 0;
                last_window_ts = market.window_start_ts;

                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                info!(">>> NEW WINDOW {} — BTC 15M flash ladder max {:.0}sh/side", market.window_start_ts, max_shares_per_side);
            }
        }

        tokio::select! {
            // Drain signals (not used directly but keeps channel flowing)
            Some(_) = signal_rx.recv() => {}

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

                // Only check match AFTER one side is full (>= practical_full)
                let practical_full = max_shares_per_side - 4.0; // 196 when cap = 200
                if !pair_done && (up_shares >= practical_full || dn_shares >= practical_full) {
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

                // ═══ CANCEL after fixed 2s ═══
                if orders_live && (now_ms - last_post_ts) >= cancel_delay_ms {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "flash_cancel_2s".into(),
                    }).await;
                    orders_live = false;
                }

                // ═══ PHASE 1: Both sides need tokens — post flash ladders ═══
                let up_needs = (max_shares_per_side - up_shares).max(0.0);
                let dn_needs = (max_shares_per_side - dn_shares).max(0.0);

                // Check if one side is full
                // Use practical_full (196) to avoid dead zone when remaining < 5 (venue min)
                let practical_full = max_shares_per_side - 4.0; // 196 when cap = 200
                let one_side_full = up_shares >= practical_full || dn_shares >= practical_full;

                if !one_side_full && !matching_gtc_posted {
                    // Both sides still need tokens — randomized interval + offset
                    if !orders_live && (now_ms - last_post_ts) >= next_post_interval_ms {
                        // Randomize: interval 500–2000ms, offset_base 1–3 (bid-1/2/3 to bid-3/4/5)
                        next_post_interval_ms = rand::thread_rng().gen_range(500..=2000);
                        let offset_base = rand::thread_rng().gen_range(1usize..=3);
                        // UP side
                        if up_needs >= 5.0 {
                            let base_sizes = [8.0_f64, 6.0, 6.0];
                            let mut up_remaining = up_needs;
                            for (level, &base_size) in base_sizes.iter().enumerate() {
                                let size = base_size.min(up_remaining);
                                if size < 5.0 { break; }
                                let offset = (offset_base + level) as f64 * 0.01;
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
                                up_remaining -= size;
                                if up_remaining < 5.0 { break; }
                            }
                        }

                        // DN side
                        if dn_needs >= 5.0 {
                            let base_sizes = [8.0_f64, 6.0, 6.0];
                            let mut dn_remaining = dn_needs;
                            for (level, &base_size) in base_sizes.iter().enumerate() {
                                let size = base_size.min(dn_remaining);
                                if size < 5.0 { break; }
                                let offset = (offset_base + level) as f64 * 0.01;
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
                                dn_remaining -= size;
                                if dn_remaining < 5.0 { break; }
                            }
                        }

                        orders_live = true;
                        last_post_ts = now_ms;
                    }
                }

                // ═══ PHASE 2: One side full — keep laddering the underweight side ═══
                if one_side_full && !pair_done {
                    let (full_side, full_shares, _full_cost) =
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

                    let still_need = (full_shares - already).max(0.0);

                    // Within 10% tolerance? Done.
                    if already >= full_shares * (1.0 - match_tolerance) {
                        info!(">>> MATCHED (within 10%): {} {:.0}sh, other {:.0}sh — done",
                            full_side, full_shares, already);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "matched_within_tolerance".into(),
                        }).await;
                    } else if still_need < 5.0 {
                        info!(">>> CLOSE ENOUGH: {} {:.0}sh, other {:.0}sh, need {:.1} < 5 — done",
                            full_side, full_shares, already, still_need);
                        pair_done = true;
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "close_enough_sub5".into(),
                        }).await;
                    } else if !orders_live && (now_ms - last_post_ts) >= next_post_interval_ms {
                        next_post_interval_ms = rand::thread_rng().gen_range(500..=2000);
                        let offset_base = rand::thread_rng().gen_range(1usize..=3);
                        let base_sizes = [8.0_f64, 6.0, 6.0];
                        let mut remaining = still_need.min(max_shares_per_side - already);
                        let need_label = if need_side == Side::Up { "UP" } else { "DN" };

                        for (level, &base_size) in base_sizes.iter().enumerate() {
                            let size = base_size.min(remaining);
                            if size < 5.0 { break; }
                            let offset = (offset_base + level) as f64 * 0.01;
                            let price = ((need_bid - offset) * 100.0).round() / 100.0;
                            if price <= 0.01 || price >= need_ask { continue; }

                            let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                                market_slug: market.slug.clone(),
                                token_id: need_token.clone(),
                                side: need_side,
                                price,
                                size,
                                post_only: true,
                                ladder_key: format!("match_{}:{}", need_label.to_lowercase(), level),
                                was_cheap: price < 0.50,
                            }).await;
                            remaining -= size;
                            if remaining < 5.0 { break; }
                        }

                        orders_live = true;
                        last_post_ts = now_ms;
                        info!(">>> PHASE 2 BTC 15M: {} full {:.0}sh | {} needs {:.0} more | ladders -3/-4/-5c",
                            full_side, full_shares, need_label, still_need);
                    }
                }
            }
        }
    }
}
