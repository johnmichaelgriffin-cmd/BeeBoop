//! Sniper strategy — production version with dynamic sizing.
//!
//! Receives production signals (4-feature stacked score).
//! Validates against risk/market state.
//! Computes dynamic notional from score + confirmations.
//! Emits ExecutionCommand.
//! FOK taker only for now.

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

/// Compute dynamic notional based on score strength and confirmations.
fn compute_notional(score_abs: f64, base: f64, max: f64, all_three_confirm: bool) -> f64 {
    let mut n = base * (0.75 + 0.75 * score_abs.min(2.0));
    if all_three_confirm {
        n *= 1.25;
    }
    n.clamp(base, max)
}

pub async fn run_strategy_task(
    config: Config,
    shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut signal_rx: mpsc::Receiver<Signal>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    info!("strategy: started (FOK taker, base=${:.0}, max=${:.0})",
        config.base_notional, config.max_notional);

    while let Some(signal) = signal_rx.recv().await {
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Double-check state
        if !shared.is_idle() {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("state={:?}", shared.get_state()),
            }).await;
            continue;
        }

        // Session stop loss
        if shared.should_stop(config.session_stop_loss) {
            warn!("strategy: SESSION STOP LOSS (PnL=${:.2})", shared.get_pnl());
            continue;
        }

        // Signal age check
        let signal_age = now_ms - signal.created_ts_ms;
        let max_age = match signal.entry_mode {
            EntryMode::Maker => config.maker_max_signal_age_ms,
            EntryMode::Taker => config.max_signal_age_ms,
        };
        if signal_age > max_age {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("signal_stale age={}ms max={}ms", signal_age, max_age),
            }).await;
            continue;
        }

        // Market info
        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() { continue; }

        // Time to expiry
        let now_s = now_ms / 1000;
        let time_to_expiry = market.window_end_ts - now_s;
        if time_to_expiry < config.min_time_to_expiry_s as i64 {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("expiry_too_close {}s", time_to_expiry),
            }).await;
            continue;
        }

        // Token and price
        let pm_top = pm_top_rx.borrow().clone();
        let (token_id, ask, bid) = match signal.side {
            Side::Up => (&market.up_token_id, pm_top.up_ask, pm_top.up_bid),
            Side::Down => (&market.down_token_id, pm_top.down_ask, pm_top.down_bid),
        };

        let Some(ask_price) = ask else { continue };
        let bid_price = bid.unwrap_or(0.0);

        // Spread check
        let spread_cents = (ask_price - bid_price) * 100.0;
        if spread_cents > config.max_spread_cents {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("spread {:.1}c > {:.1}c", spread_cents, config.max_spread_cents),
            }).await;
            continue;
        }

        // Max entry price
        if ask_price > config.max_entry_price {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("ask {:.0}c > {:.0}c", ask_price * 100.0, config.max_entry_price * 100.0),
            }).await;
            continue;
        }

        // Dynamic sizing
        let all_confirm = signal.confirms_r2s && signal.confirms_basis && signal.confirms_obi;
        let notional = compute_notional(
            signal.score.abs(),
            config.base_notional,
            config.max_notional,
            all_confirm,
        );

        let side_label = if signal.side == Side::Up { "UP" } else { "DN" };
        let shares_est = (notional / ask_price) as u32;

        info!(">>> ENTRY FOK: {} ${:.0} ~{}sh @ {:.0}c | score={:.2} fast={:.1}bps basis={:.1}bps conf={}/3",
            side_label, notional, shares_est, ask_price * 100.0,
            signal.score, signal.fast_move_bps, signal.basis_bps,
            signal.confirms_r2s as u8 + signal.confirms_basis as u8 + signal.confirms_obi as u8,
        );

        // Transition to Entering
        shared.set_state(StrategyState::Entering);

        let _ = log_tx.send(LogEvent::EntrySent {
            ts_ms: now_ms,
            side: signal.side,
            token_id: token_id.clone(),
            notional,
            move_bps: signal.fast_move_bps,
        }).await;

        let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
            market_slug: market.slug.clone(),
            token_id: token_id.clone(),
            side: signal.side,
            max_price: (ask_price + 0.02).min(config.max_entry_price),
            notional,
            signal,
        }).await;
    }
}
