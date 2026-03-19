//! Binance Perpetual Futures L2 order book ingestion.
//!
//! Connects to:
//!   - `wss://fstream.binance.com/ws/btcusdt@depth10@100ms` (L2 top-10 levels, 100ms)
//!   - `wss://fstream.binance.com/ws/btcusdt@bookTicker` (L1 best bid/ask, real-time)
//!
//! Computes Order Book Imbalance (OBI) from the depth stream.
//! This module is PASSIVE — emits signals to channels.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types::{
    BinanceBookTicker, BinanceDepthUpdate, ObiSignal, PriceLevel,
    RecordedEvent, RefPrice, RefSource,
};

const DEPTH_URL: &str = "wss://fstream.binance.com/ws/btcusdt@depth10@100ms";
const BOOKTICKER_URL: &str = "wss://fstream.binance.com/ws/btcusdt@bookTicker";

/// Events emitted by the Binance L2 module.
#[derive(Debug, Clone)]
pub enum BinanceEvent {
    Depth(BinanceDepthUpdate),
    BookTicker(BinanceBookTicker),
    Obi(ObiSignal),
    MidPrice(RefPrice),
    Raw(RecordedEvent),
}

/// Compute weighted OBI from bid/ask levels.
/// Uses exponential decay: level 0 weight = 1.0, level 1 = 0.5, etc.
pub fn compute_obi(bids: &[PriceLevel], asks: &[PriceLevel], max_levels: usize) -> f64 {
    let decay = 0.5_f64;
    let mut weighted_bid = 0.0;
    let mut weighted_ask = 0.0;
    let levels = max_levels.min(bids.len()).min(asks.len());

    for i in 0..levels {
        let w = decay.powi(i as i32);
        weighted_bid += w * bids[i].size;
        weighted_ask += w * asks[i].size;
    }

    let total = weighted_bid + weighted_ask;
    if total == 0.0 {
        return 0.0;
    }
    (weighted_bid - weighted_ask) / total
}

/// Run both Binance WebSocket streams concurrently.
pub async fn run(tx: mpsc::UnboundedSender<BinanceEvent>) {
    let tx2 = tx.clone();
    tokio::join!(
        run_depth(tx),
        run_bookticker(tx2),
    );
}

async fn run_depth(tx: mpsc::UnboundedSender<BinanceEvent>) {
    loop {
        info!("binance_l2: connecting depth stream");
        match connect_depth(&tx).await {
            Ok(()) => warn!("binance_l2: depth stream closed, reconnecting..."),
            Err(e) => error!("binance_l2: depth error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_depth(
    tx: &mpsc::UnboundedSender<BinanceEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(DEPTH_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("binance_l2: depth stream connected");

    while let Some(msg) = read.next().await {
        let local_recv_us = chrono::Utc::now().timestamp_micros();

        match msg? {
            Message::Text(text) => {
                let text_str: &str = text.as_ref();

                // Record raw
                let raw = RecordedEvent {
                    local_recv_us,
                    source: "binance_l2".to_string(),
                    event_type: "depth".to_string(),
                    raw_json: text_str.to_string(),
                };
                let _ = tx.send(BinanceEvent::Raw(raw));

                // Parse depth update
                if let Ok(update) = parse_depth(text_str, local_recv_us) {
                    // Compute OBI
                    let obi_value = compute_obi(&update.bids, &update.asks, 10);
                    let obi = ObiSignal {
                        value: obi_value,
                        levels_used: update.bids.len().min(update.asks.len()).min(10),
                        timestamp: update.event_time,
                        local_ts: local_recv_us,
                    };
                    let _ = tx.send(BinanceEvent::Obi(obi));

                    // Compute mid price for lead/lag
                    if !update.bids.is_empty() && !update.asks.is_empty() {
                        let mid = (update.bids[0].price + update.asks[0].price) / 2.0;
                        let _ = tx.send(BinanceEvent::MidPrice(RefPrice {
                            source: RefSource::BinanceDepth,
                            symbol: "btcusdt".to_string(),
                            price: mid,
                            timestamp: update.event_time,
                            local_recv_ts: local_recv_us,
                        }));
                    }

                    let _ = tx.send(BinanceEvent::Depth(update));
                }
            }
            Message::Ping(data) => {
                write.send(Message::Pong(data)).await?;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

async fn run_bookticker(tx: mpsc::UnboundedSender<BinanceEvent>) {
    loop {
        info!("binance_l2: connecting bookTicker stream");
        match connect_bookticker(&tx).await {
            Ok(()) => warn!("binance_l2: bookTicker closed, reconnecting..."),
            Err(e) => error!("binance_l2: bookTicker error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_bookticker(
    tx: &mpsc::UnboundedSender<BinanceEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(BOOKTICKER_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("binance_l2: bookTicker connected");

    while let Some(msg) = read.next().await {
        let local_recv_us = chrono::Utc::now().timestamp_micros();

        match msg? {
            Message::Text(text) => {
                let text_str: &str = text.as_ref();

                // Record raw
                let raw = RecordedEvent {
                    local_recv_us,
                    source: "binance_l2".to_string(),
                    event_type: "bookticker".to_string(),
                    raw_json: text_str.to_string(),
                };
                let _ = tx.send(BinanceEvent::Raw(raw));

                if let Ok(bt) = parse_bookticker(text_str, local_recv_us) {
                    // Emit mid price
                    let mid = (bt.best_bid + bt.best_ask) / 2.0;
                    let _ = tx.send(BinanceEvent::MidPrice(RefPrice {
                        source: RefSource::BinanceBookTicker,
                        symbol: "btcusdt".to_string(),
                        price: mid,
                        timestamp: bt.transaction_time,
                        local_recv_ts: local_recv_us,
                    }));
                    let _ = tx.send(BinanceEvent::BookTicker(bt));
                }
            }
            Message::Ping(data) => {
                write.send(Message::Pong(data)).await?;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

fn parse_depth(text: &str, local_recv_us: i64) -> Result<BinanceDepthUpdate, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;

    let parse_levels = |key: &str| -> Vec<PriceLevel> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter().filter_map(|level| {
                    let arr = level.as_array()?;
                    let price = arr.first()?.as_str()?.parse::<f64>().ok()?;
                    let size = arr.get(1)?.as_str()?.parse::<f64>().ok()?;
                    Some(PriceLevel { price, size })
                }).collect()
            })
            .unwrap_or_default()
    };

    Ok(BinanceDepthUpdate {
        event_time: v.get("E").and_then(|e| e.as_i64()).unwrap_or(0),
        transaction_time: v.get("T").and_then(|t| t.as_i64()).unwrap_or(0),
        first_update_id: v.get("U").and_then(|u| u.as_u64()).unwrap_or(0),
        last_update_id: v.get("u").and_then(|u| u.as_u64()).unwrap_or(0),
        bids: parse_levels("b"),
        asks: parse_levels("a"),
        local_recv_ts: local_recv_us,
    })
}

fn parse_bookticker(text: &str, local_recv_us: i64) -> Result<BinanceBookTicker, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;

    Ok(BinanceBookTicker {
        symbol: v.get("s").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        best_bid: v.get("b").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        best_bid_qty: v.get("B").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        best_ask: v.get("a").and_then(|a| a.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        best_ask_qty: v.get("A").and_then(|a| a.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        event_time: v.get("E").and_then(|e| e.as_i64()).unwrap_or(0),
        transaction_time: v.get("T").and_then(|t| t.as_i64()).unwrap_or(0),
        local_recv_ts: local_recv_us,
    })
}
