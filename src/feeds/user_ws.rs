//! Polymarket User WebSocket — authenticated feed for order/trade events.
//!
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/user
//! Subscribes with auth + specific condition_id.
//! Parses maker_orders[] for precise fill attribution.
//! Dedupes fills to prevent inflation on reconnect/replay.
//! Reconnects on market window change.

use std::collections::HashSet;
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
    pub asset_id: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub status: String,
    pub ts_ms: i64,
}

/// Run the authenticated user WebSocket.
/// Watches market_rx for window changes — reconnects when condition_id changes.
/// Parses maker_orders[] for precise fill sizes.
/// Dedupes fills across reconnects.
pub async fn run_user_ws_task(
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    mut market_rx: tokio::sync::watch::Receiver<crate::types_v2::MarketDescriptor>,
    fill_tx: mpsc::Sender<UserFillEvent>,
) {
    // P0.3: Dedupe set — prevents double-counting fills across reconnects
    let mut seen_fills: HashSet<String> = HashSet::new();
    let mut current_condition_id = String::new();

    loop {
        // Get current market condition ID
        let market = market_rx.borrow().clone();
        let condition_id = market.condition_id.clone();

        // Wait for a valid condition_id before connecting
        if condition_id.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        // If condition changed, clear dedupe set for new window
        if condition_id != current_condition_id {
            if !current_condition_id.is_empty() {
                info!("user_ws: condition changed {} -> {} — clearing dedupe",
                    &current_condition_id[..16.min(current_condition_id.len())],
                    &condition_id[..16.min(condition_id.len())]);
            }
            seen_fills.clear();
            current_condition_id = condition_id.clone();
        }

        info!("user_ws: connecting (condition={})...",
            &condition_id[..16.min(condition_id.len())]);

        let ws_result = connect_async(USER_WS_URL).await;
        let (ws_stream, _) = match ws_result {
            Ok(s) => s,
            Err(e) => {
                error!("user_ws: connect failed: {} — retrying", e);
                tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        };

        let (mut write, mut read) = ws_stream.split();

        // Authenticate with specific market condition
        let auth_msg = json!({
            "auth": {
                "apiKey": api_key,
                "secret": api_secret,
                "passphrase": api_passphrase,
            },
            "type": "user",
            "markets": [&condition_id],
        });

        if let Err(e) = write.send(Message::Text(auth_msg.to_string())).await {
            error!("user_ws: auth send failed: {}", e);
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }

        info!("user_ws: connected and authenticated for condition {}", &condition_id[..16.min(condition_id.len())]);

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

        // P0.1: Reader loop with market_rx.changed() watch for resubscribe
        loop {
            tokio::select! {
                // Watch for market window change — reconnect if condition changes
                _ = market_rx.changed() => {
                    let new_market = market_rx.borrow().clone();
                    if new_market.condition_id != condition_id && !new_market.condition_id.is_empty() {
                        info!("user_ws: market changed — breaking to reconnect");
                        break; // will reconnect with new condition in outer loop
                    }
                }

                // Read WS messages
                msg = read.next() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        Ok(Message::Text(text)) => {
                            if text == "PONG" { continue; }

                            if let Ok(data) = serde_json::from_str::<Value>(&text) {
                                events_received += 1;

                                if events_received <= 5 {
                                    info!("user_ws: event #{} — {}", events_received, &text[..200.min(text.len())]);
                                }

                                let event_type = data.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

                                // P0.2: Parse maker_orders[] for precise fill attribution
                                if event_type == "trade" {
                                    let status = data.get("status")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("UNKNOWN");

                                    // Only process MATCHED status
                                    if status != "MATCHED" { continue; }

                                    let trade_id = data.get("id")
                                        .or_else(|| data.get("trade_id"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    let asset_id = data.get("asset_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    // Try maker_orders[] first — more precise for our fills
                                    let maker_orders = data.get("maker_orders")
                                        .and_then(|v| v.as_array());

                                    if let Some(orders) = maker_orders {
                                        for mo in orders {
                                            let order_id = mo.get("order_id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();

                                            let matched_amount = mo.get("matched_amount")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok())
                                                .unwrap_or(0.0);

                                            let price = mo.get("price")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok())
                                                .unwrap_or(0.0);

                                            if matched_amount <= 0.0 { continue; }

                                            // P0.3: Dedupe — unique key per maker fill
                                            let dedupe_key = format!("{}:{}:{:.4}:{:.2}",
                                                trade_id, order_id, matched_amount, price);

                                            if seen_fills.contains(&dedupe_key) {
                                                continue; // already counted
                                            }
                                            seen_fills.insert(dedupe_key);

                                            let fill = UserFillEvent {
                                                order_id,
                                                asset_id: asset_id.clone(),
                                                side: "BUY".to_string(), // we only post buys
                                                price,
                                                size: matched_amount,
                                                status: status.to_string(),
                                                ts_ms: chrono::Utc::now().timestamp_millis(),
                                            };
                                            let _ = fill_tx.send(fill).await;
                                        }
                                    } else {
                                        // Fallback: use top-level fields if no maker_orders
                                        let order_id = data.get("id")
                                            .or_else(|| data.get("order_id"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
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

                                        if size <= 0.0 { continue; }

                                        // Dedupe
                                        let dedupe_key = format!("{}:{}:{:.4}:{:.2}",
                                            trade_id, order_id, size, price);
                                        if seen_fills.contains(&dedupe_key) { continue; }
                                        seen_fills.insert(dedupe_key);

                                        let fill = UserFillEvent {
                                            order_id,
                                            asset_id,
                                            side: "BUY".to_string(),
                                            price,
                                            size,
                                            status: status.to_string(),
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
                            warn!("user_ws: server closed");
                            break;
                        }
                        Err(e) => {
                            error!("user_ws: read error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        writer.abort();
        warn!("user_ws: disconnected ({} events, {} unique fills) — reconnecting",
            events_received, seen_fills.len());
        tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
    }
}
