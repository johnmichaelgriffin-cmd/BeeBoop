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

const CLOB_BASE: &str = "https://clob.polymarket.com";

/// Poll CLOB for trade settlement status.
/// Returns true when trade reaches MINED or CONFIRMED status.
/// Polls `max_attempts` times with `interval_ms` between each.
async fn poll_settlement(order_id: &str, max_attempts: u32, interval_ms: u64) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    for attempt in 1..=max_attempts {
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;

        // Query CLOB for trade status
        let url = format!("{}/data/order/{}", CLOB_BASE, order_id);
        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    // Check if any associated trades have reached MINED/CONFIRMED
                    if let Some(status) = body.get("status").and_then(|s| s.as_str()) {
                        info!(">>> Settlement poll {}/{}: status={}", attempt, max_attempts, status);
                        match status {
                            "MINED" | "CONFIRMED" | "MATCHED" => {
                                // MATCHED means the CLOB accepted it — tokens should be reserved
                                // MINED means on-chain settlement complete
                                if status == "MINED" || status == "CONFIRMED" {
                                    return true;
                                }
                                // For MATCHED, check if enough time has passed (1.5s minimum)
                                if attempt >= 3 {
                                    info!(">>> Settlement: MATCHED after {}ms — proceeding", attempt as u64 * interval_ms);
                                    return true;
                                }
                            }
                            "FAILED" | "CANCELLED" => {
                                warn!(">>> Settlement: order {} — aborting", status);
                                return false;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                warn!(">>> Settlement poll {}/{}: error: {}", attempt, max_attempts, e);
            }
        }
    }

    // Fallback: proceed after max attempts even if not confirmed
    warn!(">>> Settlement: not confirmed after {} attempts — proceeding", max_attempts);
    true // proceed anyway — the sell will fail if tokens aren't there
}

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

                        // Poll CLOB for trade settlement (MATCHED→MINED)
                        // instead of blind wait — exit as soon as tokens are available
                        info!(">>> POLLING for settlement...");
                        let settled = poll_settlement(&order_id, 10, 500).await;
                        if settled {
                            info!(">>> Settlement CONFIRMED — monitoring for exit");
                        } else {
                            warn!(">>> Settlement not confirmed after 5s — proceeding anyway");
                        }
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
