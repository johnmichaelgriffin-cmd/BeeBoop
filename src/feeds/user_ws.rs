//! Polymarket User WebSocket — authenticated feed for order/trade events.
//!
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/user
//! Subscribes with auth credentials to receive:
//! - trade lifecycle: MATCHED -> MINED -> CONFIRMED
//! - order placements, updates, cancellations
//!
//! This is how we learn that our GTC maker bids got filled.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

const USER_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
const HEARTBEAT_SECS: u64 = 8;
const RECONNECT_DELAY_MS: u64 = 2000;

/// A fill event from the user channel
#[derive(Debug, Clone)]
pub struct UserFillEvent {
    pub order_id: String,
    pub asset_id: String,    // token_id
    pub side: String,        // "BUY" or "SELL"
    pub price: f64,
    pub size: f64,
    pub status: String,      // "MATCHED", "MINED", "CONFIRMED"
    pub ts_ms: i64,
}

/// Run the authenticated user WebSocket.
/// Emits UserFillEvent for every trade/fill on our orders.
pub async fn run_user_ws_task(
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    market_rx: tokio::sync::watch::Receiver<crate::types_v2::MarketDescriptor>,
    fill_tx: mpsc::Sender<UserFillEvent>,
) {
    loop {
        // Get current market condition ID for subscription
        let market = market_rx.borrow().clone();
        let condition_id = market.condition_id.clone();

        info!("user_ws: connecting (condition={})...",
            if condition_id.is_empty() { "none".to_string() } else { condition_id[..16.min(condition_id.len())].to_string() });

        let ws_result = connect_async(USER_WS_URL).await;
        let (ws_stream, _) = match ws_result {
            Ok(s) => s,
            Err(e) => {
                error!("user_ws: connect failed: {} — retrying in {}ms", e, RECONNECT_DELAY_MS);
                tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        };

        let (mut write, mut read) = ws_stream.split();

        // Authenticate on the user channel with specific market condition
        let markets_array = if condition_id.is_empty() {
            vec![]
        } else {
            vec![condition_id.clone()]
        };

        let auth_msg = json!({
            "auth": {
                "apiKey": api_key,
                "secret": api_secret,
                "passphrase": api_passphrase,
            },
            "type": "user",
            "markets": markets_array,
        });

        if let Err(e) = write.send(Message::Text(auth_msg.to_string())).await {
            error!("user_ws: auth send failed: {}", e);
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }

        info!("user_ws: connected and authenticated");

        // Writer task — text "PING" heartbeat
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            let mut hb = interval(Duration::from_secs(HEARTBEAT_SECS));
            loop {
                tokio::select! {
                    _ = hb.tick() => {
                        if write.send(Message::Text("PING".into())).await.is_err() {
                            break;
                        }
                    }
                    Some(msg) = out_rx.recv() => {
                        if write.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut events_received: u64 = 0;

        // Reader loop
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if text == "PONG" { continue; }

                    if let Ok(data) = serde_json::from_str::<Value>(&text) {
                        events_received += 1;

                        // Log first few for debugging
                        if events_received <= 5 {
                            info!("user_ws: event #{} — {}", events_received, &text[..200.min(text.len())]);
                        }

                        // Parse trade events
                        // Format: {"event_type": "trade", "asset_id": "...", "price": "0.50", "size": "20", "side": "BUY", "status": "MATCHED", ...}
                        let event_type = data.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

                        if event_type == "trade" || event_type == "order" {
                            let order_id = data.get("id")
                                .or_else(|| data.get("order_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            let asset_id = data.get("asset_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            let side = data.get("side")
                                .and_then(|v| v.as_str())
                                .unwrap_or("BUY")
                                .to_string();

                            let price = data.get("price")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);

                            let size = data.get("size")
                                .or_else(|| data.get("size_matched"))
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);

                            let status = data.get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("UNKNOWN")
                                .to_string();

                            // Only emit on MATCHED — do NOT count MINED/CONFIRMED again
                            // The same trade emits MATCHED -> MINED -> CONFIRMED lifecycle.
                            // Counting all three would 2x-3x inflate inventory.
                            if size > 0.0 && status == "MATCHED" {
                                let fill = UserFillEvent {
                                    order_id,
                                    asset_id,
                                    side,
                                    price,
                                    size,
                                    status,
                                    ts_ms: chrono::Utc::now().timestamp_millis(),
                                };
                                let _ = fill_tx.send(fill).await;
                            }
                        }
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = out_tx.send(Message::Pong(payload));
                }
                Ok(Message::Close(_)) => {
                    warn!("user_ws: server closed connection");
                    break;
                }
                Err(e) => {
                    error!("user_ws: read error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        writer.abort();
        warn!("user_ws: disconnected ({} events received) — reconnecting in {}ms", events_received, RECONNECT_DELAY_MS);
        tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
    }
}
