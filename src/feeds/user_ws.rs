//! Polymarket User WebSocket — authenticated feed for order/trade events.
//!
//! TWO-STEP AUTH FLOW (per API reference):
//! 1. Send: {"auth":{...},"type":"user"}
//! 2. Send: {"operation":"subscribe","markets":["<condition_id>"]}
//!
//! PRIMARY fill tracking: order UPDATE events with size_matched deltas
//! SECONDARY: trade events as audit trail
//!
//! Reconnects on market window change.

use std::collections::HashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use chrono::Timelike;
use tracing::{error, info, warn};

const USER_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
const HEARTBEAT_SECS: u64 = 8;
const RECONNECT_DELAY_MS: u64 = 2000;

/// A fill event derived from order UPDATE size_matched deltas
#[derive(Debug, Clone)]
pub struct UserFillEvent {
    pub order_id: String,
    pub asset_id: String,
    pub side: String,
    pub price: f64,
    pub size: f64,       // delta: new fills since last update
    pub status: String,  // "ORDER_UPDATE" or "TRADE_MATCHED"
    pub ts_ms: i64,
}

/// Run the authenticated user WebSocket with two-step auth flow.
/// Tracks order size_matched deltas as primary fill signal.
pub async fn run_user_ws_task(
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    mut market_rx: tokio::sync::watch::Receiver<crate::types_v2::MarketDescriptor>,
    fill_tx: mpsc::Sender<UserFillEvent>,
) {
    let mut current_condition_id = String::new();

    loop {
        // Get current market condition ID
        let market = market_rx.borrow().clone();
        let condition_id = market.condition_id.clone();

        if condition_id.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        // Clear order tracking on condition change
        if condition_id != current_condition_id {
            if !current_condition_id.is_empty() {
                info!("user_ws: condition changed — reconnecting");
            }
            current_condition_id = condition_id.clone();
        }

        info!("user_ws: connecting (condition={})...",
            &condition_id[..16.min(condition_id.len())]);

        let ws_result = connect_async(USER_WS_URL).await;
        let (ws_stream, _) = match ws_result {
            Ok(s) => s,
            Err(e) => {
                error!("user_ws: connect failed: {}", e);
                tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        };

        let (mut write, mut read) = ws_stream.split();

        // ═══ TWO-STEP AUTH FLOW (per API reference) ═══

        // Step 1: Auth handshake
        let auth_msg = json!({
            "auth": {
                "apiKey": api_key,
                "secret": api_secret,
                "passphrase": api_passphrase,
            },
            "type": "user",
        });

        if let Err(e) = write.send(Message::Text(auth_msg.to_string())).await {
            error!("user_ws: auth send failed: {}", e);
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }

        // Brief pause for auth to process
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Step 2: Subscribe to specific market condition
        let sub_msg = json!({
            "operation": "subscribe",
            "markets": [&condition_id],
        });

        if let Err(e) = write.send(Message::Text(sub_msg.to_string())).await {
            error!("user_ws: subscribe send failed: {}", e);
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }

        info!("user_ws: authenticated + subscribed to {}", &condition_id[..16.min(condition_id.len())]);

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

        // ═══ ORDER TRACKING STATE ═══
        // Track size_matched per order_id — delta = fill
        let mut order_matched: HashMap<String, f64> = HashMap::new();
        let mut events_received: u64 = 0;
        let user_connect_time = chrono::Utc::now().timestamp();
        // Forced reconnect at the next :59 past the hour (e.g. 10:59, 11:59, ...)
        let user_reconnect_at: i64 = {
            let now = chrono::Utc::now();
            let secs_into_hour = (now.minute() as i64) * 60 + (now.second() as i64);
            let target_secs: i64 = 59 * 60; // :59:00 into the hour
            if secs_into_hour < target_secs {
                now.timestamp() + (target_secs - secs_into_hour)
            } else {
                now.timestamp() + (3600 - secs_into_hour + target_secs)
            }
        };

        // Reader loop with market change detection + forced reconnect
        loop {
            // Forced reconnect at :59 past the hour
            let now_secs = chrono::Utc::now().timestamp();
            if now_secs >= user_reconnect_at {
                info!("user_ws: FORCED RECONNECT at :59 mark ({}s since connect)", now_secs - user_connect_time);
                break;
            }

            tokio::select! {
                // Watch for market window change
                _ = market_rx.changed() => {
                    let new_market = market_rx.borrow().clone();
                    if new_market.condition_id != condition_id && !new_market.condition_id.is_empty() {
                        info!("user_ws: market changed — breaking to reconnect");
                        break;
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

                                // Log first events for debugging
                                if events_received <= 10 {
                                    info!("user_ws: event #{} — {}", events_received,
                                        &text[..300.min(text.len())]);
                                }

                                let event_type = data.get("event_type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                // ═══ PRIMARY: Order UPDATE events ═══
                                // Track size_matched deltas for fill accounting
                                if event_type == "order" {
                                    let order_type = data.get("type")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    let order_id = data.get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let asset_id = data.get("asset_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let price = data.get("price")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| s.parse::<f64>().ok())
                                        .unwrap_or(0.0);

                                    let size_matched = data.get("size_matched")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| s.parse::<f64>().ok())
                                        .unwrap_or(0.0);

                                    let side = data.get("side")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("BUY")
                                        .to_string();

                                    match order_type {
                                        "PLACEMENT" => {
                                            // Order placed — register with 0 matched
                                            order_matched.insert(order_id.clone(), 0.0);
                                        }
                                        "UPDATE" => {
                                            // Check for new fills via size_matched delta
                                            let prev_matched = order_matched.get(&order_id).copied().unwrap_or(0.0);
                                            let delta = size_matched - prev_matched;

                                            if delta > 0.01 {
                                                // New fill detected!
                                                order_matched.insert(order_id.clone(), size_matched);

                                                info!("user_ws: ORDER FILL: {} {:.1}sh @ {:.2} (delta={:.1}, total_matched={:.1})",
                                                    if side == "BUY" { "BUY" } else { "SELL" },
                                                    delta, price, delta, size_matched);

                                                let fill = UserFillEvent {
                                                    order_id,
                                                    asset_id,
                                                    side,
                                                    price,
                                                    size: delta, // only the NEW fill amount
                                                    status: "ORDER_UPDATE".to_string(),
                                                    ts_ms: chrono::Utc::now().timestamp_millis(),
                                                };
                                                let _ = fill_tx.send(fill).await;
                                            } else {
                                                // size_matched didn't increase — just a status update
                                                order_matched.insert(order_id, size_matched);
                                            }
                                        }
                                        "CANCELLATION" => {
                                            // Order cancelled — remove from tracking
                                            order_matched.remove(&order_id);
                                        }
                                        _ => {}
                                    }
                                }

                                // ═══ SECONDARY: Trade events as audit trail ═══
                                if event_type == "trade" {
                                    let status = data.get("status")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    // Only log MATCHED trades for visibility
                                    if status == "MATCHED" {
                                        let asset_id = data.get("asset_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let size = data.get("size")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<f64>().ok())
                                            .unwrap_or(0.0);
                                        let price = data.get("price")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<f64>().ok())
                                            .unwrap_or(0.0);

                                        info!("user_ws: TRADE: {:.1}sh @ {:.2} [{}] asset={}...",
                                            size, price, status,
                                            &asset_id[..16.min(asset_id.len())]);
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
        warn!("user_ws: disconnected ({} events, {} tracked orders) — reconnecting",
            events_received, order_matched.len());
        tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
    }
}
