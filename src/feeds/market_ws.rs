//! Polymarket Market WebSocket — real-time token prices.
//!
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/market
//! Subscribes with {"assets_ids": [...], "type": "market"}
//! Sends text "PING" every 8s (NOT WebSocket control ping).
//! Parses price_change events for best_bid/best_ask.
//! Falls back to REST poller if WS dies.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

use crate::types_v2::{LogEvent, MarketDescriptor, PolymarketTop};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const HEARTBEAT_SECS: u64 = 8;
const RECONNECT_DELAY_MS: u64 = 1000;

pub async fn run_market_ws_task(
    market_rx: watch::Receiver<MarketDescriptor>,
    _tx: broadcast::Sender<PolymarketTop>,
    latest_tx: watch::Sender<PolymarketTop>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    // Track latest prices — updated on every price_change event
    let mut up_ask: Option<f64> = None;
    let mut up_bid: Option<f64> = None;
    let mut dn_ask: Option<f64> = None;
    let mut dn_bid: Option<f64> = None;
    let mut events_received: u64 = 0;

    loop {
        // Get current token IDs
        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() || market.down_token_id.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        let up_token = market.up_token_id.clone();
        let dn_token = market.down_token_id.clone();

        info!("market_ws: connecting to {}", WS_URL);

        let ws_result = connect_async(WS_URL).await;
        let ws_stream = match ws_result {
            Ok((stream, _)) => {
                info!("market_ws: connected!");
                stream
            }
            Err(e) => {
                error!("market_ws: connect failed: {} — retrying in {}ms", e, RECONNECT_DELAY_MS);
                tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        };

        // Split read and write — critical for heartbeat not to starve
        let (mut write, mut read) = ws_stream.split();

        // Send initial subscribe with custom_feature_enabled for best_bid_ask events
        let sub_msg = json!({
            "assets_ids": [&up_token, &dn_token],
            "type": "market",
            "custom_feature_enabled": true
        });

        if let Err(e) = write.send(Message::Text(sub_msg.to_string())).await {
            error!("market_ws: subscribe send failed: {}", e);
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            continue;
        }
        info!("market_ws: subscribed to UP={} DN={}", &up_token[..16.min(up_token.len())], &dn_token[..16.min(dn_token.len())]);

        // Channel for outbound messages (pong replies)
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

        // Writer task — owns heartbeat + outbound messages
        let writer_handle = tokio::spawn(async move {
            let mut hb = interval(Duration::from_secs(HEARTBEAT_SECS));
            loop {
                tokio::select! {
                    _ = hb.tick() => {
                        // TEXT "PING" — not WebSocket control ping!
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

        // Reader loop
        let mut alive = true;
        while alive {
            // Also check if market changed (new window) — need to resubscribe
            let current_market = market_rx.borrow().clone();
            if current_market.up_token_id != up_token || current_market.down_token_id != dn_token {
                info!("market_ws: window changed — reconnecting with new tokens");
                break;
            }

            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            // Ignore PONG heartbeat response
                            if text == "PONG" || text == "pong" {
                                continue;
                            }

                            // Parse JSON event
                            if let Ok(data) = serde_json::from_str::<Value>(&text) {
                                events_received += 1;

                                // Determine event type
                                let event_type = data.get("event_type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                let mut updated = false;

                                // ── FORMAT 1: price_change with nested price_changes[] array ──
                                // Doc format: {"market":"...","price_changes":[{"asset_id":"...","price":"0.5","size":"200","side":"BUY","best_bid":"0.5","best_ask":"1"}],"timestamp":"...","event_type":"price_change"}
                                if event_type == "price_change" || data.get("price_changes").is_some() {
                                    if let Some(changes) = data.get("price_changes").and_then(|v| v.as_array()) {
                                        for change in changes {
                                            let aid = change.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
                                            let best_ask_val = change.get("best_ask")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok());
                                            let best_bid_val = change.get("best_bid")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok());

                                            if aid == up_token {
                                                if let Some(a) = best_ask_val { up_ask = Some(a); updated = true; }
                                                if let Some(b) = best_bid_val { up_bid = Some(b); updated = true; }
                                            } else if aid == dn_token {
                                                if let Some(a) = best_ask_val { dn_ask = Some(a); updated = true; }
                                                if let Some(b) = best_bid_val { dn_bid = Some(b); updated = true; }
                                            }
                                        }
                                    }
                                }

                                // ── FORMAT 2: best_bid_ask event (requires custom_feature_enabled) ──
                                if event_type == "best_bid_ask" {
                                    if let Some(changes) = data.get("changes").and_then(|v| v.as_array()) {
                                        for change in changes {
                                            let aid = change.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
                                            let best_ask_val = change.get("best_ask")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok());
                                            let best_bid_val = change.get("best_bid")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse::<f64>().ok());

                                            if aid == up_token {
                                                if let Some(a) = best_ask_val { up_ask = Some(a); updated = true; }
                                                if let Some(b) = best_bid_val { up_bid = Some(b); updated = true; }
                                            } else if aid == dn_token {
                                                if let Some(a) = best_ask_val { dn_ask = Some(a); updated = true; }
                                                if let Some(b) = best_bid_val { dn_bid = Some(b); updated = true; }
                                            }
                                        }
                                    }
                                }

                                // ── FORMAT 3: top-level asset_id (older/fallback format) ──
                                if !updated {
                                    if let Some(aid) = data.get("asset_id").and_then(|v| v.as_str()) {
                                        let best_ask_val = data.get("best_ask")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<f64>().ok());
                                        let best_bid_val = data.get("best_bid")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<f64>().ok());

                                        if aid == up_token {
                                            if let Some(a) = best_ask_val { up_ask = Some(a); updated = true; }
                                            if let Some(b) = best_bid_val { up_bid = Some(b); updated = true; }
                                        } else if aid == dn_token {
                                            if let Some(a) = best_ask_val { dn_ask = Some(a); updated = true; }
                                            if let Some(b) = best_bid_val { dn_bid = Some(b); updated = true; }
                                        }
                                    }
                                }

                                // ── FORMAT 4: book snapshot — "asks" and "bids" arrays ──
                                if !updated {
                                    if let (Some(asks), Some(bids)) = (
                                        data.get("asks").and_then(|v| v.as_array()),
                                        data.get("bids").and_then(|v| v.as_array()),
                                    ) {
                                        let book_asset = data.get("asset_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");

                                        let best_ask = asks.iter()
                                            .filter_map(|a| a.get("price")?.as_str()?.parse::<f64>().ok())
                                            .reduce(f64::min);
                                        let best_bid = bids.iter()
                                            .filter_map(|b| b.get("price")?.as_str()?.parse::<f64>().ok())
                                            .reduce(f64::max);

                                        if book_asset == up_token {
                                            if let Some(a) = best_ask { up_ask = Some(a); updated = true; }
                                            if let Some(b) = best_bid { up_bid = Some(b); updated = true; }
                                        } else if book_asset == dn_token {
                                            if let Some(a) = best_ask { dn_ask = Some(a); updated = true; }
                                            if let Some(b) = best_bid { dn_bid = Some(b); updated = true; }
                                        }
                                    }
                                }

                                // ── FORMAT 5: last_trade_price fallback ──
                                if !updated {
                                    if let Some(aid) = data.get("asset_id").and_then(|v| v.as_str()) {
                                        if let Some(price) = data.get("price")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<f64>().ok())
                                        {
                                            if aid == up_token && up_ask.is_none() { up_ask = Some(price); updated = true; }
                                            else if aid == dn_token && dn_ask.is_none() { dn_ask = Some(price); updated = true; }
                                        }
                                    }
                                }

                                // Update shared state if anything changed
                                if updated {
                                    let top = PolymarketTop {
                                        recv_ts_ms: chrono::Utc::now().timestamp_millis(),
                                        up_ask,
                                        up_bid,
                                        down_ask: dn_ask,
                                        down_bid: dn_bid,
                                    };
                                    let _ = latest_tx.send(top);
                                }

                                // Log first 3 events then every 500th — enough to see prices updating
                                if events_received <= 3 || events_received % 500 == 0 {
                                    info!("market_ws: #{} | UP ask={} bid={} | DN ask={} bid={}",
                                        events_received,
                                        up_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                                        up_bid.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                                        dn_ask.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                                        dn_bid.map_or("?".into(), |v| format!("{:.0}c", v * 100.0)),
                                    );
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            // Reply to control pings too, just in case
                            let _ = out_tx.send(Message::Pong(payload));
                        }
                        Some(Ok(Message::Close(_))) => {
                            warn!("market_ws: server sent Close");
                            alive = false;
                        }
                        Some(Err(e)) => {
                            warn!("market_ws: read error: {}", e);
                            alive = false;
                        }
                        None => {
                            warn!("market_ws: stream ended");
                            alive = false;
                        }
                        _ => {}
                    }
                }
                // Check for window change every 1s
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }

        writer_handle.abort();
        warn!("market_ws: disconnected after {} events — reconnecting in {}ms", events_received, RECONNECT_DELAY_MS);
        tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)).await;

        // Reset prices on reconnect
        up_ask = None;
        up_bid = None;
        dn_ask = None;
        dn_bid = None;
    }
}
