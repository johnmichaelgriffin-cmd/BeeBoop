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

fn sign_matches(side: Side, x: f64) -> bool {
    match side {
        Side::Up => x > 0.0,
        Side::Down => x < 0.0,
    }
}

/// Compute dynamic notional based on score strength + feature alignment bonus.
fn compute_notional(signal: &Signal, base: f64, max: f64) -> f64 {
    let mut n = base * (0.8 + 0.8 * signal.score.abs().min(2.0));

    // Aligned bonus: r2s, basis_dev, dbasis all agree AND OBI not strongly opposite
    let obi_not_opposite = match signal.side {
        Side::Up => signal.obi_ema >= -0.50,
        Side::Down => signal.obi_ema <= 0.50,
    };
    let aligned = sign_matches(signal.side, signal.r2000_bps)
        && sign_matches(signal.side, signal.basis_dev_bps)
        && sign_matches(signal.side, signal.dbasis_bps)
        && obi_not_opposite;

    if aligned {
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

    let mut window_locked_side: Option<Side> = None;
    let mut last_window_ts: i64 = 0;

    while let Some(signal) = signal_rx.recv().await {
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Reset direction lock on new window
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                if window_locked_side.is_some() {
                    info!("strategy: NEW WINDOW — clearing direction lock");
                }
                window_locked_side = None;
                last_window_ts = market.window_start_ts;
            }
        }

        // Direction lock: only trade same side as first signal in this window
        if let Some(locked) = window_locked_side {
            if signal.side != locked {
                let _ = log_tx.send(LogEvent::TradeSkipped {
                    ts_ms: now_ms,
                    reason: format!("wrong_direction: signal={:?} locked={:?}", signal.side, locked),
                }).await;
                continue;
            }
        }

        // Cheap-side gate: only buy when signal side is the cheap token (<50c)
        // This ensures we always load the cheap side first
        {
            let pm_top = pm_top_rx.borrow().clone();
            let signal_ask = match signal.side {
                Side::Up => pm_top.up_ask,
                Side::Down => pm_top.down_ask,
            };
            if let Some(ask) = signal_ask {
                if ask >= 0.50 {
                    let _ = log_tx.send(LogEvent::TradeSkipped {
                        ts_ms: now_ms,
                        reason: format!("cheap_gate: {:?} ask={:.0}c >= 50c (not cheap side)", signal.side, ask * 100.0),
                    }).await;
                    continue;
                }
            }
        }

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

        // Min entry price — don't buy tokens below 15c (market already decided)
        if ask_price < config.min_entry_price {
            let _ = log_tx.send(LogEvent::TradeSkipped {
                ts_ms: now_ms,
                reason: format!("ask {:.0}c < {:.0}c floor", ask_price * 100.0, config.min_entry_price * 100.0),
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

        // Dynamic sizing — corrected production version
        let notional = compute_notional(&signal, config.base_notional, config.max_notional);

        let side_label = if signal.side == Side::Up { "UP" } else { "DN" };
        let shares_est = (notional / ask_price) as u32;

        info!(">>> ENTRY FOK: {} ${:.0} ~{}sh @ {:.0}c | score={:.3} fast={:.1}bps r2s={:.1}bps bdev={:.1}bps db={:.1}bps obi={:.2}",
            side_label, notional, shares_est, ask_price * 100.0,
            signal.score, signal.fast_move_bps, signal.r2000_bps, signal.basis_dev_bps, signal.dbasis_bps, signal.obi_ema,
        );

        // Lock direction for this window
        if window_locked_side.is_none() {
            info!(">>> DIRECTION LOCKED: {:?} for this window", signal.side);
            window_locked_side = Some(signal.side);
        }

        // Transition to Entering
        shared.set_state(StrategyState::Entering);

        let _ = log_tx.send(LogEvent::EntrySent {
            ts_ms: now_ms,
            side: signal.side,
            token_id: token_id.clone(),
            notional,
            score: signal.score,
            fast_move_bps: signal.fast_move_bps,
            basis_dev_bps: signal.basis_dev_bps,
        }).await;

        let _ = exec_cmd_tx.send(ExecutionCommand::EnterTaker {
            market_slug: market.slug.clone(),
            token_id: token_id.clone(),
            side: signal.side,
            max_price: ask_price, // used as ask_price for lockprofit formula: floor(20 * ask) shares @ 99c
            notional,
            signal,
        }).await;
    }
}
