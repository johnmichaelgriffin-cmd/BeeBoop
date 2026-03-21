//! Position manager — processes execution events, tracks P&L, manages exits.
//!
//! On entry fill: creates position, sets state to InPosition.
//! On entry reject: resets to Idle.
//! While InPosition: monitors bid for take-profit, triggers exit on timeout.
//! On exit fill: computes P&L, sets cooldown, resets to Idle.
//! On exit reject: holds to resolution (never panic-sell).

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

pub async fn run_position_manager_task(
    config: Config,
    shared: SharedState,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    mut exec_evt_rx: mpsc::Receiver<ExecutionEvent>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    info!("position_manager: started (tp={:.0}%, max_hold={}ms, cooldown={}ms)",
        config.take_profit_pct * 100.0, config.max_hold_ms, config.cooldown_ms);

    // Process execution events + poll for exit conditions
    let mut exit_attempts = 0u32;
    let mut position_held = false;

    loop {
        tokio::select! {
            // Handle execution events
            evt = exec_evt_rx.recv() => {
                let Some(evt) = evt else { break };
                match evt {
                    ExecutionEvent::EntryFilled {
                        market_slug, token_id, side, filled_price, filled_size,
                        notional, order_id, signal, ack_ts_ms, ..
                    } => {
                        let pos = Position {
                            market_slug: market_slug.clone(),
                            token_id: token_id.clone(),
                            side,
                            entry_ts_ms: ack_ts_ms,
                            entry_price: filled_price,
                            size: filled_size,
                            notional,
                            signal_bps: signal.move_bps,
                            signal_confidence: signal.confidence,
                        };

                        info!(">>> POSITION OPENED: {} {:.0}sh @ {:.0}c | target sell @ {:.0}c (+{:.0}%)",
                            if side == Side::Up { "UP" } else { "DN" },
                            filled_size, filled_price * 100.0,
                            filled_price * (1.0 + config.take_profit_pct) * 100.0,
                            config.take_profit_pct * 100.0,
                        );

                        shared.set_position(pos);
                        shared.set_state(StrategyState::InPosition);
                        exit_attempts = 0;
                        position_held = true;

                        // Wait for on-chain settlement before allowing sells
                        // CLOB returns Matched but tokens aren't available until Mined
                        info!(">>> WAITING 6s for settlement...");
                        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                        info!(">>> Settlement wait complete — monitoring for exit");
                    }

                    ExecutionEvent::EntryRejected { reason, .. } => {
                        warn!(">>> ENTRY REJECTED: {} — back to Idle", reason);
                        shared.set_state(StrategyState::Idle);
                    }

                    ExecutionEvent::ExitFilled {
                        market_slug, side, filled_price, filled_size, reason, ack_ts_ms, ..
                    } => {
                        if let Some(pos) = shared.get_position() {
                            let pnl = (filled_price - pos.entry_price) * filled_size;
                            let hold_ms = ack_ts_ms - pos.entry_ts_ms;

                            info!(">>> POSITION CLOSED: {} | entry={:.0}c exit={:.0}c | PnL=${:.2} | hold={}ms | {}",
                                if side == Side::Up { "UP" } else { "DN" },
                                pos.entry_price * 100.0, filled_price * 100.0,
                                pnl, hold_ms, reason,
                            );

                            shared.add_pnl(pnl);

                            let _ = log_tx.send(LogEvent::PositionClosed {
                                ts_ms: ack_ts_ms,
                                market_slug,
                                side,
                                entry_price: pos.entry_price,
                                exit_price: filled_price,
                                size: filled_size,
                                pnl_usdc: pnl,
                                hold_ms,
                                reason,
                            }).await;
                        }

                        shared.clear_position();
                        shared.set_cooldown(chrono::Utc::now().timestamp_millis() + config.cooldown_ms);
                        shared.set_state(StrategyState::Cooldown);
                        position_held = false;
                        exit_attempts = 0;

                        // Cooldown timer — return to Idle after cooldown_ms
                        let cooldown = config.cooldown_ms as u64;
                        let shared_clone = shared.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(cooldown)).await;
                            if *shared_clone.strategy_state.read() == StrategyState::Cooldown {
                                shared_clone.set_state(StrategyState::Idle);
                                info!(">>> COOLDOWN EXPIRED — back to Idle");
                            }
                        });
                    }

                    ExecutionEvent::ExitRejected { reason, .. } => {
                        exit_attempts += 1;
                        if exit_attempts >= 3 {
                            warn!(">>> SELL FAILED 3x: {} — holding to resolution", reason);
                            // Don't reset to Idle — stay InPosition
                            // The tokens will resolve at window end
                            // Sweeper will redeem winners

                            if let Some(pos) = shared.get_position() {
                                let _ = log_tx.send(LogEvent::HoldToResolution {
                                    ts_ms: chrono::Utc::now().timestamp_millis(),
                                    market_slug: pos.market_slug.clone(),
                                    side: pos.side,
                                    entry_price: pos.entry_price,
                                    size: pos.size,
                                    reason: format!("sell_failed_3x: {}", reason),
                                }).await;
                            }

                            // Reset to Idle so we can trade the next window
                            shared.clear_position();
                            shared.set_cooldown(chrono::Utc::now().timestamp_millis() + config.cooldown_ms);
                            shared.set_state(StrategyState::Cooldown);
                            position_held = false;
                        } else {
                            warn!(">>> SELL FAILED ({}/3): {} — retry in 2s", exit_attempts, reason);
                        }
                    }

                    ExecutionEvent::CancelAck { .. } => {}
                }
            }

            // Check exit conditions every 100ms while in position
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)), if position_held => {
                if shared.get_state() != StrategyState::InPosition {
                    continue;
                }

                let Some(pos) = shared.get_position() else { continue };
                let now_ms = chrono::Utc::now().timestamp_millis();
                let hold_ms = now_ms - pos.entry_ts_ms;

                // Get current bid
                let pm_top = pm_top_rx.borrow().clone();
                let current_bid = match pos.side {
                    Side::Up => pm_top.up_bid,
                    Side::Down => pm_top.down_bid,
                };

                if let Some(bid) = current_bid {
                    let profit_pct = (bid - pos.entry_price) / pos.entry_price;

                    // Take profit
                    if profit_pct >= config.take_profit_pct {
                        info!(">>> EXIT: TARGET HIT {:.1}% | entry={:.0}c bid={:.0}c | hold={}ms",
                            profit_pct * 100.0, pos.entry_price * 100.0, bid * 100.0, hold_ms);

                        shared.set_state(StrategyState::Exiting);
                        let _ = exec_cmd_tx.send(ExecutionCommand::ExitTaker {
                            market_slug: pos.market_slug.clone(),
                            token_id: pos.token_id.clone(),
                            side: pos.side,
                            shares: pos.size,
                            min_price: pos.entry_price, // floor = entry, never sell at loss
                            reason: format!("take_profit {:.1}%", profit_pct * 100.0),
                        }).await;
                        continue;
                    }
                }

                // Time stop — hold to resolution, don't panic sell
                if hold_ms >= config.max_hold_ms {
                    info!(">>> TIME STOP: hold={}ms >= {}ms — holding to resolution",
                        hold_ms, config.max_hold_ms);

                    if let Some(pos) = shared.get_position() {
                        let _ = log_tx.send(LogEvent::HoldToResolution {
                            ts_ms: now_ms,
                            market_slug: pos.market_slug.clone(),
                            side: pos.side,
                            entry_price: pos.entry_price,
                            size: pos.size,
                            reason: format!("time_stop {}ms", hold_ms),
                        }).await;
                    }

                    // Reset to allow next trade
                    shared.clear_position();
                    shared.set_cooldown(now_ms + config.cooldown_ms);
                    shared.set_state(StrategyState::Cooldown);
                    position_held = false;
                }
            }
        }
    }
}
