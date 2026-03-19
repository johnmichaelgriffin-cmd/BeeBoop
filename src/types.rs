//! Shared types used across all modules.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Price & Order Book ──────────────────────────────────────────

/// A single price level in an order book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

/// Snapshot of one side of an order book (sorted by price).
pub type BookSide = BTreeMap<u64, f64>; // price_cents -> size

/// Full L2 order book for one token.
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub token_id: String,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: i64,       // venue timestamp (ms)
    pub local_recv_ts: i64,   // local recv timestamp (us)
}

/// Best bid/ask for one token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BBO {
    pub token_id: String,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub timestamp: i64,
    pub local_recv_ts: i64,
}

// ── Market Data Events ──────────────────────────────────────────

/// Tick size for a token (must quantize prices to this).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickSize {
    pub token_id: String,
    pub tick_size: f64,
}

/// A trade that occurred on the venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: String, // "BUY" or "SELL"
    pub timestamp: i64,
    pub local_recv_ts: i64,
}

// ── Oracle / Reference Prices ───────────────────────────────────

/// A price update from any reference source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefPrice {
    pub source: RefSource,
    pub symbol: String,
    pub price: f64,
    pub timestamp: i64,       // source timestamp (ms)
    pub local_recv_ts: i64,   // local recv timestamp (us)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefSource {
    BinanceBookTicker,
    BinanceDepth,
    RtdsBinance,
    RtdsChainlink,
}

// ── Binance L2 ──────────────────────────────────────────────────

/// Binance depth update (diff).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceDepthUpdate {
    pub event_time: i64,
    pub transaction_time: i64,
    pub first_update_id: u64,
    pub last_update_id: u64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub local_recv_ts: i64,
}

/// Binance book ticker (L1 best bid/ask).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceBookTicker {
    pub symbol: String,
    pub best_bid: f64,
    pub best_bid_qty: f64,
    pub best_ask: f64,
    pub best_ask_qty: f64,
    pub event_time: i64,
    pub transaction_time: i64,
    pub local_recv_ts: i64,
}

// ── Order Lifecycle ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    GTC,
    FOK,
    GTD,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: OrderSide,
    pub price: f64,
    pub size: f64,
    pub order_type: OrderType,
    pub post_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Placed,
    Matched,
    Mined,
    Confirmed,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderState {
    pub order_id: String,
    pub request: OrderRequest,
    pub status: OrderStatus,
    pub filled_size: f64,
    pub avg_fill_price: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Signals ─────────────────────────────────────────────────────

/// Order book imbalance signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObiSignal {
    pub value: f64,         // [-1, +1]
    pub levels_used: usize,
    pub timestamp: i64,
    pub local_ts: i64,
}

/// Oracle lag signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleLagSignal {
    pub binance_price: f64,
    pub chainlink_price: f64,
    pub lag_bps: f64,
    pub direction: OracleLagDirection,
    pub timestamp: i64,
    pub local_ts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OracleLagDirection {
    Up,    // Binance above Chainlink
    Down,  // Binance below Chainlink
    Flat,  // Within threshold
}

// ── Unified Event (for recording / replay) ──────────────────────

/// Every event that flows through the system, tagged for recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub local_recv_us: i64,  // microsecond precision local timestamp
    pub source: String,      // "market_ws", "user_ws", "rtds", "binance_l2"
    pub event_type: String,  // "book", "bbo", "trade", "tick_size", "ref_price", etc.
    pub raw_json: String,    // original payload for replay
}

// ── Config ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub poly_private_key: String,
    pub poly_api_key: String,
    pub poly_api_secret: String,
    pub poly_api_passphrase: String,
    pub market_slug: String,          // e.g. "btc-updown-5m"
    pub window_seconds: u64,          // 300
    pub start_delay_s: u64,           // T+15s (when to start)
    pub cancel_at_s: u64,             // T+240s (when to cancel)
    pub max_position: f64,            // max shares per side
    pub max_open_orders: usize,       // kill switch
    pub stop_loss_usd: f64,           // session kill switch
    pub obi_threshold: f64,           // signal threshold
    pub oracle_lag_threshold_bps: f64,// signal threshold
}
