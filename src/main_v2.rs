//! BeeBoop v2 — Event-driven sniper bot for Polymarket BTC Up/Down.
//!
//! Architecture: channel-based Tokio tasks, no shared SDK client locks.
//! Strategy: FOK taker scalping on Binance→Chainlink oracle lag.

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

// New v2 modules
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
    // Install TLS crypto provider FIRST — before any WebSocket/HTTP connections
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .init();

    // Load env
    dotenvy::dotenv().ok();

    // Parse mode from CLI
    let args: Vec<String> = std::env::args().collect();
    let mode = match args.get(1).map(|s| s.as_str()) {
        Some("live") => BotMode::Live,
        _ => BotMode::DryRun,
    };

    // Build config
    let mut config = Config::default();
    config.mode = mode;
    config.load_credentials();

    info!("=== BeeBoop v2.1.0 — Production Signal Engine ===");
    info!("Mode: {:?}", config.mode);
    info!("Market: {}", config.market);
    info!("Signal: trigger={:.2}bps, maker={:.2}, taker={:.2}",
        config.trigger_min_fast_move_bps, config.maker_score_min, config.taker_score_min);
    info!("Exit: TP=+{:.0}c, SL=-{:.0}c, hold={}ms, cooldown={}ms",
        config.take_profit_cents, config.stop_loss_cents, config.max_hold_ms, config.cooldown_ms);
    info!("Sizing: ${:.0}-${:.0}", config.base_notional, config.max_notional);
    info!("Risk: stop-loss=${:.0}, spread<{:.0}c, ask={:.0}c-{:.0}c",
        config.session_stop_loss, config.max_spread_cents, config.min_entry_price * 100.0, config.max_entry_price * 100.0);

    if config.mode == BotMode::Live && !config.has_credentials() {
        tracing::error!("Live mode requires POLY_PRIVATE_KEY, POLY_API_KEY, POLY_SECRET, POLY_PASSPHRASE in .env");
        std::process::exit(1);
    }

    let shared = SharedState::new();

    // ── Broadcast channels (hot market data) ────────────────────
    let (binance_tx, _) = broadcast::channel::<BinanceTick>(10_000);
    let (polymarket_tx, _) = broadcast::channel::<PolymarketTop>(10_000);
    let (chainlink_tx, _) = broadcast::channel::<ChainlinkTick>(10_000);

    // ── Watch channels (latest snapshots) ───────────────────────
    let (market_watch_tx, market_watch_rx) = watch::channel::<MarketDescriptor>(MarketDescriptor::default());
    let (pm_top_watch_tx, pm_top_watch_rx) = watch::channel::<PolymarketTop>(PolymarketTop::default());
    let (chainlink_watch_tx, chainlink_watch_rx) = watch::channel::<Option<ChainlinkTick>>(None);

    // ── Command/event channels ──────────────────────────────────
    let (signal_tx, signal_rx) = mpsc::channel::<Signal>(1_000);
    let (exec_cmd_tx, exec_cmd_rx) = mpsc::channel::<ExecutionCommand>(1_000);
    let (exec_evt_tx, exec_evt_rx) = mpsc::channel::<ExecutionEvent>(1_000);
    let (log_tx, log_rx) = mpsc::channel::<LogEvent>(10_000);

    // ── 1. Market discovery ─────────────────────────────────────
    tokio::spawn(market::discovery::run_market_discovery_task(
        config.clone(),
        market_watch_tx,
        log_tx.clone(),
    ));

    // ── 2. Binance feed ─────────────────────────────────────────
    tokio::spawn(feeds::binance::run_binance_feed_task(
        binance_tx.clone(),
        log_tx.clone(),
    ));

    // ── 3. Polymarket price feed (REST polling) ─────────────────
    tokio::spawn(feeds::polymarket_ws::run_polymarket_ws_task(
        market_watch_rx.clone(),
        polymarket_tx,
        pm_top_watch_tx,
        log_tx.clone(),
    ));

    // ── 4. RTDS / Chainlink ─────────────────────────────────────
    tokio::spawn(feeds::rtds::run_rtds_task(
        chainlink_tx,
        chainlink_watch_tx,
        log_tx.clone(),
    ));

    // ── 5. Signal engine ────────────────────────────────────────
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

    // ── 6. Strategy ─────────────────────────────────────────────
    tokio::spawn(strategy_v2::sniper::run_strategy_task(
        config.clone(),
        shared.clone(),
        market_watch_rx.clone(),
        pm_top_watch_rx.clone(),
        signal_rx,
        exec_cmd_tx.clone(),
        log_tx.clone(),
    ));

    // ── 7. Executor (OWNS the SDK client — no locks) ────────────
    tokio::spawn(execution_v2::executor::run_executor_task(
        config.clone(),
        exec_cmd_rx,
        exec_evt_tx,
        log_tx.clone(),
    ));

    // ── 8. Position manager ─────────────────────────────────────
    tokio::spawn(portfolio::position_manager::run_position_manager_task(
        config.clone(),
        shared.clone(),
        market_watch_rx.clone(),
        pm_top_watch_rx,
        exec_evt_rx,
        exec_cmd_tx,
        log_tx.clone(),
    ));

    // ── 9. Trade logger ─────────────────────────────────────────
    tokio::spawn(logging::trade_log::run_trade_log_task(
        config.clone(),
        log_rx,
    ));

    info!("All tasks running. Press Ctrl+C to stop.");

    // Wait for shutdown
    tokio::signal::ctrl_c().await?;
    info!("Shutting down... Session PnL: ${:.2}", shared.get_pnl());

    Ok(())
}
