//! Pair Position Manager — buy momentum side, wait for reprice, buy other side.
//!
//! Strategy: buy both sides of a binary market for < $1 total per pair.
//! The 2s oracle lag means we buy the momentum side cheap, then the other
//! side cheap after repricing. Hold both to resolution. Collect $1. Profit.
//!
//! NO SELLING. Ever. Both sides resolve at window end.

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

/// Tracks one pair trade (both legs)
#[derive(Debug, Clone)]
struct PairTrade {
    market_slug: String,
    // First leg (momentum side)
    leg1_side: Side,
    leg1_token_id: String,
    leg1_price: f64,
    leg1_shares: f64,
    leg1_notional: f64,
    leg1_ts_ms: i64,
    // Second leg (opposite side) — filled later
    leg2_side: Option<Side>,
    leg2_price: Option<f64>,
    leg2_shares: Option<f64>,
    leg2_notional: Option<f64>,
}

pub async fn run_position_manager_task(
    config: Config,
    shared: SharedState,
    market_rx: watch::Receiver<MarketDescriptor>,
    _pm_top_rx: watch::Receiver<PolymarketTop>,
    mut exec_evt_rx: mpsc::Receiver<ExecutionEvent>,
    exec_cmd_tx: mpsc::Sender<ExecutionCommand>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    info!("position_manager: PAIR MODE — buy momentum, wait, buy opposite, hold to resolution");
    info!("position_manager: target=20sh/side, cooldown={}ms", config.cooldown_ms);

    let mut current_pair: Option<PairTrade> = None;
    let mut window_pair_done: bool = false;   // ONE pair per window, period
    let mut window_shares_bought: f64 = 0.0;  // tracking only
    let mut last_window_ts: i64 = 0;
    let reprice_wait_ms: u64 = 2000;         // wait 2s for repricing before buying second leg

    loop {
        // ── Window change: reset everything ──
        {
            let market = market_rx.borrow().clone();
            if market.window_start_ts > 0 && market.window_start_ts != last_window_ts {
                if last_window_ts > 0 {
                    let state = shared.get_state();
                    if state != StrategyState::Idle {
                        info!(">>> NEW WINDOW {} — resetting from {:?} to Idle | window_shares={:.0}",
                            market.window_start_ts, state, window_shares_bought);
                    }
                    shared.clear_position();
                    shared.set_state(StrategyState::Idle);
                    current_pair = None;
                    window_pair_done = false;
                    window_shares_bought = 0.0;
                }
                last_window_ts = market.window_start_ts;
            }
        }

        tokio::select! {
            // ── Handle execution events ──
            evt = exec_evt_rx.recv() => {
                let Some(evt) = evt else { break };
                match evt {
                    // ── First leg filled ──
                    ExecutionEvent::EntryFilled {
                        market_slug, token_id, side, filled_price, filled_size,
                        notional, ack_ts_ms, signal, ..
                    } => {
                        let side_label = if side == Side::Up { "UP" } else { "DN" };
                        info!(">>> LEG 1 FILLED: {} {:.0}sh @ {:.0}c (${:.2}) | waiting {}ms for reprice",
                            side_label, filled_size, filled_price * 100.0, notional, reprice_wait_ms);

                        current_pair = Some(PairTrade {
                            market_slug: market_slug.clone(),
                            leg1_side: side,
                            leg1_token_id: token_id,
                            leg1_price: filled_price,
                            leg1_shares: filled_size,
                            leg1_notional: notional,
                            leg1_ts_ms: ack_ts_ms,
                            leg2_side: None,
                            leg2_price: None,
                            leg2_shares: None,
                            leg2_notional: None,
                        });

                        window_shares_bought += filled_size;

                        // Move to WaitForReprice
                        shared.set_state(StrategyState::WaitForReprice);

                        // Wait for repricing, then buy second leg
                        let wait = reprice_wait_ms;
                        let exec_tx = exec_cmd_tx.clone();
                        let market = market_rx.borrow().clone();
                        let shared_c = shared.clone();
                        let pm_top_for_leg2 = _pm_top_rx.clone();

                        // Determine opposite side token
                        let (opp_token, opp_side) = match side {
                            Side::Up => (market.down_token_id.clone(), Side::Down),
                            Side::Down => (market.up_token_id.clone(), Side::Up),
                        };

                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;

                            if *shared_c.strategy_state.read() != StrategyState::WaitForReprice {
                                return; // state changed (window rolled), abort
                            }

                            // Read the CURRENT ask of the opposite token (after repricing)
                            let top = pm_top_for_leg2.borrow().clone();
                            let opp_ask = match opp_side {
                                Side::Up => top.up_ask.unwrap_or(0.50),
                                Side::Down => top.down_ask.unwrap_or(0.50),
                            };

                            let opp_label = if opp_side == Side::Up { "UP" } else { "DN" };
                            let shares_to_post = (20.0_f64 * opp_ask).floor();
                            info!(">>> REPRICING DONE — buying LEG 2: {} {:.0}sh @ 99c (ask={:.0}c)",
                                opp_label, shares_to_post, opp_ask * 100.0);

                            shared_c.set_state(StrategyState::BuyingSecondLeg);

                            let _ = exec_tx.send(ExecutionCommand::BuySecondLeg {
                                market_slug,
                                opposite_token_id: opp_token,
                                opposite_side: opp_side,
                                ask_price: opp_ask,
                                reason: "leg2_pair".to_string(),
                            }).await;
                        });
                    }

                    ExecutionEvent::EntryRejected { reason, .. } => {
                        warn!(">>> LEG 1 REJECTED: {} — back to Idle", reason);
                        shared.set_state(StrategyState::Idle);
                        current_pair = None;
                    }

                    // ── Second leg filled ──
                    ExecutionEvent::SecondLegFilled {
                        opposite_side, filled_price, filled_size, reason, ..
                    } => {
                        let opp_label = if opposite_side == Side::Up { "UP" } else { "DN" };
                        let leg2_notional = filled_size * filled_price; // estimate cost

                        if let Some(ref mut pair) = current_pair {
                            pair.leg2_side = Some(opposite_side);
                            pair.leg2_price = Some(filled_price);
                            pair.leg2_shares = Some(filled_size);
                            pair.leg2_notional = Some(leg2_notional);

                            let total_cost = pair.leg1_notional + leg2_notional;
                            let pair_shares = pair.leg1_shares.min(filled_size);
                            let guaranteed_payout = pair_shares; // $1 per pair at resolution
                            let est_profit = guaranteed_payout - total_cost;
                            let cost_per_pair = if pair_shares > 0.0 { total_cost / pair_shares } else { 1.0 };

                            info!(">>> ✅ PAIR COMPLETE: {} {:.0}sh@{:.0}c + {} {:.0}sh@{:.0}c | cost/pair={:.2}c | est profit=${:.2}",
                                if pair.leg1_side == Side::Up { "UP" } else { "DN" },
                                pair.leg1_shares, pair.leg1_price * 100.0,
                                opp_label, filled_size, filled_price * 100.0,
                                cost_per_pair * 100.0, est_profit);

                            window_shares_bought += filled_size;

                            let _ = log_tx.send(LogEvent::PositionClosed {
                                ts_ms: chrono::Utc::now().timestamp_millis(),
                                market_slug: pair.market_slug.clone(),
                                side: pair.leg1_side,
                                entry_price: pair.leg1_price,
                                exit_price: filled_price,
                                size: pair_shares,
                                pnl_usdc: est_profit,
                                hold_ms: chrono::Utc::now().timestamp_millis() - pair.leg1_ts_ms,
                                reason: format!("pair_complete cost={:.3}", cost_per_pair),
                            }).await;
                        } else {
                            info!(">>> LEG 2 FILLED but no active pair (window rolled?) — {} {:.0}sh@{:.0}c",
                                opp_label, filled_size, filled_price * 100.0);
                        }

                        // ONE pair per window. Done. Wait for next window.
                        window_pair_done = true;
                        shared.set_state(StrategyState::PairComplete);
                        current_pair = None;
                        info!(">>> DONE FOR THIS WINDOW — waiting for next window");
                    }

                    ExecutionEvent::SecondLegRejected { reason, .. } => {
                        warn!(">>> LEG 2 FAILED: {} — holding leg 1 to resolution", reason);
                        // Done for this window even if leg 2 failed
                        window_pair_done = true;
                        current_pair = None;
                        shared.set_state(StrategyState::PairComplete);
                        info!(">>> DONE FOR THIS WINDOW (leg 2 failed) — waiting for next window");
                    }

                    // Legacy events — ignore
                    ExecutionEvent::ExitFilled { .. } => {}
                    ExecutionEvent::ExitRejected { .. } => {}
                    ExecutionEvent::CancelAck { .. } => {}
                }
            }

            // ── Periodic check for stuck states ──
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let state = shared.get_state();
                match state {
                    StrategyState::WaitForReprice | StrategyState::BuyingSecondLeg => {
                        // Check if we've been stuck too long (>15s)
                        if let Some(ref pair) = current_pair {
                            let stuck_ms = chrono::Utc::now().timestamp_millis() - pair.leg1_ts_ms;
                            if stuck_ms > 15_000 {
                                warn!(">>> STUCK in {:?} for {}s — forcing to Idle", state, stuck_ms / 1000);
                                current_pair = None;
                                shared.set_state(StrategyState::Idle);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
