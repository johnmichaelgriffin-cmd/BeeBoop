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

    info!("=== BEEBOOP LOCKPROFIT XRP 5M ===");
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
    tokio::spawn(feeds::binance::run_binance_feed_task_for("xrpusdt",
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
    tokio::spawn(feeds::rtds::run_rtds_task_for("xrp/usd",
        chainlink_tx, chainlink_watch_tx, log_tx.clone(),
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

/// LOCKPROFIT Strategy — wait for 60c, FOK expensive, GTC bid cheap at (95c - fill).
///
/// 1. Wait for one side to hit 60c ask
/// 2. FOK buy 20sh of the expensive side at 99c limit
/// 3. Post GTC postOnly bid on cheap side at (95c - fill_price), 20sh
/// 4. Hold both to resolution. One pair per window.
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
    info!("LOCKPROFIT: wait for 60c → FOK 20sh expensive → GTC bid cheap at (95c - fill)");

    // Per-window state
    let mut last_window_ts: i64 = 0;
    let mut pair_done = false;          // true once FULLY done (GTC filled or stop-loss triggered)
    let mut exp_fill_price: f64 = 0.0;  // what we paid for the expensive side
    let mut exp_fill_shares: f64 = 0.0;
    let mut cheap_gtc_posted = false;    // have we posted the cheap GTC?
    let mut watching_for_flip = false;   // monitoring cheap side for 60c flip
    let mut cheap_token_id: String = String::new();  // which token the GTC is on
    let mut cheap_side_saved: Option<Side> = None;
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    let mut fills_this_window: u32 = 0;

    let target_profit_per_pair = 0.95;   // buy both sides for 95c total → 5c profit
    let momentum_gate = 0.60;            // wait for one side to hit 60c

    // ═══ AUTO-SCALING ═══
    // Track lockprofit success over rolling 30-window batches.
    // If >=27/30 lock successfully, scale up +20sh. If <27/30, scale back down.
    let mut shares_per_leg: f64 = 40.0;
    let min_shares: f64 = 20.0;
    let scale_window_batch: usize = 30;
    let scale_success_threshold: usize = 27; // 90% success rate to scale up
    let scale_step: f64 = 20.0;
    let mut window_results: std::collections::VecDeque<bool> = std::collections::VecDeque::new(); // true = both legs filled
    let mut both_legs_filled_this_window = false;

    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        // Window change detection
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                // Log previous window + record lockprofit result
                if last_window_ts > 0 {
                    let matched = up_shares.min(dn_shares);
                    let pair_cost = if matched > 0.0 {
                        let up_avg = up_cost / up_shares.max(0.001);
                        let dn_avg = dn_cost / dn_shares.max(0.001);
                        up_avg + dn_avg
                    } else { 0.0 };

                    if up_shares > 0.0 || dn_shares > 0.0 {
                        info!(">>> WINDOW COMPLETE: UP {:.0}sh (${:.2}) DN {:.0}sh (${:.2}) | matched={:.0} pcost={:.0}c | fills={} | locked={}",
                            up_shares, up_cost, dn_shares, dn_cost,
                            matched, pair_cost * 100.0, fills_this_window, both_legs_filled_this_window);
                    }

                    // ═══ AUTO-SCALE: record result and check batch ═══
                    if pair_done {
                        window_results.push_back(both_legs_filled_this_window);
                    }
                    // Only check scaling every 30 windows
                    if window_results.len() >= scale_window_batch {
                        let successes = window_results.iter().filter(|&&v| v).count();
                        let old_shares = shares_per_leg;

                        if successes >= scale_success_threshold {
                            // 90%+ success → scale UP
                            shares_per_leg += scale_step;
                            info!(">>> AUTO-SCALE UP: {}/{} locked ({:.0}%) → shares {:.0} → {:.0}",
                                successes, scale_window_batch,
                                successes as f64 / scale_window_batch as f64 * 100.0,
                                old_shares, shares_per_leg);
                        } else {
                            // Below 90% → scale DOWN (but not below minimum)
                            shares_per_leg = (shares_per_leg - scale_step).max(min_shares);
                            if shares_per_leg < old_shares {
                                warn!(">>> AUTO-SCALE DOWN: {}/{} locked ({:.0}%) → shares {:.0} → {:.0}",
                                    successes, scale_window_batch,
                                    successes as f64 / scale_window_batch as f64 * 100.0,
                                    old_shares, shares_per_leg);
                            }
                        }
                        // Clear the batch for next 30
                        window_results.clear();
                    }
                }

                // Reset for new window
                up_shares = 0.0; dn_shares = 0.0;
                up_cost = 0.0; dn_cost = 0.0;
                fills_this_window = 0;
                pair_done = false;
                exp_fill_price = 0.0;
                exp_fill_shares = 0.0;
                cheap_gtc_posted = false;
                watching_for_flip = false;
                cheap_token_id.clear();
                cheap_side_saved = None;
                both_legs_filled_this_window = false;
                last_window_ts = market.window_start_ts;

                // Cancel leftover GTC from previous window
                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                info!(">>> NEW WINDOW {} — {:.0}sh/leg — waiting for 60c momentum ({}/{} in current batch)",
                    market.window_start_ts, shares_per_leg,
                    window_results.len(), scale_window_batch);
            }
        }

        tokio::select! {
            // Drain signals (we don't use them for lockprofit, but need to drain the channel)
            Some(_signal) = signal_rx.recv() => {}

            // Drain executor events
            Some(_evt) = exec_evt_rx.recv() => {}

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

                // Check if both legs now have fills
                if up_shares > 0.0 && dn_shares > 0.0 {
                    both_legs_filled_this_window = true;
                }

                info!(">>> FILL: {} {:.1}sh @ {:.2} | UP={:.0} DN={:.0} matched={:.0} pcost={:.0}c | fills={} | locked={}",
                    side_label, fill.size, fill.price,
                    up_shares, dn_shares, matched, pair_cost * 100.0, fills_this_window, both_legs_filled_this_window);
            }

            // Heartbeat
            _ = heartbeat_interval.tick() => {
                let top = pm_top_rx.borrow().clone();
                info!("HEARTBEAT: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh | pair_done={} locked={} | {:.0}sh/leg | batch {}/{}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, pair_done, both_legs_filled_this_window,
                    shares_per_leg, window_results.len(), scale_window_batch);
            }

            // ═══ MAIN STRATEGY LOOP — every 500ms ═══
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if pair_done { continue; } // already done for this window

                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();
                if market.up_token_id.is_empty() { continue; }

                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                // Wait for T+10s
                if elapsed_s < 10 { continue; }
                // Stop at T+240s
                if elapsed_s >= 240 { continue; }

                // Get prices
                let top = pm_top_rx.borrow().clone();
                let (up_ask, dn_ask) = match (top.up_ask, top.down_ask) {
                    (Some(ua), Some(da)) => (ua, da),
                    _ => continue,
                };

                // ═══ STEP 1: Wait for momentum (60c on one side) ═══
                if exp_fill_price == 0.0 {
                    // Determine which side is expensive
                    let (exp_side, exp_ask, exp_token, cheap_token, cheap_side) = if up_ask >= dn_ask {
                        (Side::Up, up_ask, &market.up_token_id, &market.down_token_id, Side::Down)
                    } else {
                        (Side::Down, dn_ask, &market.down_token_id, &market.up_token_id, Side::Up)
                    };

                    // Momentum gate: expensive side must be >= 60c
                    if exp_ask < momentum_gate {
                        continue; // not ready yet
                    }

                    // FOK BUY 20sh of expensive side at 99c
                    info!(">>> MOMENTUM HIT: {} ask={:.0}c — FOK BUY {:.0}sh @ 99c",
                        if exp_side == Side::Up { "UP" } else { "DN" },
                        exp_ask * 100.0, shares_per_leg);

                    let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
                        market_slug: market.slug.clone(),
                        token_id: exp_token.clone(),
                        side: exp_side,
                        max_price: exp_ask, // used as ask_price for lockprofit formula
                        notional: shares_per_leg, // share_base for the formula
                        signal: Signal {
                            created_ts_ms: now_ms,
                            side: exp_side,
                            entry_mode: EntryMode::Taker,
                            r200_bps: 0.0, r500_bps: 0.0, r800_bps: 0.0, r2000_bps: 0.0,
                            fast_move_bps: 0.0, raw_basis_bps: 0.0, basis_dev_bps: 0.0,
                            dbasis_bps: 0.0, obi_ema: 0.0, score: 0.0, confidence: 0.0,
                            strong_contradiction: false,
                        },
                    }).await;

                    // Estimate fill price (actual fill comes from user WS)
                    exp_fill_price = exp_ask;
                    exp_fill_shares = shares_per_leg;

                    // ═══ STEP 2: Immediately post GTC bid on cheap side ═══
                    let cheap_bid_price = target_profit_per_pair - exp_ask;

                    if cheap_bid_price > 0.01 && cheap_bid_price < 0.50 {
                        // Round to 2 decimal places
                        let cheap_bid_price = (cheap_bid_price * 100.0).round() / 100.0;

                        info!(">>> GTC BID: {} {:.0}sh @ {:.0}c (lockprofit: 95c - {:.0}c = {:.0}c)",
                            if cheap_side == Side::Up { "UP" } else { "DN" },
                            shares_per_leg, cheap_bid_price * 100.0,
                            exp_ask * 100.0, cheap_bid_price * 100.0);

                        let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                            market_slug: market.slug.clone(),
                            token_id: cheap_token.clone(),
                            side: cheap_side,
                            price: cheap_bid_price,
                            size: shares_per_leg,
                            post_only: true,
                            ladder_key: "lockprofit_cheap".into(),
                            was_cheap: true,
                        }).await;

                        cheap_gtc_posted = true;
                        watching_for_flip = true; // NOT pair_done — watch for GTC fill or flip
                        cheap_token_id = cheap_token.clone();
                        cheap_side_saved = Some(cheap_side);
                        info!(">>> GTC POSTED + WATCHING: exp {:.0}c + cheap bid {:.0}c = {:.0}c target | watching for fill or flip",
                            exp_ask * 100.0, cheap_bid_price * 100.0,
                            (exp_ask + cheap_bid_price) * 100.0);
                    } else {
                        warn!(">>> CHEAP BID PRICE {:.2} out of range — skipping cheap leg", cheap_bid_price);
                        pair_done = true;
                    }
                }

                // ═══ STEP 3: WATCH FOR GTC FILL, FLIP STOP-LOSS, OR T+210 FORCE BUY ═══
                if watching_for_flip && !pair_done {
                    // First: check if GTC already filled (both sides have shares)
                    if up_shares > 0.0 && dn_shares > 0.0 {
                        info!(">>> GTC FILLED — both legs complete, lockprofit done");
                        watching_for_flip = false;
                        pair_done = true;
                    }

                    // Only do flip detection / force buy AFTER T+210s
                    if !pair_done && elapsed_s >= 210 {
                        let cheap_ask_now = if cheap_side_saved == Some(Side::Up) { up_ask } else { dn_ask };

                        // FLIP: cheap side surged to 60c+ → stop-loss buy
                        if cheap_ask_now >= momentum_gate {
                            warn!(">>> FLIP DETECTED at T+{}s: cheap ask={:.0}c >= 60c — STOP-LOSS FOK",
                                elapsed_s, cheap_ask_now * 100.0);

                            // Cancel the resting GTC first
                            let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                                reason: "flip_stop_loss".into(),
                            }).await;
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                            let stop_side = cheap_side_saved.unwrap_or(Side::Down);
                            let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
                                market_slug: market.slug.clone(),
                                token_id: cheap_token_id.clone(),
                                side: stop_side,
                                max_price: cheap_ask_now,
                                notional: shares_per_leg,
                                signal: Signal {
                                    created_ts_ms: now_ms,
                                    side: stop_side,
                                    entry_mode: EntryMode::Taker,
                                    r200_bps: 0.0, r500_bps: 0.0, r800_bps: 0.0, r2000_bps: 0.0,
                                    fast_move_bps: 0.0, raw_basis_bps: 0.0, basis_dev_bps: 0.0,
                                    dbasis_bps: 0.0, obi_ema: 0.0, score: 0.0, confidence: 0.0,
                                    strong_contradiction: false,
                                },
                            }).await;

                            let est_loss = exp_fill_price + cheap_ask_now - 1.0;
                            warn!(">>> STOP-LOSS: exp {:.0}c + cheap FOK {:.0}c = {:.0}c | est loss ~{:.0}c/pair",
                                exp_fill_price * 100.0, cheap_ask_now * 100.0,
                                (exp_fill_price + cheap_ask_now) * 100.0, est_loss * 100.0);

                            watching_for_flip = false;
                            pair_done = true;
                        }
                    }

                    // T+210 FORCE BUY: if GTC hasn't filled by T+210, buy cheap at market
                    if !pair_done && elapsed_s >= 210 && !(up_shares > 0.0 && dn_shares > 0.0) {
                        let cheap_ask_now = if cheap_side_saved == Some(Side::Up) { up_ask } else { dn_ask };

                        // Only force buy if cheap side is still actually cheap (< 60c)
                        if cheap_ask_now < momentum_gate {
                            info!(">>> T+210 FORCE BUY: GTC unfilled, cheap ask={:.0}c — FOK buying to match",
                                cheap_ask_now * 100.0);

                            // Cancel resting GTC
                            let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                                reason: "force_match_t210".into(),
                            }).await;
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                            let stop_side = cheap_side_saved.unwrap_or(Side::Down);
                            let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
                                market_slug: market.slug.clone(),
                                token_id: cheap_token_id.clone(),
                                side: stop_side,
                                max_price: cheap_ask_now,
                                notional: shares_per_leg,
                                signal: Signal {
                                    created_ts_ms: now_ms,
                                    side: stop_side,
                                    entry_mode: EntryMode::Taker,
                                    r200_bps: 0.0, r500_bps: 0.0, r800_bps: 0.0, r2000_bps: 0.0,
                                    fast_move_bps: 0.0, raw_basis_bps: 0.0, basis_dev_bps: 0.0,
                                    dbasis_bps: 0.0, obi_ema: 0.0, score: 0.0, confidence: 0.0,
                                    strong_contradiction: false,
                                },
                            }).await;

                            let est_cost = exp_fill_price + cheap_ask_now;
                            info!(">>> FORCE MATCH: exp {:.0}c + cheap FOK {:.0}c = {:.0}c",
                                exp_fill_price * 100.0, cheap_ask_now * 100.0, est_cost * 100.0);

                            watching_for_flip = false;
                            pair_done = true;
                        }
                    }
                }
            }
        }
    }
}
