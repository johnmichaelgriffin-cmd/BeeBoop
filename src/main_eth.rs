//! BooBeETH — Event-driven sniper bot for Polymarket ETH Up/Down.
//!
//! Same architecture as BeeBoop v2 but for ETH markets.
//! 10sh per snipe, 50 snipes per window.

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
use tracing::info;

use config_v2::Config;
use state::SharedState;
use types_v2::*;

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

    // ETH config overrides
    let mut config = Config::default();
    config.mode = mode;
    config.market = "eth".to_string();
    config.share_base = 10.0;           // 10sh per snipe (BTC=25)
    config.max_pairs_per_window = 50;   // 50 snipes per window (BTC=40)
    config.load_credentials();

    info!("=== BooBeETH v1.0.0 — ETH Sniper ===");
    info!("Mode: {:?}", config.mode);
    info!("Market: {}", config.market);
    info!("Signal: trigger={:.2}bps, maker={:.2}, taker={:.2}",
        config.trigger_min_fast_move_bps, config.maker_score_min, config.taker_score_min);
    info!("Exit: TP=+{:.0}c, SL=-{:.0}c, hold={}ms, cooldown={}ms",
        config.take_profit_cents, config.stop_loss_cents, config.max_hold_ms, config.cooldown_ms);
    info!("Risk: stop-loss=${:.0}, spread<{:.0}c, ask={:.0}c-{:.0}c",
        config.session_stop_loss, config.max_spread_cents, config.min_entry_price * 100.0, config.max_entry_price * 100.0);

    if config.mode == BotMode::Live && !config.has_credentials() {
        tracing::error!("Live mode requires POLY_PRIVATE_KEY, POLY_API_KEY, POLY_SECRET, POLY_PASSPHRASE in .env");
        std::process::exit(1);
    }

    let shared = SharedState::new();

    // ── Channels ──
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

    // 1. Market discovery (ETH markets)
    tokio::spawn(market::discovery::run_market_discovery_task(
        config.clone(),
        market_watch_tx,
        log_tx.clone(),
    ));

    // 2. Binance feed — ETHUSDT instead of BTCUSDT
    let binance_tx_clone = binance_tx.clone();
    let log_tx_clone = log_tx.clone();
    tokio::spawn(async move {
        feeds::binance::run_binance_feed_task_for(
            "ethusdt",
            binance_tx_clone,
            log_tx_clone,
        ).await;
    });

    // 3a. Polymarket WS price feed
    let pm_top_watch_tx_ws = pm_top_watch_tx.clone();
    tokio::spawn(feeds::market_ws::run_market_ws_task(
        market_watch_rx.clone(),
        polymarket_tx.clone(),
        pm_top_watch_tx_ws,
        log_tx.clone(),
    ));

    // 3b. Polymarket REST fallback
    tokio::spawn(feeds::polymarket_ws::run_polymarket_ws_task(
        market_watch_rx.clone(),
        polymarket_tx,
        pm_top_watch_tx,
        log_tx.clone(),
    ));

    // 4. RTDS — ETH/USD instead of BTC/USD
    let log_tx_rtds = log_tx.clone();
    tokio::spawn(async move {
        feeds::rtds::run_rtds_task_for(
            "eth/usd",
            chainlink_tx,
            chainlink_watch_tx,
            log_tx_rtds,
        ).await;
    });

    // 5. Signal engine
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

    // 6. Strategy
    tokio::spawn(strategy_v2::sniper::run_strategy_task(
        config.clone(),
        shared.clone(),
        market_watch_rx.clone(),
        pm_top_watch_rx.clone(),
        signal_rx,
        exec_cmd_tx.clone(),
        log_tx.clone(),
    ));

    // 7. Executor
    tokio::spawn(execution_v2::executor::run_executor_task(
        config.clone(),
        exec_cmd_rx,
        exec_evt_tx,
        log_tx.clone(),
    ));

    // 8. Position manager
    tokio::spawn(portfolio::position_manager::run_position_manager_task(
        config.clone(),
        shared.clone(),
        market_watch_rx.clone(),
        pm_top_watch_rx,
        exec_evt_rx,
        exec_cmd_tx,
        log_tx.clone(),
    ));

    // 9. Trade logger
    tokio::spawn(logging::trade_log::run_trade_log_task(
        config.clone(),
        log_rx,
    ));

    info!("All tasks running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    info!("Shutting down... Session PnL: ${:.2}", shared.get_pnl());

    Ok(())
}
