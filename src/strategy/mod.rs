//! Strategy module — Scalp state machine with BUY + SELL logic.
//!
//! Monetizes the ~2s Binance→Chainlink oracle lag by:
//!   1. ENTRY: Buy token when strong directional signal fires (Binance moved, oracle hasn't)
//!   2. HOLD: Wait 1-10s for Polymarket token to reprice
//!   3. EXIT: Sell when target hit, signal reverses, or time stop
//!
//! Three execution modes for entry:
//!   Mode A (default): FOK entry (taker), GTD exit (maker) — recommended start
//!   Mode B: Maker entry (postOnly GTC), taker exit
//!   Mode C: Market-making (both sides) — future
//!
//! Position state machine per side: FLAT → ENTERING → LONG → EXITING → FLAT

use crate::types::{
    BotConfig, ObiSignal, OracleLagDirection, OracleLagSignal, OrderRequest, OrderSide, OrderType,
};

// ── Position State Machine ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionState {
    Flat,       // No position
    Entering,   // Entry order submitted, waiting for fill
    Long,       // Holding shares, waiting for exit signal
    Exiting,    // Exit order submitted, waiting for fill
}

#[derive(Debug, Clone)]
pub struct Position {
    pub state: PositionState,
    pub token_id: String,
    pub side_label: String,       // "UP" or "DN"
    pub entry_price: f64,         // price we bought at
    pub shares: f64,              // shares held
    pub entry_time_us: i64,       // when we entered
    pub entry_order_id: Option<String>,
    pub exit_order_id: Option<String>,
    pub exit_posted_at_us: i64,   // when exit order was posted
}

impl Position {
    pub fn new_flat() -> Self {
        Self {
            state: PositionState::Flat,
            token_id: String::new(),
            side_label: String::new(),
            entry_price: 0.0,
            shares: 0.0,
            entry_time_us: 0,
            entry_order_id: None,
            exit_order_id: None,
            exit_posted_at_us: 0,
        }
    }

    pub fn hold_time_ms(&self) -> i64 {
        if self.entry_time_us == 0 { return 0; }
        (chrono::Utc::now().timestamp_micros() - self.entry_time_us) / 1000
    }
}

// ── Strategy Actions ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum StrategyAction {
    /// Buy entry: FOK taker to grab liquidity
    TakerEntry {
        token_id: String,
        spend_usdc: f64,
        side_label: String,
        reason: String,
    },
    /// Sell exit: GTC postOnly to sell at target price (maker)
    MakerExit {
        token_id: String,
        shares: f64,
        price: f64,
        reason: String,
    },
    /// Sell exit: FOK taker to flatten immediately
    TakerExit {
        token_id: String,
        shares: f64,
        reason: String,
    },
    /// Cancel a specific order
    CancelOrder(String),
    /// Cancel all orders
    CancelAll,
    /// Do nothing
    Hold,
}

// ── Market State ────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MarketState {
    pub up_token_id: String,
    pub dn_token_id: String,
    pub up_best_ask: Option<f64>,
    pub up_best_bid: Option<f64>,
    pub dn_best_ask: Option<f64>,
    pub dn_best_bid: Option<f64>,
    pub up_filled: f64,
    pub dn_filled: f64,
    pub window_start_ts: i64,
    pub elapsed_s: u64,
    pub tick_size_up: f64,
    pub tick_size_dn: f64,
}

// ── Risk Limits ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_position_per_side: f64,
    pub max_open_orders: usize,
    pub max_cancel_rate: f64,
    pub cancel_count_this_window: u64,
    pub order_count_this_window: u64,
    pub kill_switch_triggered: bool,
    pub scalps_this_window: u64,
    pub max_scalps_per_window: u64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_per_side: 200.0,
            max_open_orders: 30,
            max_cancel_rate: 10.0,
            cancel_count_this_window: 0,
            order_count_this_window: 0,
            kill_switch_triggered: false,
            scalps_this_window: 0,
            max_scalps_per_window: 50,
        }
    }
}

// ── Binance Return Signal ───────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BinanceReturnSignal {
    pub return_2s: f64,     // 2-second return (bps)
    pub return_500ms: f64,  // 500ms return (bps)
    pub timestamp: i64,
}

// ── Signal Snapshot ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SignalSnapshot {
    pub score: f64,
    pub obi_component: f64,
    pub oracle_lag_component: f64,
    pub binance_return_component: f64,
    pub basis_bps: f64,
    pub mode: SignalMode,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalMode {
    NoSignal,       // Below threshold
    Entry,          // Strong enough to enter
}

// Keep old names for compatibility
pub type ExecutionMode = SignalMode;

// ── Signal Weights ──────────────────────────────────────────────

const W_OBI: f64 = 0.25;
const W_ORACLE_LAG: f64 = 0.40;
const W_BINANCE_RETURN: f64 = 0.35;

// ── Scalp Parameters ────────────────────────────────────────────

const ENTRY_THRESHOLD: f64 = 0.40;       // score > this → enter
const MIN_ORACLE_LAG_BPS: f64 = 1.5;     // oracle must be lagging
const ENTRY_SPEND_USDC: f64 = 20.0;      // dollars per entry
const EXIT_TARGET_PCT: f64 = 0.10;       // 10% profit target
const EXIT_TIME_STOP_MS: i64 = 8_000;    // 8s max hold
const EXIT_SIGNAL_REVERSAL: f64 = -0.20; // exit if signal flips against us
const COOLDOWN_MS: i64 = 3_000;          // 3s between scalps
const MAKER_EXIT_TIMEOUT_MS: i64 = 4_000; // 4s GTD timeout, then taker exit

/// Quantize price to nearest tick size (round DOWN for buys, round UP for sells).
fn quantize_buy(price: f64, tick_size: f64) -> f64 {
    if tick_size <= 0.0 {
        return (price * 100.0).floor() / 100.0;
    }
    let ticks = (price / tick_size).floor() as i64;
    let result = ticks as f64 * tick_size;
    (result * 1_000_000.0).round() / 1_000_000.0
}

fn quantize_sell(price: f64, tick_size: f64) -> f64 {
    if tick_size <= 0.0 {
        return (price * 100.0).ceil() / 100.0;
    }
    let ticks = (price / tick_size).ceil() as i64;
    let result = ticks as f64 * tick_size;
    (result * 1_000_000.0).round() / 1_000_000.0
}

// ── Signal Computation ──────────────────────────────────────────

pub fn compute_signal(
    obi: Option<&ObiSignal>,
    oracle_lag: Option<&OracleLagSignal>,
    binance_ret: Option<&BinanceReturnSignal>,
) -> SignalSnapshot {
    let obi_component = obi.map(|o| o.value.clamp(-1.0, 1.0)).unwrap_or(0.0);

    let (oracle_component, basis_bps) = oracle_lag
        .map(|o| {
            let dir = match o.direction {
                OracleLagDirection::Up => 1.0,
                OracleLagDirection::Down => -1.0,
                OracleLagDirection::Flat => 0.0,
            };
            let magnitude = (o.lag_bps.abs() / 5.0).clamp(0.0, 1.0);
            (dir * magnitude, o.lag_bps)
        })
        .unwrap_or((0.0, 0.0));

    let ret_component = binance_ret
        .map(|r| (r.return_2s / 5.0).clamp(-1.0, 1.0))
        .unwrap_or(0.0);

    let score = W_OBI * obi_component + W_ORACLE_LAG * oracle_component + W_BINANCE_RETURN * ret_component;

    let oracle_lagging = oracle_lag
        .map(|o| o.lag_bps.abs() >= MIN_ORACLE_LAG_BPS)
        .unwrap_or(false);

    let mode = if score.abs() >= ENTRY_THRESHOLD && oracle_lagging {
        SignalMode::Entry
    } else {
        SignalMode::NoSignal
    };

    let reason = format!(
        "score={:.3} obi={:.3} oracle={:.1}bps ret={:.1}bps",
        score, obi_component, basis_bps,
        binance_ret.map(|r| r.return_2s).unwrap_or(0.0)
    );

    SignalSnapshot {
        score,
        obi_component: W_OBI * obi_component,
        oracle_lag_component: W_ORACLE_LAG * oracle_component,
        binance_return_component: W_BINANCE_RETURN * ret_component,
        basis_bps,
        mode,
        reason,
    }
}

// ── Scalp Strategy Evaluator ────────────────────────────────────

/// Evaluate the scalp strategy given current position, signal, and market state.
///
/// Returns actions to take. The caller (main loop) is responsible for
/// executing actions and updating position state.
pub fn evaluate_scalp(
    position: &Position,
    signal: &SignalSnapshot,
    state: &MarketState,
    risk: &RiskLimits,
    config: &BotConfig,
    last_scalp_exit_us: i64,
) -> Vec<StrategyAction> {
    let mut actions = Vec::new();
    let now_us = chrono::Utc::now().timestamp_micros();

    // Kill switch
    if risk.kill_switch_triggered {
        actions.push(StrategyAction::CancelAll);
        return actions;
    }

    // Window timing gate
    if state.elapsed_s < config.start_delay_s {
        return actions;
    }
    if state.elapsed_s >= config.cancel_at_s {
        // Flatten any position before window ends
        if position.state == PositionState::Long {
            actions.push(StrategyAction::TakerExit {
                token_id: position.token_id.clone(),
                shares: position.shares,
                reason: "WINDOW END — emergency flatten".to_string(),
            });
        }
        actions.push(StrategyAction::CancelAll);
        return actions;
    }

    // Scalp rate limit
    if risk.scalps_this_window >= risk.max_scalps_per_window {
        return actions;
    }

    match position.state {
        // ── FLAT: look for entry ────────────────────────────
        PositionState::Flat => {
            // Cooldown check
            if last_scalp_exit_us > 0 && (now_us - last_scalp_exit_us) < COOLDOWN_MS as i64 * 1000 {
                actions.push(StrategyAction::Hold);
                return actions;
            }

            if signal.mode != SignalMode::Entry {
                actions.push(StrategyAction::Hold);
                return actions;
            }

            // Determine which token to buy
            let (token_id, best_ask, side_label) = if signal.score > 0.0 {
                // BTC going up → buy UP token
                (&state.up_token_id, state.up_best_ask, "UP")
            } else {
                // BTC going down → buy DN token
                (&state.dn_token_id, state.dn_best_ask, "DN")
            };

            if token_id.is_empty() {
                return actions;
            }

            if let Some(ask) = best_ask {
                // Sanity: don't buy if ask is too high or too low
                if ask > 0.05 && ask < 0.95 {
                    actions.push(StrategyAction::TakerEntry {
                        token_id: token_id.clone(),
                        spend_usdc: ENTRY_SPEND_USDC,
                        side_label: side_label.to_string(),
                        reason: format!("ENTRY {} | {}", side_label, signal.reason),
                    });
                }
            }
        }

        // ── ENTERING: waiting for fill confirmation ─────────
        PositionState::Entering => {
            // Check timeout: if entry not confirmed in 3s, go back to FLAT
            if position.hold_time_ms() > 3_000 {
                if let Some(ref oid) = position.entry_order_id {
                    actions.push(StrategyAction::CancelOrder(oid.clone()));
                }
            }
            // Otherwise wait for fill (handled by main loop via user WS)
        }

        // ── LONG: look for exit ─────────────────────────────
        PositionState::Long => {
            let hold_ms = position.hold_time_ms();

            // Get current bid for our token (what we can sell at)
            let current_bid = if position.token_id == state.up_token_id {
                state.up_best_bid
            } else {
                state.dn_best_bid
            };

            // Exit condition 1: TARGET HIT — bid is above entry + 10%
            if let Some(bid) = current_bid {
                let profit_pct = (bid - position.entry_price) / position.entry_price;
                if profit_pct >= EXIT_TARGET_PCT {
                    actions.push(StrategyAction::TakerExit {
                        token_id: position.token_id.clone(),
                        shares: position.shares,
                        reason: format!(
                            "TARGET HIT: entry={:.0}c bid={:.0}c profit={:.1}% hold={}ms",
                            position.entry_price * 100.0,
                            bid * 100.0,
                            profit_pct * 100.0,
                            hold_ms,
                        ),
                    });
                    return actions;
                }
            }

            // Exit condition 2: SIGNAL REVERSAL — signal flipped against our position
            let our_direction = if position.side_label == "UP" { 1.0 } else { -1.0 };
            if signal.score * our_direction < EXIT_SIGNAL_REVERSAL {
                actions.push(StrategyAction::TakerExit {
                    token_id: position.token_id.clone(),
                    shares: position.shares,
                    reason: format!(
                        "SIGNAL REVERSAL: score={:.3} vs our_dir={:.0} hold={}ms",
                        signal.score, our_direction, hold_ms,
                    ),
                });
                return actions;
            }

            // Exit condition 3: TIME STOP — held too long, edge decayed
            if hold_ms >= EXIT_TIME_STOP_MS {
                actions.push(StrategyAction::TakerExit {
                    token_id: position.token_id.clone(),
                    shares: position.shares,
                    reason: format!(
                        "TIME STOP: {}ms >= {}ms",
                        hold_ms, EXIT_TIME_STOP_MS,
                    ),
                });
                return actions;
            }

            // Otherwise hold
            actions.push(StrategyAction::Hold);
        }

        // ── EXITING: waiting for exit fill ──────────────────
        PositionState::Exiting => {
            let exit_elapsed_ms = if position.exit_posted_at_us > 0 {
                (now_us - position.exit_posted_at_us) / 1000
            } else {
                0
            };

            // If maker exit hasn't filled after timeout, go taker
            if exit_elapsed_ms > MAKER_EXIT_TIMEOUT_MS {
                // Cancel the maker exit and do a taker exit
                if let Some(ref oid) = position.exit_order_id {
                    actions.push(StrategyAction::CancelOrder(oid.clone()));
                }
                actions.push(StrategyAction::TakerExit {
                    token_id: position.token_id.clone(),
                    shares: position.shares,
                    reason: format!(
                        "MAKER EXIT TIMEOUT: {}ms, switching to taker",
                        exit_elapsed_ms,
                    ),
                });
            }
            // Otherwise wait for fill
        }
    }

    actions
}

// ── Backward compatibility ──────────────────────────────────────

/// Legacy evaluate function — wraps scalp evaluator for existing callers.
pub fn evaluate(
    state: &MarketState,
    signal: &SignalSnapshot,
    risk: &RiskLimits,
    config: &BotConfig,
) -> Vec<StrategyAction> {
    let flat_pos = Position::new_flat();
    evaluate_scalp(&flat_pos, signal, state, risk, config, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn default_config() -> BotConfig {
        BotConfig {
            poly_private_key: String::new(),
            poly_api_key: String::new(),
            poly_api_secret: String::new(),
            poly_api_passphrase: String::new(),
            market_slug: "btc-updown-5m".into(),
            window_seconds: 300,
            start_delay_s: 15,
            cancel_at_s: 240,
            max_position: 200.0,
            max_open_orders: 30,
            stop_loss_usd: -2000.0,
            obi_threshold: 0.3,
            oracle_lag_threshold_bps: 2.0,
        }
    }

    fn strong_signal_up() -> SignalSnapshot {
        let obi = ObiSignal { value: 0.9, levels_used: 10, timestamp: 0, local_ts: 0 };
        let oracle = OracleLagSignal {
            binance_price: 70000.0, chainlink_price: 69985.0,
            lag_bps: 2.1, direction: OracleLagDirection::Up,
            timestamp: 0, local_ts: 0,
        };
        let ret = BinanceReturnSignal { return_2s: 3.5, return_500ms: 1.5, timestamp: 0 };
        compute_signal(Some(&obi), Some(&oracle), Some(&ret))
    }

    #[test]
    fn test_flat_entry_on_strong_signal() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_ask: Some(0.65),
            tick_size_up: 0.01,
            ..Default::default()
        };
        let signal = strong_signal_up();
        assert_eq!(signal.mode, SignalMode::Entry);

        let pos = Position::new_flat();
        let actions = evaluate_scalp(&pos, &signal, &state, &RiskLimits::default(), &default_config(), 0);
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::TakerEntry { .. })));
    }

    #[test]
    fn test_long_exit_on_target() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_bid: Some(0.72), // 10.7% above entry of 0.65
            tick_size_up: 0.01,
            ..Default::default()
        };
        let signal = strong_signal_up();
        let mut pos = Position::new_flat();
        pos.state = PositionState::Long;
        pos.token_id = "up_123".into();
        pos.side_label = "UP".into();
        pos.entry_price = 0.65;
        pos.shares = 30.0;
        pos.entry_time_us = chrono::Utc::now().timestamp_micros() - 1_000_000; // 1s ago

        let actions = evaluate_scalp(&pos, &signal, &state, &RiskLimits::default(), &default_config(), 0);
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::TakerExit { .. })));
    }

    #[test]
    fn test_long_exit_on_time_stop() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_bid: Some(0.655), // not enough profit
            tick_size_up: 0.01,
            ..Default::default()
        };
        let signal = strong_signal_up();
        let mut pos = Position::new_flat();
        pos.state = PositionState::Long;
        pos.token_id = "up_123".into();
        pos.side_label = "UP".into();
        pos.entry_price = 0.65;
        pos.shares = 30.0;
        pos.entry_time_us = chrono::Utc::now().timestamp_micros() - 9_000_000; // 9s ago

        let actions = evaluate_scalp(&pos, &signal, &state, &RiskLimits::default(), &default_config(), 0);
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::TakerExit { .. })));
    }

    #[test]
    fn test_cooldown_prevents_immediate_reentry() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_ask: Some(0.65),
            tick_size_up: 0.01,
            ..Default::default()
        };
        let signal = strong_signal_up();
        let pos = Position::new_flat();
        // Last exit was 1s ago (within 3s cooldown)
        let last_exit = chrono::Utc::now().timestamp_micros() - 1_000_000;

        let actions = evaluate_scalp(&pos, &signal, &state, &RiskLimits::default(), &default_config(), last_exit);
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::Hold)));
        assert!(!actions.iter().any(|a| matches!(a, StrategyAction::TakerEntry { .. })));
    }

    #[test]
    fn test_window_end_flattens_position() {
        let state = MarketState {
            elapsed_s: 241, // past cancel_at_s
            up_token_id: "up_123".into(),
            ..Default::default()
        };
        let signal = compute_signal(None, None, None);
        let mut pos = Position::new_flat();
        pos.state = PositionState::Long;
        pos.token_id = "up_123".into();
        pos.shares = 30.0;

        let actions = evaluate_scalp(&pos, &signal, &state, &RiskLimits::default(), &default_config(), 0);
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::TakerExit { .. })));
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::CancelAll)));
    }

    #[test]
    fn test_quantize_sell_rounds_up() {
        assert_eq!(quantize_sell(0.653, 0.01), 0.66);
        assert_eq!(quantize_sell(0.65, 0.01), 0.65);
        assert_eq!(quantize_sell(0.651, 0.01), 0.66);
    }

    #[test]
    fn test_quantize_buy_rounds_down() {
        assert_eq!(quantize_buy(0.657, 0.01), 0.65);
        assert_eq!(quantize_buy(0.65, 0.01), 0.65);
    }
}
