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
const GAMMA_API: &str = "https://gamma-api.polymarket.com";
const CLOB_API: &str = "https://clob.polymarket.com";

/// REST fallback: poll best ask/bid from CLOB /book endpoint.
/// Used when WebSocket is flaky.
pub async fn poll_book_rest(token_id: &str) -> Option<(f64, f64)> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/book", CLOB_API))
        .query(&[("token_id", token_id)])
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;

    let best_ask = body.get("asks")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|l| l.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse::<f64>().ok()))
                .reduce(f64::min)
        })?;

    let best_bid = body.get("bids")
        .and_then(|b| b.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|l| l.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse::<f64>().ok()))
                .reduce(f64::max)
        })?;

    Some((best_bid, best_ask))
}

/// Token info with UP/DN label.
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub token_id: String,
    pub outcome: String, // "Up" or "Down"
    pub window_ts: i64,
}

/// Fetch BTC 5-min market token IDs for current + next window from Gamma API.
/// Returns labeled tokens (UP/DN) so the strategy knows which side is which.
pub async fn fetch_btc_tokens() -> Result<(Vec<String>, Vec<TokenInfo>), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let now = chrono::Utc::now().timestamp();
    let wts = now - (now % 300);

    let mut token_ids = Vec::new();
    let mut token_infos = Vec::new();

    // Fetch current window + next window
    for ts in [wts, wts + 300] {
        let slug = format!("btc-updown-5m-{}", ts);
        let resp = client
            .get(format!("{}/events", GAMMA_API))
            .query(&[("slug", slug.as_str())])
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;

        let body = resp.text().await?;
        let events: serde_json::Value = serde_json::from_str(&body)?;

        if let Some(arr) = events.as_array() {
            for event in arr {
                if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                    for market in markets {
                        // outcomes and clobTokenIds are parallel JSON-encoded arrays
                        let outcomes_raw = market.get("outcomes")
                            .and_then(|o| o.as_str())
                            .unwrap_or("[]");
                        let tokens_raw = market.get("clobTokenIds")
                            .and_then(|t| t.as_str())
                            .unwrap_or("[]");

                        let outcomes: Vec<String> = serde_json::from_str(outcomes_raw).unwrap_or_default();
                        let tokens: Vec<String> = serde_json::from_str(tokens_raw).unwrap_or_default();

                        for (i, tid) in tokens.iter().enumerate() {
                            if !tid.is_empty() && !token_ids.contains(tid) {
                                let outcome = outcomes.get(i).cloned().unwrap_or_default();
                                token_ids.push(tid.clone());
                                token_infos.push(TokenInfo {
                                    token_id: tid.clone(),
                                    outcome,
                                    window_ts: ts,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    info!("market_ws: fetched {} tokens for windows {} and {}", token_ids.len(), wts, wts + 300);
    for ti in &token_infos {
        info!("market_ws:   {} = {}...{} (window {})",
            ti.outcome,
            &ti.token_id[..8.min(ti.token_id.len())],
            &ti.token_id[ti.token_id.len().saturating_sub(8)..],
            ti.window_ts,
        );
    }
    Ok((token_ids, token_infos))
}

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
    custom_feature_enabled: bool,
}

/// Run the market WebSocket connection loop.
/// Auto-fetches token IDs from Gamma API and reconnects on disconnect.
/// Re-fetches tokens on each reconnect to pick up new windows.
pub async fn run(
    _initial_token_ids: Vec<String>,
    tx: mpsc::UnboundedSender<MarketEvent>,
    token_info_tx: Option<mpsc::UnboundedSender<Vec<TokenInfo>>>,
) {
    loop {
        // Fetch fresh token IDs each connection cycle
        let (token_ids, token_infos) = match fetch_btc_tokens().await {
            Ok((ids, infos)) if !ids.is_empty() => {
                // Send token infos to main for shared state update
                if let Some(ref info_tx) = token_info_tx {
                    let _ = info_tx.send(infos.clone());
                }
                (ids, infos)
            }
            Ok(_) => {
                warn!("market_ws: no token IDs found, retrying in 10s...");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
            Err(e) => {
                error!("market_ws: failed to fetch token IDs: {}, retrying in 10s...", e);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
        };

        info!("market_ws: connecting to {} with {} tokens", MARKET_WS_URL, token_ids.len());
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
    for (i, tid) in token_ids.iter().enumerate() {
        info!("market_ws:   token[{}]: {}...{}", i, &tid[..8.min(tid.len())], &tid[tid.len().saturating_sub(8)..]);
    }

    // Subscribe to all event types for our tokens
    // Build subscription JSON manually to control exact field names
    let sub_json = serde_json::json!({
        "type": "market",
        "assets_ids": token_ids,
        "custom_feature_enabled": true,
    }).to_string();
    info!("market_ws: sending subscription: {}", &sub_json[..200.min(sub_json.len())]);
    write.send(Message::Text(sub_json.into())).await?;

    info!("market_ws: subscribed, listening for events...");

    // Spawn PING keepalive task (every 10s)
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            if ping_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    // Track window boundary — reconnect every 5 min to pick up new tokens
    let now = chrono::Utc::now().timestamp();
    let next_window = (now - (now % 300)) + 300;
    let reconnect_at = next_window + 5; // reconnect 5s into next window
    info!("market_ws: will reconnect at {} ({}s from now)", reconnect_at, reconnect_at - now);

    loop {
        // Check if we should reconnect for new window
        if chrono::Utc::now().timestamp() >= reconnect_at {
            info!("market_ws: new window — disconnecting to re-subscribe with fresh tokens");
            let _ = write.send(Message::Close(None)).await;
            break;
        }

        tokio::select! {
            // Handle PING keepalive
            _ = ping_rx.recv() => {
                let _ = write.send(Message::Ping(vec![].into())).await;
            }

            // Handle incoming messages with timeout
            msg = tokio::time::timeout(std::time::Duration::from_secs(15), read.next()) => {
                match msg {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        let local_recv_us = chrono::Utc::now().timestamp_micros();
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
                    Ok(Some(Ok(Message::Ping(data)))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => {
                        // Server responded to our PING — connection is alive
                    }
                    Ok(Some(Ok(Message::Close(_)))) => {
                        info!("market_ws: received close frame");
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        return Err(e.into());
                    }
                    Ok(None) => {
                        info!("market_ws: stream ended");
                        break;
                    }
                    Err(_) => {
                        warn!("market_ws: no data for 15s, reconnecting...");
                        break;
                    }
                    _ => {}
                }
            }
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
