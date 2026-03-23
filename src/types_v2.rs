//! Shared types for BeeBoop v2 — event-driven sniper bot.

use serde::{Deserialize, Serialize};

// ── Core Enums ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BotMode {
    DryRun,
    Live,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StrategyState {
    Idle,           // ready to trade
    Entering,       // first leg (momentum side) being bought
    WaitForReprice, // first leg filled, waiting ~2s for repricing
    BuyingSecondLeg,// buying the opposite side
    PairComplete,   // both sides held — done for this cycle
    Cooldown,       // brief pause before next pair
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryMode {
    Maker, // postOnly GTC — medium signal
    Taker, // FOK — strong signal
}

// ── Market Data Types ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceTick {
    pub recv_ts_ms: i64,
    pub event_ts_ms: i64,
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    pub obi: Option<f64>, // order book imbalance from depth, if available
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainlinkTick {
    pub recv_ts_ms: i64,
    pub source_ts_ms: Option<i64>,
    pub price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PolymarketTop {
    pub recv_ts_ms: i64,
    pub up_bid: Option<f64>,
    pub up_ask: Option<f64>,
    pub down_bid: Option<f64>,
    pub down_ask: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDescriptor {
    pub slug: String,
    pub condition_id: String,
    pub up_token_id: String,
    pub down_token_id: String,
    pub window_start_ts: i64,  // unix seconds
    pub window_end_ts: i64,
}

impl Default for MarketDescriptor {
    fn default() -> Self {
        Self {
            slug: String::new(),
            condition_id: String::new(),
            up_token_id: String::new(),
            down_token_id: String::new(),
            window_start_ts: 0,
            window_end_ts: 0,
        }
    }
}

// ── Signal Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSnapshot {
    pub now_ts_ms: i64,
    pub move_bps: f64,
    pub basis_bps: f64,
    pub volatility_bps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub created_ts_ms: i64,
    pub side: Side,
    pub entry_mode: EntryMode,

    // Raw return features
    pub r200_bps: f64,
    pub r500_bps: f64,
    pub r800_bps: f64,
    pub r2000_bps: f64,
    pub fast_move_bps: f64,

    // Oracle lag features (corrected — debiased)
    pub raw_basis_bps: f64,
    pub basis_dev_bps: f64,   // raw - EMA_60s (debiased)
    pub dbasis_bps: f64,      // short-horizon slope (250ms delta)

    // Smoothed OBI
    pub obi_ema: f64,

    // Combined score
    pub score: f64,
    pub confidence: f64,

    // Strong contradiction flag (hard veto)
    pub strong_contradiction: bool,
}

// ── Position Types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub market_slug: String,
    pub token_id: String,
    pub side: Side,
    pub entry_ts_ms: i64,
    pub entry_price: f64,
    pub size: f64,        // shares
    pub notional: f64,    // USDC spent
    pub signal_bps: f64,
    pub signal_confidence: f64,
}

// ── Execution Types ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionCommand {
    EnterTaker {
        market_slug: String,
        token_id: String,
        side: Side,
        max_price: f64,
        notional: f64,
        signal: Signal,
    },
    ExitTaker {
        market_slug: String,
        token_id: String,
        side: Side,
        shares: f64,
        min_price: f64,
        reason: String,
    },
    /// Buy the OPPOSITE side (second leg of pair).
    /// No settlement wait needed — USDC is always available for buying.
    BuySecondLeg {
        market_slug: String,
        opposite_token_id: String,
        opposite_side: Side,
        ask_price: f64, // current ask of the opposite token
        reason: String,
    },
    CancelAll {
        reason: String,
    },
    /// Vidarx: post a GTC postOnly BUY bid at a specific price/size
    PostMakerBid {
        market_slug: String,
        token_id: String,
        side: Side,
        price: f64,
        size: f64,
        post_only: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionEvent {
    EntryFilled {
        sent_ts_ms: i64,
        ack_ts_ms: i64,
        market_slug: String,
        token_id: String,
        side: Side,
        filled_price: f64,
        filled_size: f64,
        notional: f64,
        order_id: String,
        signal: Signal,
    },
    EntryRejected {
        ts_ms: i64,
        reason: String,
    },
    ExitFilled {
        sent_ts_ms: i64,
        ack_ts_ms: i64,
        market_slug: String,
        token_id: String,
        side: Side,
        filled_price: f64,
        filled_size: f64,
        reason: String,
        order_id: String,
    },
    ExitRejected {
        ts_ms: i64,
        reason: String,
    },
    /// Second leg filled — pair is complete
    SecondLegFilled {
        ts_ms: i64,
        market_slug: String,
        opposite_token_id: String,
        opposite_side: Side,
        filled_price: f64,
        filled_size: f64,
        reason: String,
    },
    SecondLegRejected {
        ts_ms: i64,
        reason: String,
    },
    CancelAck {
        ts_ms: i64,
        count: usize,
    },
}

// ── Logging Types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogEvent {
    SignalSeen {
        ts_ms: i64,
        signal: Signal,
    },
    TradeSkipped {
        ts_ms: i64,
        reason: String,
    },
    EntrySent {
        ts_ms: i64,
        side: Side,
        token_id: String,
        notional: f64,
        score: f64,
        fast_move_bps: f64,
        basis_dev_bps: f64,
    },
    EntryFilled {
        ts_ms: i64,
        side: Side,
        token_id: String,
        price: f64,
        size: f64,
        latency_ms: i64,
    },
    EntryRejected {
        ts_ms: i64,
        reason: String,
    },
    ExitSent {
        ts_ms: i64,
        side: Side,
        token_id: String,
        shares: f64,
        reason: String,
    },
    ExitFilled {
        ts_ms: i64,
        side: Side,
        price: f64,
        size: f64,
        latency_ms: i64,
    },
    PositionClosed {
        ts_ms: i64,
        market_slug: String,
        side: Side,
        entry_price: f64,
        exit_price: f64,
        size: f64,
        pnl_usdc: f64,
        hold_ms: i64,
        reason: String,
    },
    HoldToResolution {
        ts_ms: i64,
        market_slug: String,
        side: Side,
        entry_price: f64,
        size: f64,
        reason: String,
    },
    Metric {
        ts_ms: i64,
        key: String,
        value: f64,
    },
    WalletCheck {
        ts_ms: i64,
        balance_usdc: f64,
        session_pnl: f64,
    },
}
