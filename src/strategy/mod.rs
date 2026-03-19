//! Strategy module — PURE decision logic, no network calls.
//!
//! Takes signals (OBI, oracle lag, market state) and outputs
//! order intents. The execution module handles actual placement.

use crate::types::{BotConfig, ObiSignal, OracleLagDirection, OracleLagSignal, OrderRequest, OrderSide, OrderType};

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
}

/// Risk limits.
#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_position_per_side: f64,
    pub max_open_orders: usize,
    pub max_cancel_rate: f64,    // cancels per second
    pub kill_switch_triggered: bool,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_per_side: 200.0,
            max_open_orders: 30,
            max_cancel_rate: 10.0,
            kill_switch_triggered: false,
        }
    }
}

/// v0 Strategy: OBI + Oracle Lag → directional maker quoting.
///
/// This is intentionally simple and non-manipulative:
/// - At most one bid per outcome per cycle
/// - postOnly to guarantee maker status
/// - Cancel only when target price moves by >= 1 tick
pub fn evaluate(
    state: &MarketState,
    obi: Option<&ObiSignal>,
    oracle_lag: Option<&OracleLagSignal>,
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
    if state.elapsed_s < config.start_delay_s || state.elapsed_s >= config.cancel_at_s {
        if state.elapsed_s >= config.cancel_at_s {
            actions.push(StrategyAction::CancelAll);
        }
        return actions;
    }

    // Determine direction from signals
    let obi_direction = obi.map(|o| {
        if o.value > config.obi_threshold { 1.0 }
        else if o.value < -config.obi_threshold { -1.0 }
        else { 0.0 }
    }).unwrap_or(0.0);

    let oracle_direction = oracle_lag.map(|o| {
        match o.direction {
            OracleLagDirection::Up if o.lag_bps.abs() > config.oracle_lag_threshold_bps => 1.0,
            OracleLagDirection::Down if o.lag_bps.abs() > config.oracle_lag_threshold_bps => -1.0,
            _ => 0.0,
        }
    }).unwrap_or(0.0);

    // Combined signal (simple average for v0)
    let signal = (obi_direction + oracle_direction) / 2.0;

    // If signal is positive → BTC going up → buy UP token
    // If signal is negative → BTC going down → buy DN token
    if signal > 0.0 {
        if let Some(ask) = state.up_best_ask {
            if state.up_filled < risk.max_position_per_side {
                actions.push(StrategyAction::PlaceOrder(OrderRequest {
                    token_id: state.up_token_id.clone(),
                    side: OrderSide::Buy,
                    price: ask - 0.03, // 3c below ask
                    size: 20.0,
                    order_type: OrderType::GTC,
                    post_only: true,
                }));
            }
        }
    } else if signal < 0.0 {
        if let Some(ask) = state.dn_best_ask {
            if state.dn_filled < risk.max_position_per_side {
                actions.push(StrategyAction::PlaceOrder(OrderRequest {
                    token_id: state.dn_token_id.clone(),
                    side: OrderSide::Buy,
                    price: ask - 0.03,
                    size: 20.0,
                    order_type: OrderType::GTC,
                    post_only: true,
                }));
            }
        }
    }

    actions
}
