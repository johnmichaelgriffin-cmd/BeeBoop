//! BeeBoop Vidarx v4 — XRP 5M binary markets.
//!
//! Strategy: FOK exp side at ask, 5sh every 25s, 10 rounds from T+10s.
//! No cheap side — exp only, hold to resolution.
//!
//! Usage: beeboop-v3-xrp live

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
use tracing::info;
use tokio::sync::{broadcast, mpsc, watch};

use config_v2::Config;
use state::SharedState;
use types_v2::*;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("crypto provider");

    tracing_subscriber::fmt()
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .init();

    let mode = std::env::args().nth(1).unwrap_or("dryrun".into());
    let mut config = Config::default();
    config.market = "xrp".to_string();

    config.start_delay_s = 5;
    config.cancel_at_s = 240;
    config.cooldown_ms = 500;
    config.max_pairs_per_window = 999;
    config.share_base = 20.0;

    if mode == "live" {
        config.mode = BotMode::Live;
        dotenvy::dotenv().ok();
        config.load_credentials();
    }

    info!("=== BEEBOOP VIDARX v4 — XRP 5M ===");
    info!("Mode: {:?}", config.mode);
    info!("Strategy: FOK exp at ask, 5sh every 25s, 10 rounds from T+10s — exp only");

    let shared = SharedState::new();

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

    tokio::spawn(market::discovery::run_market_discovery_task(
        config.clone(), market_watch_tx, log_tx.clone(),
    ));

    tokio::spawn(feeds::binance::run_binance_feed_task_for(
        "xrpusdt", binance_tx.clone(), log_tx.clone(),
    ));

    tokio::spawn(feeds::market_ws::run_market_ws_task(
        market_watch_rx.clone(), polymarket_tx, pm_top_watch_tx.clone(), log_tx.clone(),
    ));

    tokio::spawn(feeds::rtds::run_rtds_task_for(
        "xrp/usd", chainlink_tx, chainlink_watch_tx, log_tx.clone(),
    ));

    let (fill_tx, fill_rx) = mpsc::channel::<feeds::user_ws::UserFillEvent>(1_000);
    if config.has_credentials() {
        let api_key = config.poly_api_key.clone();
        let api_secret = config.poly_api_secret.clone();
        let api_passphrase = config.poly_api_passphrase.clone();
        tokio::spawn(feeds::user_ws::run_user_ws_task(
            api_key, api_secret, api_passphrase,
            market_watch_rx.clone(), fill_tx, 600,
        ));
    } else {
        info!("No credentials — user WS disabled");
    }

    tokio::spawn(signals::signal_engine::run_signal_engine_task(
        config.clone(), shared.clone(),
        binance_tx.subscribe(), market_watch_rx.clone(),
        pm_top_watch_rx.clone(), chainlink_watch_rx,
        signal_tx, log_tx.clone(),
    ));

    tokio::spawn(execution_v2::executor::run_executor_task(
        config.clone(), exec_cmd_rx, exec_evt_tx, log_tx.clone(),
    ));

    tokio::spawn(run_vidarx_strategy(
        config.clone(), shared.clone(),
        market_watch_rx.clone(), pm_top_watch_rx,
        signal_rx, exec_cmd_tx, exec_evt_rx, fill_rx,
        log_tx.clone(),
    ));

    tokio::spawn(logging::trade_log::run_trade_log_task(config.clone(), log_rx));

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");
    Ok(())
}

async fn run_vidarx_strategy(
    _config: Config,
    _shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut signal_rx: mpsc::Receiver<Signal>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    mut exec_evt_rx: mpsc::Receiver<ExecutionEvent>,
    mut fill_rx: mpsc::Receiver<feeds::user_ws::UserFillEvent>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    info!("VIDARX v4 XRP: FOK exp at ask, 5sh every 25s — exp only, no cheap side");

    let mut last_window_ts: i64 = 0;
    let mut up_shares: f64 = 0.0;
    let mut dn_shares: f64 = 0.0;
    let mut up_cost: f64 = 0.0;
    let mut dn_cost: f64 = 0.0;
    let mut fills_this_window: u32 = 0;

    let total_rounds: u32 = 10;
    let round_size: f64 = 5.0;
    let mut rounds_posted: u32 = 0;

    let mut latest_obi: f64 = 0.0;
    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                if last_window_ts > 0 && (up_shares > 0.0 || dn_shares > 0.0) {
                    info!(">>> WINDOW COMPLETE: UP {:.0}sh (${:.2}) DN {:.0}sh (${:.2}) | fills={} rounds={}",
                        up_shares, up_cost, dn_shares, dn_cost, fills_this_window, rounds_posted);
                }
                up_shares = 0.0; dn_shares = 0.0;
                up_cost = 0.0; dn_cost = 0.0;
                fills_this_window = 0;
                rounds_posted = 0;
                last_window_ts = market.window_start_ts;
                let _ = exec_cmd_tx.send(ExecutionCommand::CancelAll { reason: "new_window".into() }).await;
                info!(">>> NEW WINDOW {} — {} rounds × {:.0}sh EXP-only every 25s from T+10s",
                    market.window_start_ts, total_rounds, round_size);
            }
        }

        tokio::select! {
            Some(sig) = signal_rx.recv() => {
                latest_obi = sig.obi_ema;
            }

            Some(_evt) = exec_evt_rx.recv() => {}

            Some(fill) = fill_rx.recv() => {
                let market = market_rx.borrow().clone();
                let is_up = fill.asset_id == market.up_token_id;
                let is_dn = fill.asset_id == market.down_token_id;
                if !is_up && !is_dn { continue; }
                if fill.side != "BUY" { continue; }

                fills_this_window += 1;
                if is_up { up_shares += fill.size; up_cost += fill.size * fill.price; }
                else      { dn_shares += fill.size; dn_cost += fill.size * fill.price; }

                info!(">>> FILL: {} {:.1}sh @ {:.0}c | UP={:.0} DN={:.0} | fills={}",
                    if is_up { "UP" } else { "DN" }, fill.size, fill.price * 100.0,
                    up_shares, dn_shares, fills_this_window);
            }

            _ = heartbeat_interval.tick() => {
                let top = pm_top_rx.borrow().clone();
                info!("HEARTBEAT: UP ask={} DN ask={} | UP={:.0}sh DN={:.0}sh | rounds={}/{} | obi={:.2}",
                    top.up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    top.down_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                    up_shares, dn_shares, rounds_posted, total_rounds, latest_obi);
            }

            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let market = market_rx.borrow().clone();
                if market.up_token_id.is_empty() { continue; }

                let elapsed_s = (now_ms / 1000) - market.window_start_ts;

                let rounds_due = if elapsed_s < 10 {
                    0u32
                } else {
                    (((elapsed_s - 10) / 25) + 1).min(total_rounds as i64) as u32
                };
                if rounds_posted >= rounds_due { continue; }

                let top = pm_top_rx.borrow().clone();
                let up_ask = match top.up_ask { Some(a) => a, None => continue };
                let dn_ask = match top.down_ask { Some(a) => a, None => continue };

                // FOK the expensive side (higher ask)
                let exp_is_up = up_ask >= dn_ask;
                let (exp_token, exp_side, exp_price, exp_label) = if exp_is_up {
                    (market.up_token_id.clone(), Side::Up, up_ask, "UP")
                } else {
                    (market.down_token_id.clone(), Side::Down, dn_ask, "DN")
                };

                let round_num = rounds_posted + 1;
                info!(">>> ROUND {}/{}: FOK {} EXP@{:.0}c (ask) {:.0}sh | T+{}s",
                    round_num, total_rounds, exp_label, exp_price * 100.0, round_size, elapsed_s);
                let _ = exec_cmd_tx.send(ExecutionCommand::PostMakerBid {
                    market_slug: market.slug.clone(),
                    token_id: exp_token,
                    side: exp_side,
                    price: exp_price,
                    size: round_size,
                    post_only: false,
                    ladder_key: format!("exp:fok:r{}", round_num),
                    was_cheap: false,
                }).await;
                rounds_posted += 1;
            }
        }
    }
}
