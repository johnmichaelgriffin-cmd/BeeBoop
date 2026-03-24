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

/// Vidarx Strategy — buy-only maker accumulator
///
/// Posts GTC postOnly bids on both sides, skewed by signal.
/// Tracks inventory, manages quote lifecycle, holds to resolution.
async fn run_vidarx_strategy(
    config: Config,
    shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut signal_rx: mpsc::Receiver<Signal>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    mut exec_evt_rx: mpsc::Receiver<ExecutionEvent>,
    mut fill_rx: mpsc::Receiver<feeds::user_ws::UserFillEvent>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    info!("vidarx_strategy: started");

    // Per-window tracking
    let mut last_window_ts: i64 = 0;
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    // Fill-time cheap/exp classification (not refresh-time)
    let mut cheap_shares: f64 = 0.0;
    let mut exp_shares: f64 = 0.0;
    let mut cheap_cost: f64 = 0.0;
    let mut exp_cost: f64 = 0.0;
    let mut quotes_posted: u32 = 0;
    let mut fills_this_window: u32 = 0;
    // Track shares POSTED as a worst-case inventory estimate
    // (assumes all posted orders fill — conservative for balancing)
    let mut up_shares_posted: f64 = 0.0;
    let mut dn_shares_posted: f64 = 0.0;
    let mut last_quote_ts: i64 = 0;
    let mut last_signal: Option<Signal> = None;
    let mut window_active = false;
    let mut last_reconcile_ts: i64 = 0;
    let reconcile_interval_ms: i64 = 10_000;
    let mut last_price_update_ms: i64 = 0;

    // Quote refresh interval
    let quote_refresh_ms: i64 = 2000;
    // Ladder sizes
    let cheap_ladder = [20.0_f64, 15.0, 10.0];
    let exp_ladder = [15.0_f64, 12.0, 8.0];

    // #2: Live order tracking for selective refresh (avoid cancel-all every cycle)
    // Maps "side:level" -> (order_id, posted_price, posted_size)
    let mut live_orders: std::collections::HashMap<String, (String, f64, f64)> = std::collections::HashMap::new();

    // #3: Dynamic tick size per token (updated from WS tick_size_change events)
    let mut up_tick_size: f64 = 0.01;
    let mut dn_tick_size: f64 = 0.01;

    // #4: Order metadata map for fill attribution
    // Maps order_id -> PostedOrderMeta
    struct PostedOrderMeta {
        _asset_id: String,
        was_cheap_at_post: bool,
        _ladder_level: u8,
        posted_price: f64,
    }
    let mut order_meta: std::collections::HashMap<String, PostedOrderMeta> = std::collections::HashMap::new();

    // #5: Our wallet address for reconciliation filtering
    let our_wallet = if !config.poly_private_key.is_empty() {
        // Derive wallet address from private key
        let pk_clean = if config.poly_private_key.starts_with("0x") {
            &config.poly_private_key[2..]
        } else {
            &config.poly_private_key
        };
        use alloy::signers::local::PrivateKeySigner;
        use std::str::FromStr;
        PrivateKeySigner::from_str(pk_clean)
            .map(|s| format!("{:?}", s.address()).to_lowercase())
            .unwrap_or_default()
    } else {
        String::new()
    };
    if !our_wallet.is_empty() {
        info!("Wallet for reconciliation: {}", &our_wallet[..12.min(our_wallet.len())]);
    }

    // Rolling regime filter state
    let mut rolling_pnl: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
    let mut rolling_pair_costs: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
    let mut market_paused = false;

    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        // Window change detection
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                // New window — log previous window results if any
                if last_window_ts > 0 && (up_shares > 0.0 || dn_shares > 0.0) {
                    let matched = up_shares.min(dn_shares);
                    let pair_cost = if matched > 0.0 {
                        let up_avg = if up_shares > 0.0 { up_cost / up_shares } else { 0.0 };
                        let dn_avg = if dn_shares > 0.0 { dn_cost / dn_shares } else { 0.0 };
                        up_avg + dn_avg
                    } else { 0.0 };
                    let cheap_ratio = if up_shares + dn_shares > 0.0 {
                        up_shares.min(dn_shares) / (up_shares + dn_shares) * 2.0 // approximate
                    } else { 0.0 };

                    info!(">>> WINDOW COMPLETE: UP {:.0}sh (${:.2}) DN {:.0}sh (${:.2}) | matched={:.0} pair_cost={:.2}c | fills={} quotes={}",
                        up_shares, up_cost, dn_shares, dn_cost,
                        matched, pair_cost * 100.0,
                        fills_this_window, quotes_posted);
                }

                // Rolling regime tracking — estimate window PnL
                // (crude: assume winner resolves to $1, loser to $0)
                if fills_this_window > 0 {
                    let matched = up_shares.min(dn_shares);
                    let pair_cost = if matched > 0.0 {
                        let up_avg = up_cost / up_shares.max(0.001);
                        let dn_avg = dn_cost / dn_shares.max(0.001);
                        up_avg + dn_avg
                    } else { 1.0 };

                    // Estimate PnL: matched * (1 - pair_cost) + max residual * (0.5 - avg_cost)
                    let est_pnl = matched * (1.0 - pair_cost);
                    rolling_pnl.push_back(est_pnl);
                    rolling_pair_costs.push_back(pair_cost);
                    while rolling_pnl.len() > config.tune.rolling_short_windows { rolling_pnl.pop_front(); }
                    while rolling_pair_costs.len() > config.tune.rolling_short_windows { rolling_pair_costs.pop_front(); }

                    // Check pause conditions
                    if rolling_pnl.len() >= config.tune.rolling_short_windows {
                        let avg_pnl: f64 = rolling_pnl.iter().sum::<f64>() / rolling_pnl.len() as f64;
                        let avg_pc: f64 = rolling_pair_costs.iter().sum::<f64>() / rolling_pair_costs.len() as f64;
                        if avg_pnl < config.tune.rolling_pause_pnl_per_window || avg_pc > config.tune.rolling_pause_pair_cost {
                            if !market_paused {
                                market_paused = true;
                                warn!(">>> REGIME PAUSED: rolling_pnl={:.1}/w pair_cost={:.3} — waiting for recovery", avg_pnl, avg_pc);
                            }
                        }
                    }
                }

                // Reset for new window
                up_shares = 0.0;
                dn_shares = 0.0;
                up_cost = 0.0;
                dn_cost = 0.0;
                cheap_shares = 0.0;
                exp_shares = 0.0;
                cheap_cost = 0.0;
                exp_cost = 0.0;
                quotes_posted = 0;
                fills_this_window = 0;
                up_shares_posted = 0.0;
                dn_shares_posted = 0.0;
                last_quote_ts = 0;
                last_signal = None;
                window_active = false;
                last_price_update_ms = 0;
                last_window_ts = market.window_start_ts;
                live_orders.clear();
                order_meta.clear();
                up_tick_size = 0.01;
                dn_tick_size = 0.01;

                // Cancel any leftover orders from previous window
                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                    reason: "new_window".into(),
                }).await;

                info!(">>> NEW WINDOW {} — ready to accumulate", market.window_start_ts);
            }
        }

        tokio::select! {
            // Receive signals — just save the latest, don't act immediately
            Some(signal) = signal_rx.recv() => {
                last_signal = Some(signal);
            }

            // Receive execution events (order posted acks, cancel acks, reconciliation)
            Some(evt) = exec_evt_rx.recv() => {
                match evt {
                    // Maker order successfully posted — track it
                    ExecutionEvent::MakerOrderPosted { order_id, token_id, side, price, size, ladder_key, was_cheap } => {
                        // Populate live_orders for selective refresh
                        live_orders.insert(ladder_key.clone(), (order_id.clone(), price, size));
                        // Populate order_meta for fill attribution
                        order_meta.insert(order_id, PostedOrderMeta {
                            _asset_id: token_id,
                            was_cheap_at_post: was_cheap,
                            _ladder_level: ladder_key.split(':').last()
                                .and_then(|s| s.parse().ok()).unwrap_or(0),
                            posted_price: price,
                        });
                    }
                    ExecutionEvent::ReconcileResult(recon) => {
                        info!("RECONCILE: open_orders={} CLOB_fills: UP={:.1} DN={:.1} | local: UP={:.1} DN={:.1} | drift: UP={:.1} DN={:.1}",
                            recon.open_order_count,
                            recon.up_size_matched, recon.down_size_matched,
                            up_shares, dn_shares,
                            recon.up_size_matched - up_shares, recon.down_size_matched - dn_shares,
                        );
                        if recon.up_size_matched > up_shares + 1.0 || recon.down_size_matched > dn_shares + 1.0 {
                            warn!("RECONCILE DRIFT DETECTED — CLOB has more fills than local state");
                        }
                    }
                    _ => {}
                }
            }

            // Receive REAL fills from user WebSocket (authenticated trade events)
            Some(fill) = fill_rx.recv() => {
                let market = market_rx.borrow().clone();

                // Determine which side this fill is for
                let is_up = fill.asset_id == market.up_token_id;
                let is_dn = fill.asset_id == market.down_token_id;

                if !is_up && !is_dn {
                    // Fill for a different market / old window — ignore
                    continue;
                }

                // Only count BUY fills (we're buy-only)
                if fill.side != "BUY" { continue; }

                fills_this_window += 1;

                if is_up {
                    up_shares += fill.size;
                    up_cost += fill.size * fill.price;
                } else {
                    dn_shares += fill.size;
                    dn_cost += fill.size * fill.price;
                }

                // #4: Classify cheap/exp using order metadata (posted at quote time)
                // Falls back to current book if order_id not found in metadata
                let is_cheap = if let Some(meta) = order_meta.get(&fill.order_id) {
                    meta.was_cheap_at_post
                } else {
                    // Fallback: use current book snapshot
                    let top = pm_top_rx.borrow().clone();
                    match (top.up_ask, top.down_ask) {
                        (Some(ua), Some(da)) => {
                            if is_up { ua < da } else { da < ua }
                        }
                        _ => fill.price < 0.50,
                    }
                };

                if is_cheap {
                    cheap_shares += fill.size;
                    cheap_cost += fill.size * fill.price;
                } else {
                    exp_shares += fill.size;
                    exp_cost += fill.size * fill.price;
                }

                let matched = up_shares.min(dn_shares);
                let total_shares = up_shares + dn_shares;
                let up_pct = if total_shares > 0.0 { up_shares / total_shares * 100.0 } else { 0.0 };
                let cheap_pct = if total_shares > 0.0 { cheap_shares / total_shares * 100.0 } else { 55.0 };
                let side_label = if is_up { "UP" } else { "DN" };
                let ce_label = if is_cheap { "cheap" } else { "exp" };

                // Compute live pair cost on matched shares
                let pair_cost = if matched > 0.0 {
                    let up_avg = up_cost / up_shares.max(0.001);
                    let dn_avg = dn_cost / dn_shares.max(0.001);
                    up_avg + dn_avg
                } else { 0.0 };

                info!(">>> FILL: {} {} {:.1}sh @ {:.2} [{}] | UP={:.0} DN={:.0} ({:.0}%UP) cheap={:.0}% matched={:.0} pcost={:.0}c | fills={}",
                    side_label, ce_label, fill.size, fill.price, fill.status,
                    up_shares, dn_shares, up_pct, cheap_pct, matched, pair_cost * 100.0, fills_this_window);
            }

            // Heartbeat
            _ = heartbeat_interval.tick() => {
                let market = market_rx.borrow().clone();
                let top = pm_top_rx.borrow().clone();
                let matched = up_shares.min(dn_shares);
                let pair_cost = if matched > 0.0 {
                    let up_avg = up_cost / up_shares.max(0.001);
                    let dn_avg = dn_cost / dn_shares.max(0.001);
                    up_avg + dn_avg
                } else { 0.0 };

                info!("HEARTBEAT: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh matched={:.0} pcost={:.0}c | fills={} quotes={} | signal={} | window_active={}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, matched, pair_cost * 100.0,
                    fills_this_window, quotes_posted,
                    last_signal.as_ref().map_or("none".into(), |s| format!("{:.2}", s.score)),
                    window_active,
                );
            }

            // Quote refresh — every 2 seconds, post new bid ladders
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();

                if market.up_token_id.is_empty() { continue; }

                // Window timing
                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                // Not yet time to start
                if elapsed_s < config.start_delay_s as i64 {
                    continue;
                }

                // Past cutoff — cancel and stop
                if elapsed_s >= config.cancel_at_s as i64 {
                    if window_active {
                        let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                            reason: "window_cutoff".into(),
                        }).await;
                        window_active = false;
                        info!(">>> T+{}s — STOPPED QUOTING, holding to resolution", elapsed_s);
                    }
                    continue;
                }

                window_active = true;

                // Only refresh quotes every 2 seconds
                if now_ms - last_quote_ts < quote_refresh_ms {
                    continue;
                }
                last_quote_ts = now_ms;

                // Periodic reconciliation — every 10 seconds
                if now_ms - last_reconcile_ts >= reconcile_interval_ms {
                    last_reconcile_ts = now_ms;
                    let _ = exec_cmd_tx.send(ExecutionCommand::Reconcile {
                        up_token_id: market.up_token_id.clone(),
                        down_token_id: market.down_token_id.clone(),
                        wallet_address: our_wallet.clone(),
                    }).await;
                }

                // Get current book
                let top = pm_top_rx.borrow().clone();
                let (up_bid, up_ask) = match (top.up_bid, top.up_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue, // no prices yet
                };
                let (dn_bid, dn_ask) = match (top.down_bid, top.down_ask) {
                    (Some(b), Some(a)) => (b, a),
                    _ => continue,
                };

                // Staleness watchdog — don't quote off stale prices
                if top.recv_ts_ms > last_price_update_ms {
                    last_price_update_ms = top.recv_ts_ms;
                }
                let price_age_ms = now_ms - last_price_update_ms;
                if price_age_ms > 5000 && last_price_update_ms > 0 {
                    warn!("STALE PRICES: {}ms since last update — skipping quotes", price_age_ms);
                    continue;
                }

                // #3: Update dynamic tick sizes from WS
                if let Some(t) = top.up_tick_size { up_tick_size = t; }
                if let Some(t) = top.down_tick_size { dn_tick_size = t; }

                // ═══ TUNING GATES — risk-shaping layer ═══
                let tune = &config.tune;
                let score = last_signal.as_ref().map_or(0.0, |s| s.score);
                let signal_band = tune.signal_band(score);

                // Compute live pair cost
                let matched = up_shares.min(dn_shares);
                let live_pair_cost = if matched > 0.0 {
                    let up_avg = up_cost / up_shares.max(0.001);
                    let dn_avg = dn_cost / dn_shares.max(0.001);
                    up_avg + dn_avg
                } else { 0.0 };

                // Rolling regime kill switch
                if market_paused {
                    // Check resume conditions
                    if rolling_pnl.len() >= tune.rolling_short_windows {
                        let avg_pnl: f64 = rolling_pnl.iter().sum::<f64>() / rolling_pnl.len() as f64;
                        let avg_pc: f64 = rolling_pair_costs.iter().sum::<f64>() / rolling_pair_costs.len().max(1) as f64;
                        if avg_pnl > tune.rolling_resume_pnl_per_window && avg_pc < tune.rolling_resume_pair_cost {
                            market_paused = false;
                            info!(">>> REGIME RESUMED: rolling_pnl={:.1} pair_cost={:.3}", avg_pnl, avg_pc);
                        }
                    }
                    if market_paused { continue; }
                }

                // Pair-cost gating
                let pc_mode = tune.pair_cost_mode(live_pair_cost, matched);
                match pc_mode {
                    config_v2::PairCostMode::Stop => {
                        if window_active {
                            warn!(">>> PAIR COST {:.3} > {:.3} — STOPPED accumulating", live_pair_cost, tune.pair_cost_strong_only_max);
                        }
                        continue;
                    }
                    config_v2::PairCostMode::StrongOnly => {
                        if signal_band != config_v2::SignalBand::Strong && signal_band != config_v2::SignalBand::Extreme {
                            continue; // only strong/extreme signals allowed
                        }
                    }
                    _ => {} // Full or Reduced — proceed
                }

                // Predicted winner side for residual control
                let pred_winner_up = score > 0.0;
                let winner_shares = if pred_winner_up { up_shares } else { dn_shares };
                let loser_shares = if pred_winner_up { dn_shares } else { up_shares };
                let winner_ratio = winner_shares / loser_shares.max(1.0);

                // Dynamic residual cap
                let max_ratio = tune.max_ratio_for_band(signal_band);
                let favored_capped = winner_ratio >= max_ratio;
                if favored_capped && matched > tune.min_pairs_for_cost_gate {
                    // Don't log every cycle — just when first capped
                }

                // Soft freeze — preserve favorable asymmetry
                let soft_cap = max_ratio * tune.soft_cap_fraction;
                let soft_freeze = winner_ratio >= soft_cap && live_pair_cost >= tune.soft_freeze_pair_cost && matched > tune.min_pairs_for_cost_gate;

                // Compute fair values
                let up_mid = (up_bid + up_ask) / 2.0;
                let k = 0.03;
                let fair_up = (up_mid + k * score).clamp(0.02, 0.98);
                let fair_dn = (1.0 - fair_up).clamp(0.02, 0.98);

                // Edge thresholds — base + extreme-price add-on
                let mut min_edge_l1 = 0.015_f64;
                let mut min_edge_l2 = 0.010_f64;
                let mut min_edge_l3 = 0.005_f64;

                // Determine cheap vs expensive
                let (cheap_side, exp_side) = if up_ask < dn_ask {
                    (Side::Up, Side::Down)
                } else {
                    (Side::Down, Side::Up)
                };
                let exp_ask = if exp_side == Side::Up { up_ask } else { dn_ask };
                let cheap_ask_raw = if cheap_side == Side::Up { up_ask } else { dn_ask };

                // Extreme-price filter
                let mut extreme_mult = 1.0_f64;
                let extreme_hard = exp_ask > tune.extreme_hard_exp_price || cheap_ask_raw < tune.extreme_hard_cheap_price;
                let extreme_reduce = exp_ask > tune.extreme_reduce_exp_price || cheap_ask_raw < tune.extreme_reduce_cheap_price;

                if extreme_hard {
                    // Hard regime: only favored L1 if strong signal
                    if signal_band != config_v2::SignalBand::Strong && signal_band != config_v2::SignalBand::Extreme {
                        continue; // skip this window
                    }
                    extreme_mult = tune.extreme_size_multiplier * 0.5; // very small
                } else if extreme_reduce {
                    extreme_mult = tune.extreme_size_multiplier;
                    min_edge_l1 += tune.extreme_edge_add_cents;
                    min_edge_l2 += tune.extreme_edge_add_cents;
                    min_edge_l3 += tune.extreme_edge_add_cents;
                }

                let min_edges = [min_edge_l1, min_edge_l2, min_edge_l3];

                // Exp-heavy override — shift cheap target in strong windows
                let pred_winner_is_exp = (pred_winner_up && exp_side == Side::Up) ||
                    (!pred_winner_up && exp_side == Side::Down);
                let cheap_target = tune.cheap_target(score, pred_winner_is_exp, Some(live_pair_cost));

                // Signal skew
                let skew = (score * 0.25).clamp(-0.5, 0.5);

                // Inventory ratio from fills
                let total_fill_shares = up_shares + dn_shares;
                let cheap_eff = if cheap_side == Side::Up { up_shares } else { dn_shares };
                let cheap_ratio = if total_fill_shares > 0.0 { cheap_eff / total_fill_shares } else { cheap_target };

                // Inventory correction toward dynamic cheap target
                let mut cheap_size_mult = 1.0_f64;
                let mut exp_size_mult = 1.0_f64;
                if cheap_ratio > cheap_target + 0.05 {
                    cheap_size_mult = 0.7;
                    exp_size_mult = 1.2;
                } else if cheap_ratio < cheap_target - 0.05 {
                    cheap_size_mult = 1.2;
                    exp_size_mult = 0.7;
                }

                // Pair-cost mode adjustments
                match pc_mode {
                    config_v2::PairCostMode::Reduced => {
                        cheap_size_mult *= 0.65;
                        exp_size_mult *= 0.65;
                        // Drop L3 on underfavored side (handled in ladder loop below)
                    }
                    config_v2::PairCostMode::StrongOnly => {
                        cheap_size_mult *= 0.50;
                        exp_size_mult *= 0.50;
                    }
                    _ => {}
                }

                // Soft freeze adjustments — protect favorable asymmetry
                if soft_freeze {
                    // Reduce underfavored side
                    if pred_winner_up {
                        // Winner is UP; reduce DN (loser) quoting
                        if cheap_side == Side::Down { cheap_size_mult *= 0.5; }
                        else { exp_size_mult *= 0.5; }
                    } else {
                        if cheap_side == Side::Up { cheap_size_mult *= 0.5; }
                        else { exp_size_mult *= 0.5; }
                    }
                }

                // Favored side capped — stop quoting favored entirely
                if favored_capped && matched > tune.min_pairs_for_cost_gate {
                    if pred_winner_up {
                        if cheap_side == Side::Up { cheap_size_mult = 0.0; }
                        else { exp_size_mult = 0.0; }
                    } else {
                        if cheap_side == Side::Down { cheap_size_mult = 0.0; }
                        else { exp_size_mult = 0.0; }
                    }
                }

                // Apply extreme-price multiplier
                cheap_size_mult *= extreme_mult;
                exp_size_mult *= extreme_mult;

                // Late window — reduce aggressiveness
                let late_mult = if elapsed_s >= 180 { 0.5 } else { 1.0 };

                // #2: Selective refresh — build new desired quotes first,
                // then only cancel/replace orders whose price actually changed.
                // Orders at the same price keep their queue position.
                let mut desired_quotes: std::collections::HashMap<String, (f64, f64, String, Side)> =
                    std::collections::HashMap::new(); // key -> (price, size, token_id, side)

                // Build bid ladders for BOTH sides
                // Cheap side ladder
                let cheap_token = if cheap_side == Side::Up { &market.up_token_id } else { &market.down_token_id };
                let cheap_bid = if cheap_side == Side::Up { up_bid } else { dn_bid };
                let cheap_ask_price = if cheap_side == Side::Up { up_ask } else { dn_ask };
                let cheap_fair = if cheap_side == Side::Up { fair_up } else { fair_dn };
                // #3: Use dynamic tick size
                let cheap_tick = if cheap_side == Side::Up { up_tick_size } else { dn_tick_size };

                for (level, &base_size) in cheap_ladder.iter().enumerate() {
                    let size = (base_size * cheap_size_mult * late_mult * (1.0 + skew.abs() * 0.3)).round().max(5.0);
                    let price = match level {
                        0 => cheap_bid, // L1: at best bid (NOT bid+tick — that crosses)
                        1 => cheap_bid - cheap_tick,
                        _ => cheap_bid - 2.0 * cheap_tick,
                    };
                    // Round price to tick size
                    let price = (price / cheap_tick).round() * cheap_tick;

                    // Must be strictly below ask and above zero
                    if price <= cheap_tick || price >= cheap_ask_price - cheap_tick { continue; }

                    let edge = cheap_fair - price;
                    if edge < min_edges[level] { continue; }

                    let key = format!("cheap:{}", level);
                    let is_cheap = true;
                    desired_quotes.insert(key, (price, size, cheap_token.clone(), cheap_side));
                }

                // Expensive side ladder
                let exp_token = if exp_side == Side::Up { &market.up_token_id } else { &market.down_token_id };
                let exp_bid = if exp_side == Side::Up { up_bid } else { dn_bid };
                let exp_ask_price = if exp_side == Side::Up { up_ask } else { dn_ask };
                let exp_fair = if exp_side == Side::Up { fair_up } else { fair_dn };
                let exp_tick = if exp_side == Side::Up { up_tick_size } else { dn_tick_size };

                let exp_skew = if (exp_side == Side::Up && score > 0.0) || (exp_side == Side::Down && score < 0.0) {
                    exp_tick
                } else {
                    0.0
                };

                for (level, &base_size) in exp_ladder.iter().enumerate() {
                    let size = (base_size * exp_size_mult * late_mult).round().max(5.0);
                    let price = match level {
                        0 => exp_bid + exp_skew, // L1: at best bid (with signal skew)
                        1 => exp_bid - exp_tick + exp_skew,
                        _ => exp_bid - 2.0 * exp_tick + exp_skew,
                    };
                    let price = (price / exp_tick).round() * exp_tick;

                    // Must be strictly below ask and above zero
                    if price <= exp_tick || price >= exp_ask_price - exp_tick { continue; }

                    let edge = exp_fair - price;
                    if edge < min_edges[level] { continue; }

                    let key = format!("exp:{}", level);
                    desired_quotes.insert(key, (price, size, exp_token.clone(), exp_side));
                }

                // Selective refresh: compare desired vs live orders.
                // Only cancel+repost orders whose price actually changed.
                // Orders at the same price keep their queue position.
                let tick_threshold = cheap_tick.min(exp_tick) * 0.5;
                let mut needs_cancel = false;

                // Check if ANY quote price moved enough to warrant a refresh
                for (key, (desired_price, _desired_size, _, _)) in &desired_quotes {
                    if let Some((_live_id, live_price, _live_size)) = live_orders.get(key) {
                        if (*desired_price - *live_price).abs() > tick_threshold {
                            needs_cancel = true;
                            break;
                        }
                    } else {
                        // New level that didn't exist before
                        needs_cancel = true;
                        break;
                    }
                }

                // Also check if any live orders are no longer desired
                for key in live_orders.keys() {
                    if !desired_quotes.contains_key(key) {
                        needs_cancel = true;
                        break;
                    }
                }

                // If nothing changed, skip this cycle — orders keep resting
                if !needs_cancel && !live_orders.is_empty() {
                    continue;
                }

                // Something changed — cancel all and repost
                if !live_orders.is_empty() {
                    let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll {
                        reason: "selective_refresh".into(),
                    }).await;
                    live_orders.clear();
                    order_meta.clear();
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }

                // Post fresh ladder
                for (key, (price, size, token_id, side)) in &desired_quotes {
                    let is_cheap_order = key.starts_with("cheap");

                    let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                        market_slug: market.slug.clone(),
                        token_id: token_id.clone(),
                        side: *side,
                        price: *price,
                        size: *size,
                        post_only: true,
                        ladder_key: key.clone(),
                        was_cheap: is_cheap_order,
                    }).await;
                    quotes_posted += 1;

                    // Track posted shares for balancing (worst-case: assume all fill)
                    if *side == Side::Up {
                        up_shares_posted += *size;
                    } else {
                        dn_shares_posted += *size;
                    }
                }
            }
        }
    }
}
