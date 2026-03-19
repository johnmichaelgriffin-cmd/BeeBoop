//! Polymarket CLOB User WebSocket — order/trade lifecycle tracking.
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/user`
//! Tracks order placements, updates, cancellations, and trade status
//! transitions (MATCHED → MINED → CONFIRMED).
//!
//! This module is PASSIVE — it does not place or cancel orders.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types::{OrderStatus, RecordedEvent};

const USER_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";

/// Events from the user channel.
#[derive(Debug, Clone)]
pub enum UserEvent {
    OrderUpdate {
        order_id: String,
        status: OrderStatus,
        filled_size: f64,
        timestamp: i64,
        local_recv_us: i64,
    },
    TradeUpdate {
        trade_id: String,
        order_id: String,
        status: String,    // MATCHED, MINED, CONFIRMED, etc.
        price: f64,
        size: f64,
        timestamp: i64,
        local_recv_us: i64,
    },
    Raw(RecordedEvent),
}

/// Run user WS with auto-reconnect. Requires API auth.
pub async fn run(
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    tx: mpsc::UnboundedSender<UserEvent>,
) {
    loop {
        info!("user_ws: connecting");
        match connect_and_stream(&api_key, &api_secret, &api_passphrase, &tx).await {
            Ok(()) => warn!("user_ws: disconnected, reconnecting..."),
            Err(e) => error!("user_ws: error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_and_stream(
    api_key: &str,
    _api_secret: &str,
    _api_passphrase: &str,
    tx: &mpsc::UnboundedSender<UserEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(USER_WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    // Authenticate
    // The user channel requires an Auth message on connect
    let auth_msg = serde_json::json!({
        "type": "Auth",
        "apiKey": api_key,
    });
    write.send(Message::Text(auth_msg.to_string().into())).await?;

    info!("user_ws: connected and authenticated");

    while let Some(msg) = read.next().await {
        let local_recv_us = chrono::Utc::now().timestamp_micros();

        match msg? {
            Message::Text(text) => {
                let text_str: &str = text.as_ref();

                let raw = RecordedEvent {
                    local_recv_us,
                    source: "user_ws".to_string(),
                    event_type: "raw".to_string(),
                    raw_json: text_str.to_string(),
                };
                let _ = tx.send(UserEvent::Raw(raw));

                if let Err(e) = parse_and_dispatch(text_str, local_recv_us, tx) {
                    warn!("user_ws: parse error: {}", e);
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

fn parse_and_dispatch(
    text: &str,
    local_recv_us: i64,
    tx: &mpsc::UnboundedSender<UserEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let v: serde_json::Value = serde_json::from_str(text)?;

    let events = if v.is_array() { v.as_array().unwrap().clone() } else { vec![v] };

    for event in events {
        let event_type = event.get("event_type").and_then(|e| e.as_str()).unwrap_or("");

        match event_type {
            "order" => {
                let order_id = event.get("order_id").and_then(|o| o.as_str()).unwrap_or("").to_string();
                let status_str = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let status = match status_str {
                    "PLACEMENT" => OrderStatus::Placed,
                    "CANCELLATION" => OrderStatus::Cancelled,
                    "MATCHED" => OrderStatus::Matched,
                    _ => OrderStatus::Pending,
                };
                let filled_size = event.get("filled_size")
                    .and_then(|f| f.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let timestamp = event.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

                let _ = tx.send(UserEvent::OrderUpdate {
                    order_id, status, filled_size, timestamp, local_recv_us,
                });
            }
            "trade" => {
                let trade_id = event.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let order_id = event.get("order_id").and_then(|o| o.as_str()).unwrap_or("").to_string();
                let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
                let price = event.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let size = event.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let timestamp = event.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

                let _ = tx.send(UserEvent::TradeUpdate {
                    trade_id, order_id, status, price, size, timestamp, local_recv_us,
                });
            }
            _ => {}
        }
    }

    Ok(())
}
