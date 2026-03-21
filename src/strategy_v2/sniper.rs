//! Sniper strategy — FOK taker only.
//!
//! Receives signals, validates against risk/market state,
//! emits ExecutionCommand::EnterTaker.
//! No maker logic, no GTC, no two-sided quoting.

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

pub async fn run_strategy_task(
    config: Config,
    shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut signal_rx: mpsc::Receiver<Signal>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    info!("strategy: started (FOK taker only, notional=${:.0})", config.base_notional);

    while let Some(signal) = signal_rx.recv().await {
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Double-check state (signal engine should already filter, but be safe)
        if !shared.is_idle() {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("state={:?}", shared.get_state()),
            }).await;
            continue;
        }

        // Check stop loss
        if shared.should_stop(config.session_stop_loss) {
            warn!("strategy: SESSION STOP LOSS hit (PnL=${:.2})", shared.get_pnl());
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("session_stop_loss pnl={:.2}", shared.get_pnl()),
            }).await;
            continue;
        }

        // Check signal age
        let signal_age = now_ms - signal.created_ts_ms;
        if signal_age > config.max_signal_age_ms {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("signal_too_old age={}ms", signal_age),
            }).await;
            continue;
        }

        // Get market info
        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() {
            continue;
        }

        // Check time to expiry
        let now_s = now_ms / 1000;
        let time_to_expiry = market.window_end_ts - now_s;
        if time_to_expiry < config.min_time_to_expiry_s as i64 {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("too_close_to_expiry {}s", time_to_expiry),
            }).await;
            continue;
        }

        // Get token and price info
        let pm_top = pm_top_rx.borrow().clone();
        let (token_id, ask, bid) = match signal.side {
            Side::Up => (
                &market.up_token_id,
                pm_top.up_ask,
                pm_top.up_bid,
            ),
            Side::Down => (
                &market.down_token_id,
                pm_top.down_ask,
                pm_top.down_bid,
            ),
        };

        let Some(ask_price) = ask else {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: "no_ask".to_string(),
            }).await;
            continue;
        };

        let bid_price = bid.unwrap_or(0.0);

        // Spread check
        let spread_cents = (ask_price - bid_price) * 100.0;
        if spread_cents > config.max_spread_cents {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("spread_too_wide {:.1}c", spread_cents),
            }).await;
            continue;
        }

        // Max entry price check
        if ask_price > config.max_entry_price {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("ask_too_high {:.0}c", ask_price * 100.0),
            }).await;
            continue;
        }

        // All checks passed — FIRE!
        let side_label = if signal.side == Side::Up { "UP" } else { "DN" };
        info!(">>> ENTRY FOK: {} ${:.2} ~{}sh @ {:.0}c | move={:.1}bps basis={:.1}bps",
            side_label,
            config.base_notional,
            (config.base_notional / ask_price) as u32,
            ask_price * 100.0,
            signal.move_bps,
            signal.basis_bps,
        );

        // Transition to Entering BEFORE sending command
        shared.set_state(StrategyState::Entering);

        let _ = log_tx.send(LogEvent::EntrySent {
            ts_ms: now_ms,
            side: signal.side,
            token_id: token_id.clone(),
            notional: config.base_notional,
            move_bps: signal.move_bps,
        }).await;

        let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
            market_slug: market.slug.clone(),
            token_id: token_id.clone(),
            side: signal.side,
            max_price: (ask_price + 0.02).min(config.max_entry_price),
            notional: config.base_notional,
            signal,
        }).await;
    }
}
