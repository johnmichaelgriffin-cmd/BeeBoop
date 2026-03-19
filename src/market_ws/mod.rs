//! Polymarket CLOB Market WebSocket ingestion.
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/market`
//! and streams: book, price_change, best_bid_ask, last_trade_price, tick_size_change.
//!
//! This module is PASSIVE — it normalizes events and sends them to channels.
//! It makes no strategy decisions.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types::{BBO, OrderBook, PriceLevel, RecordedEvent, TickSize, Trade};

const MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Events emitted by the market WebSocket.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    Book(OrderBook),
    BestBidAsk(BBO),
    LastTradePrice(Trade),
    TickSizeChange(TickSize),
    PriceChange { token_id: String, price: f64, timestamp: i64 },
    Raw(RecordedEvent),
}

/// Subscription request for market channel.
#[derive(Debug, Serialize)]
struct SubscribeMsg {
    r#type: String,
    assets_ids: Vec<String>,
}

/// Run the market WebSocket connection loop.
/// Automatically reconnects on disconnect.
pub async fn run(
    token_ids: Vec<String>,
    tx: mpsc::UnboundedSender<MarketEvent>,
) {
    loop {
        info!("market_ws: connecting to {}", MARKET_WS_URL);
        match connect_and_stream(&token_ids, &tx).await {
            Ok(()) => warn!("market_ws: connection closed cleanly, reconnecting..."),
            Err(e) => error!("market_ws: connection error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_and_stream(
    token_ids: &[String],
    tx: &mpsc::UnboundedSender<MarketEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(MARKET_WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("market_ws: connected, subscribing to {} tokens", token_ids.len());

    // Subscribe to all event types for our tokens
    let sub = SubscribeMsg {
        r#type: "subscribe".to_string(),
        assets_ids: token_ids.to_vec(),
    };
    let sub_json = serde_json::to_string(&sub)?;
    write.send(Message::Text(sub_json.into())).await?;

    info!("market_ws: subscribed");

    while let Some(msg) = read.next().await {
        let local_recv_us = chrono::Utc::now().timestamp_micros();

        match msg? {
            Message::Text(text) => {
                let text_str: &str = text.as_ref();

                // Record raw event
                let raw = RecordedEvent {
                    local_recv_us,
                    source: "market_ws".to_string(),
                    event_type: "raw".to_string(),
                    raw_json: text_str.to_string(),
                };
                let _ = tx.send(MarketEvent::Raw(raw));

                // Parse and dispatch
                if let Err(e) = parse_and_dispatch(text_str, local_recv_us, tx) {
                    warn!("market_ws: parse error: {}", e);
                }
            }
            Message::Ping(data) => {
                write.send(Message::Pong(data)).await?;
            }
            Message::Close(_) => {
                info!("market_ws: received close frame");
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Parse a market WS message and send typed events.
fn parse_and_dispatch(
    text: &str,
    local_recv_us: i64,
    tx: &mpsc::UnboundedSender<MarketEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let v: serde_json::Value = serde_json::from_str(text)?;

    // Market WS sends arrays of events
    let events = if v.is_array() {
        v.as_array().unwrap().clone()
    } else {
        vec![v]
    };

    for event in events {
        let event_type = event.get("event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("");

        match event_type {
            "book" => {
                if let Ok(book) = parse_book(&event, local_recv_us) {
                    let _ = tx.send(MarketEvent::Book(book));
                }
            }
            "best_bid_ask" => {
                if let Ok(bbo) = parse_bbo(&event, local_recv_us) {
                    let _ = tx.send(MarketEvent::BestBidAsk(bbo));
                }
            }
            "last_trade_price" => {
                if let Ok(trade) = parse_trade(&event, local_recv_us) {
                    let _ = tx.send(MarketEvent::LastTradePrice(trade));
                }
            }
            "tick_size_change" => {
                if let Ok(ts) = parse_tick_size(&event) {
                    info!("market_ws: tick_size_change for {}: {}", ts.token_id, ts.tick_size);
                    let _ = tx.send(MarketEvent::TickSizeChange(ts));
                }
            }
            "price_change" => {
                let token_id = event.get("asset_id")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                let price = event.get("price")
                    .and_then(|p| p.as_f64())
                    .unwrap_or(0.0);
                let timestamp = event.get("timestamp")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0);
                let _ = tx.send(MarketEvent::PriceChange { token_id, price, timestamp });
            }
            _ => {
                // Unknown event type — logged via raw recording
            }
        }
    }

    Ok(())
}

fn parse_book(v: &serde_json::Value, local_recv_us: i64) -> Result<OrderBook, String> {
    let token_id = v.get("asset_id").and_then(|a| a.as_str()).ok_or("missing asset_id")?.to_string();
    let timestamp = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

    let parse_levels = |key: &str| -> Vec<PriceLevel> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter().filter_map(|level| {
                    let price = level.get("price").and_then(|p| p.as_str())?.parse::<f64>().ok()?;
                    let size = level.get("size").and_then(|s| s.as_str())?.parse::<f64>().ok()?;
                    Some(PriceLevel { price, size })
                }).collect()
            })
            .unwrap_or_default()
    };

    Ok(OrderBook {
        token_id,
        bids: parse_levels("bids"),
        asks: parse_levels("asks"),
        timestamp,
        local_recv_ts: local_recv_us,
    })
}

fn parse_bbo(v: &serde_json::Value, local_recv_us: i64) -> Result<BBO, String> {
    let token_id = v.get("asset_id").and_then(|a| a.as_str()).ok_or("missing asset_id")?.to_string();
    let timestamp = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

    Ok(BBO {
        token_id,
        best_bid: v.get("best_bid").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()),
        best_ask: v.get("best_ask").and_then(|a| a.as_str()).and_then(|s| s.parse().ok()),
        bid_size: v.get("best_bid_size").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()),
        ask_size: v.get("best_ask_size").and_then(|a| a.as_str()).and_then(|s| s.parse().ok()),
        timestamp,
        local_recv_ts: local_recv_us,
    })
}

fn parse_trade(v: &serde_json::Value, local_recv_us: i64) -> Result<Trade, String> {
    let token_id = v.get("asset_id").and_then(|a| a.as_str()).ok_or("missing asset_id")?.to_string();
    let price = v.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let size = v.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let side = v.get("side").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let timestamp = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

    Ok(Trade { token_id, price, size, side, timestamp, local_recv_ts: local_recv_us })
}

fn parse_tick_size(v: &serde_json::Value) -> Result<TickSize, String> {
    let token_id = v.get("asset_id").and_then(|a| a.as_str()).ok_or("missing asset_id")?.to_string();
    let tick_size = v.get("tick_size")
        .and_then(|t| t.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or("missing tick_size")?;
    Ok(TickSize { token_id, tick_size })
}
