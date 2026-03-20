//! Strategy module — PURE decision logic, no network calls.
//!
//! Takes signals (OBI, oracle lag, basis, market state) and outputs
//! order intents. The execution module handles actual placement.
//!
//! Two execution modes:
//!   Mode 1 (Maker): postOnly GTC when signal is moderate — sit below ask
//!   Mode 2 (Taker): FOK burst when signal is strong — grab liquidity now
//!
//! Signal: combined score from oracle lag + OBI + Binance return
//! Gated by: fee-aware threshold, window timing, risk limits

use crate::types::{
    BotConfig, ObiSignal, OracleLagDirection, OracleLagSignal, OrderRequest, OrderSide, OrderType,
};

/// A decision the strategy wants to make.
#[derive(Debug, Clone)]
pub enum StrategyAction {
    /// Place an order
    PlaceOrder(OrderRequest),
    /// Cancel a specific order
    CancelOrder(String),
    /// Cancel all orders
    CancelAll,
    /// Do nothing this cycle
    Hold,
}

/// Market state visible to the strategy.
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

/// Risk limits.
#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_position_per_side: f64,
    pub max_open_orders: usize,
    pub max_cancel_rate: f64,
    pub cancel_count_this_window: u64,
    pub order_count_this_window: u64,
    pub kill_switch_triggered: bool,
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
        }
    }
}

/// Binance return signal — computed from bookTicker mid changes.
#[derive(Debug, Clone)]
pub struct BinanceReturnSignal {
    pub return_2s: f64,     // 2-second return (bps)
    pub return_500ms: f64,  // 500ms return (bps)
    pub timestamp: i64,
}

/// Combined signal score and metadata for audit logging.
#[derive(Debug, Clone)]
pub struct SignalSnapshot {
    pub score: f64,
    pub obi_component: f64,
    pub oracle_lag_component: f64,
    pub binance_return_component: f64,
    pub basis_bps: f64,
    pub mode: ExecutionMode,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExecutionMode {
    Hold,           // No action
    Maker,          // postOnly GTC, sit below ask
    Taker,          // FOK, grab liquidity now
}

// ── Signal weights (calibrated from lead/lag lab) ────────────────

const W_OBI: f64 = 0.25;
const W_ORACLE_LAG: f64 = 0.40;
const W_BINANCE_RETURN: f64 = 0.35;

// ── Thresholds ───────────────────────────────────────────────────

const MAKER_THRESHOLD: f64 = 0.30;     // score > this → maker mode
const TAKER_THRESHOLD: f64 = 0.65;     // score > this → taker burst
const MIN_ORACLE_LAG_BPS: f64 = 1.5;   // oracle must be lagging >= 1.5 bps
const TAKER_FEE_BPS: f64 = 3.0;        // approximate taker fee
const MAKER_OFFSET_C: f64 = 0.02;      // maker bid = ask - 2c

// ── Strategy sizes ───────────────────────────────────────────────

const MAKER_SIZE: f64 = 20.0;          // shares per maker GTC
const TAKER_SIZE_DOLLARS: f64 = 30.0;  // dollar notional for FOK burst

/// Quantize price to nearest tick size (always round DOWN for buys).
/// Uses integer math internally to avoid floating point precision issues.
fn quantize_price(price: f64, tick_size: f64) -> f64 {
    if tick_size <= 0.0 {
        return (price * 100.0).floor() / 100.0; // default 1c tick
    }
    // Convert to integer ticks to avoid float rounding
    let ticks = (price / tick_size).floor() as i64;
    let result = ticks as f64 * tick_size;
    // Round to avoid accumulated float error (6 decimal places)
    (result * 1_000_000.0).round() / 1_000_000.0
}

/// Compute combined signal score from all inputs.
pub fn compute_signal(
    obi: Option<&ObiSignal>,
    oracle_lag: Option<&OracleLagSignal>,
    binance_ret: Option<&BinanceReturnSignal>,
) -> SignalSnapshot {
    // OBI component: normalize to [-1, +1] range, already there
    let obi_raw = obi.map(|o| o.value).unwrap_or(0.0);
    let obi_component = obi_raw.clamp(-1.0, 1.0);

    // Oracle lag component: basis direction + magnitude
    // Positive lag_bps = Binance above Chainlink = oracle will update UP
    let (oracle_component, basis_bps) = oracle_lag
        .map(|o| {
            let dir = match o.direction {
                OracleLagDirection::Up => 1.0,
                OracleLagDirection::Down => -1.0,
                OracleLagDirection::Flat => 0.0,
            };
            // Scale: 2bps lag → component of ±0.5, 5bps → ±1.0
            let magnitude = (o.lag_bps.abs() / 5.0).clamp(0.0, 1.0);
            (dir * magnitude, o.lag_bps)
        })
        .unwrap_or((0.0, 0.0));

    // Binance return component: normalize 2s return
    // 2bps → component of ±0.5, 5bps → ±1.0
    let (ret_component, _ret_raw) = binance_ret
        .map(|r| {
            let normalized = (r.return_2s / 5.0).clamp(-1.0, 1.0);
            (normalized, r.return_2s)
        })
        .unwrap_or((0.0, 0.0));

    // Combined weighted score
    let score = W_OBI * obi_component + W_ORACLE_LAG * oracle_component + W_BINANCE_RETURN * ret_component;

    // Determine execution mode
    let abs_score = score.abs();

    // Gate: oracle must actually be lagging for taker mode
    let oracle_lagging = oracle_lag
        .map(|o| o.lag_bps.abs() >= MIN_ORACLE_LAG_BPS)
        .unwrap_or(false);

    let mode = if abs_score >= TAKER_THRESHOLD && oracle_lagging {
        ExecutionMode::Taker
    } else if abs_score >= MAKER_THRESHOLD {
        ExecutionMode::Maker
    } else {
        ExecutionMode::Hold
    };

    let reason = match mode {
        ExecutionMode::Taker => format!(
            "TAKER: score={:.3} obi={:.3} oracle={:.1}bps ret={:.1}bps",
            score, obi_component, basis_bps, binance_ret.map(|r| r.return_2s).unwrap_or(0.0)
        ),
        ExecutionMode::Maker => format!(
            "MAKER: score={:.3} obi={:.3} oracle={:.1}bps ret={:.1}bps",
            score, obi_component, basis_bps, binance_ret.map(|r| r.return_2s).unwrap_or(0.0)
        ),
        ExecutionMode::Hold => format!(
            "HOLD: score={:.3} (threshold={:.2})",
            score, MAKER_THRESHOLD
        ),
    };

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

/// v1 Strategy: OBI + Oracle Lag + Binance Return → directional trading.
///
/// Two modes:
///   - Maker: postOnly GTC below ask when signal is moderate
///   - Taker: FOK burst when signal is strong AND oracle is lagging
///
/// Guardrails:
///   - Max position per side
///   - Cancel rate limiting
///   - postOnly never combined with FOK
///   - Fee-aware: taker only fires when expected move > fees + spread
pub fn evaluate(
    state: &MarketState,
    signal: &SignalSnapshot,
    risk: &RiskLimits,
    config: &BotConfig,
) -> Vec<StrategyAction> {
    let mut actions = Vec::new();

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
        actions.push(StrategyAction::CancelAll);
        return actions;
    }

    // Order rate sanity check
    if risk.order_count_this_window as usize >= risk.max_open_orders * 10 {
        return actions; // too many orders this window, back off
    }

    match signal.mode {
        ExecutionMode::Hold => {
            actions.push(StrategyAction::Hold);
        }

        ExecutionMode::Maker => {
            // Determine which side to buy based on signal direction
            let (token_id, best_ask, filled, tick_size) = if signal.score > 0.0 {
                // BTC going up → buy UP token
                (&state.up_token_id, state.up_best_ask, state.up_filled, state.tick_size_up)
            } else {
                // BTC going down → buy DN token
                (&state.dn_token_id, state.dn_best_ask, state.dn_filled, state.tick_size_dn)
            };

            if let Some(ask) = best_ask {
                if filled < risk.max_position_per_side {
                    let raw_price = ask - MAKER_OFFSET_C;
                    let price = quantize_price(raw_price, tick_size);

                    if price > 0.0 && price < ask {
                        actions.push(StrategyAction::PlaceOrder(OrderRequest {
                            token_id: token_id.clone(),
                            side: OrderSide::Buy,
                            price,
                            size: MAKER_SIZE,
                            order_type: OrderType::GTC,
                            post_only: true, // GTC + postOnly = guaranteed maker
                        }));
                    }
                }
            }
        }

        ExecutionMode::Taker => {
            // Determine side
            let (token_id, best_ask, filled, tick_size) = if signal.score > 0.0 {
                (&state.up_token_id, state.up_best_ask, state.up_filled, state.tick_size_up)
            } else {
                (&state.dn_token_id, state.dn_best_ask, state.dn_filled, state.tick_size_dn)
            };

            if let Some(ask) = best_ask {
                if filled < risk.max_position_per_side {
                    // Fee-aware check: expected move must exceed taker fee + spread
                    let expected_move_bps = signal.basis_bps.abs();
                    let spread_bps = 100.0 * (ask - ask * 0.99) / ask; // rough estimate

                    if expected_move_bps > TAKER_FEE_BPS + spread_bps {
                        // FOK: specify dollar amount for BUY (per Polymarket docs)
                        // size = dollar notional / price ≈ shares we'll get
                        let shares = (TAKER_SIZE_DOLLARS / ask).floor();
                        let price = quantize_price(0.99, tick_size); // limit at 99c

                        if shares > 0.0 {
                            actions.push(StrategyAction::PlaceOrder(OrderRequest {
                                token_id: token_id.clone(),
                                side: OrderSide::Buy,
                                price,
                                size: shares,
                                order_type: OrderType::FOK,
                                post_only: false, // FOK cannot be postOnly
                            }));
                        }
                    }
                }
            }
        }
    }

    actions
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

    #[test]
    fn test_hold_when_no_signal() {
        let state = MarketState {
            elapsed_s: 20,
            up_best_ask: Some(0.65),
            dn_best_ask: Some(0.35),
            tick_size_up: 0.01,
            tick_size_dn: 0.01,
            ..Default::default()
        };
        let signal = compute_signal(None, None, None);
        assert_eq!(signal.mode, ExecutionMode::Hold);
        let actions = evaluate(&state, &signal, &RiskLimits::default(), &default_config());
        assert!(matches!(actions[0], StrategyAction::Hold));
    }

    #[test]
    fn test_maker_mode_on_moderate_signal() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_ask: Some(0.65),
            tick_size_up: 0.01,
            ..Default::default()
        };
        let obi = ObiSignal {
            value: 0.8,
            levels_used: 10,
            timestamp: 0,
            local_ts: 0,
        };
        let oracle = OracleLagSignal {
            binance_price: 70000.0,
            chainlink_price: 69990.0,
            lag_bps: 1.4, // below taker threshold
            direction: OracleLagDirection::Up,
            timestamp: 0,
            local_ts: 0,
        };
        let signal = compute_signal(Some(&obi), Some(&oracle), None);
        assert_eq!(signal.mode, ExecutionMode::Maker);
    }

    #[test]
    fn test_taker_mode_on_strong_signal() {
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_ask: Some(0.65),
            tick_size_up: 0.01,
            ..Default::default()
        };
        let obi = ObiSignal {
            value: 0.95,
            levels_used: 10,
            timestamp: 0,
            local_ts: 0,
        };
        let oracle = OracleLagSignal {
            binance_price: 70000.0,
            chainlink_price: 69980.0,
            lag_bps: 2.9, // above min oracle lag
            direction: OracleLagDirection::Up,
            timestamp: 0,
            local_ts: 0,
        };
        let ret = BinanceReturnSignal {
            return_2s: 4.0,
            return_500ms: 2.0,
            timestamp: 0,
        };
        let signal = compute_signal(Some(&obi), Some(&oracle), Some(&ret));
        assert_eq!(signal.mode, ExecutionMode::Taker);
    }

    #[test]
    fn test_cancel_all_at_window_end() {
        let state = MarketState {
            elapsed_s: 241,
            ..Default::default()
        };
        let signal = compute_signal(None, None, None);
        let actions = evaluate(&state, &signal, &RiskLimits::default(), &default_config());
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::CancelAll)));
    }

    #[test]
    fn test_post_only_never_with_fok() {
        // Ensure FOK orders always have post_only = false
        let state = MarketState {
            elapsed_s: 20,
            up_token_id: "up_123".into(),
            up_best_ask: Some(0.65),
            tick_size_up: 0.01,
            ..Default::default()
        };
        let obi = ObiSignal { value: 0.95, levels_used: 10, timestamp: 0, local_ts: 0 };
        let oracle = OracleLagSignal {
            binance_price: 70000.0, chainlink_price: 69980.0,
            lag_bps: 3.0, direction: OracleLagDirection::Up,
            timestamp: 0, local_ts: 0,
        };
        let ret = BinanceReturnSignal { return_2s: 5.0, return_500ms: 3.0, timestamp: 0 };
        let signal = compute_signal(Some(&obi), Some(&oracle), Some(&ret));
        let actions = evaluate(&state, &signal, &RiskLimits::default(), &default_config());
        for action in &actions {
            if let StrategyAction::PlaceOrder(req) = action {
                if req.order_type == OrderType::FOK {
                    assert!(!req.post_only, "FOK must never have postOnly=true");
                }
            }
        }
    }

    #[test]
    fn test_quantize_price() {
        assert_eq!(quantize_price(0.637, 0.01), 0.63);
        assert_eq!(quantize_price(0.637, 0.001), 0.637);
        assert_eq!(quantize_price(0.635, 0.05), 0.60);
    }

    #[test]
    fn test_kill_switch() {
        let state = MarketState { elapsed_s: 20, ..Default::default() };
        let signal = compute_signal(None, None, None);
        let mut risk = RiskLimits::default();
        risk.kill_switch_triggered = true;
        let actions = evaluate(&state, &signal, &risk, &default_config());
        assert!(actions.iter().any(|a| matches!(a, StrategyAction::CancelAll)));
    }
}
