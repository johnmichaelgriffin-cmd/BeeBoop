//! Polymarket token price feed — WebSocket primary, REST fallback.
//!
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/market
//! Subscribes to price_change events for UP and DOWN tokens.
//! Falls back to REST /price polling if WS disconnects.

use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use tracing::{error, info, warn};

use crate::types_v2::{LogEvent, MarketDescriptor, PolymarketTop};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CLOB_BASE: &str = "https://clob.polymarket.com";
const REST_POLL_MS: u64 = 500; // fallback only

pub async fn run_polymarket_ws_task(
    market_rx: watch::Receiver<MarketDescriptor>,
    tx: broadcast::Sender<PolymarketTop>,
    latest_tx: watch::Sender<PolymarketTop>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    // Shared latest prices — updated by WS or REST
    let up_ask = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let up_bid = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dn_ask = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dn_bid = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    let rest_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1000))
        .build()
        .expect("http client");

    let mut last_window_ts: i64 = 0;
    let mut ws_connected = false;

    loop {
        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() || market.down_token_id.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        }

        // Detect window change — need to resubscribe
        let new_window = market.window_start_ts != last_window_ts;
        if new_window {
            last_window_ts = market.window_start_ts;
            ws_connected = false; // force reconnect
        }

        if !ws_connected {
            info!("polymarket_ws: connecting to WebSocket...");
            match connect_ws(&market, &up_ask, &up_bid, &dn_ask, &dn_bid, &tx, &latest_tx, &market_rx).await {
                Ok(()) => {
                    // WS loop ended (disconnect or window change)
                    ws_connected = false;
                    warn!("polymarket_ws: WebSocket disconnected, will reconnect");
                }
                Err(e) => {
                    warn!("polymarket_ws: WebSocket failed: {} — falling back to REST", e);
                    ws_connected = false;
                    // Fall back to REST for a bit
                    rest_poll_burst(&rest_client, &market, &tx, &latest_tx, 10).await;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn connect_ws(
    market: &MarketDescriptor,
    up_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    up_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: &broadcast::Sender<PolymarketTop>,
    latest_tx: &watch::Sender<PolymarketTop>,
    market_rx: &watch::Receiver<MarketDescriptor>,
) -> Result<(), String> {
    let (ws_stream, _) = connect_async(WS_URL)
        .await
        .map_err(|e| format!("connect: {}", e))?;

    let (mut write, mut read) = ws_stream.split();

    // Subscribe to both tokens
    let assets: Vec<String> = vec![
        market.up_token_id.clone(),
        market.down_token_id.clone(),
    ];

    let sub_msg = serde_json::json!({
        "type": "market",
        "assets_ids": assets,
    });

    write.send(Message::Text(sub_msg.to_string()))
        .await
        .map_err(|e| format!("subscribe: {}", e))?;

    info!("polymarket_ws: subscribed to {} tokens via WebSocket", assets.len());

    let up_token = market.up_token_id.clone();
    let dn_token = market.down_token_id.clone();
    let window_start = market.window_start_ts;

    // Ping every 10s to keep alive
    let write = std::sync::Arc::new(tokio::sync::Mutex::new(write));
    let write_ping = write.clone();
    let ping_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let mut w = write_ping.lock().await;
            if w.send(Message::Ping(vec![])).await.is_err() {
                break;
            }
        }
    });

    // Read messages
    while let Some(msg) = read.next().await {
        // Check for window change
        let current_market = market_rx.borrow().clone();
        if current_market.window_start_ts != window_start {
            info!("polymarket_ws: window changed, disconnecting to resubscribe");
            break;
        }

        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("polymarket_ws: read error: {}", e);
                break;
            }
        };

        let text = match msg {
            Message::Text(t) => t,
            Message::Pong(_) => continue,
            Message::Ping(d) => {
                let mut w = write.lock().await;
                let _ = w.send(Message::Pong(d)).await;
                continue;
            }
            Message::Close(_) => {
                info!("polymarket_ws: server closed connection");
                break;
            }
            _ => continue,
        };

        // Parse the event
        if let Ok(events) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
            for event in &events {
                process_ws_event(event, &up_token, &dn_token, up_ask, up_bid, dn_ask, dn_bid);
            }
        } else if let Ok(event) = serde_json::from_str::<serde_json::Value>(&text) {
            process_ws_event(&event, &up_token, &dn_token, up_ask, up_bid, dn_ask, dn_bid);
        }

        // Publish updated prices
        let now_ms = chrono::Utc::now().timestamp_millis();
        let top = build_top(now_ms, up_ask, up_bid, dn_ask, dn_bid);
        let _ = latest_tx.send(top.clone());
        let _ = tx.send(top);
    }

    ping_handle.abort();
    Ok(())
}

fn process_ws_event(
    event: &serde_json::Value,
    up_token: &str,
    dn_token: &str,
    up_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    up_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    // Handle price_change events: {"asset_id": "...", "price": "0.55", "side": "BUY"|"SELL", ...}
    // Also handle best_bid_ask events
    let asset_id = event.get("asset_id")
        .or_else(|| event.get("token_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let price_str = event.get("price")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let price = match price_str.parse::<f64>() {
        Ok(p) if p > 0.0 => p,
        _ => return,
    };

    // Determine if this is bid or ask update
    let side = event.get("side")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let price_bits = price.to_bits();

    if asset_id == up_token {
        match side {
            "BUY" | "buy" => { up_bid.store(price_bits, std::sync::atomic::Ordering::Relaxed); }
            "SELL" | "sell" => { up_ask.store(price_bits, std::sync::atomic::Ordering::Relaxed); }
            _ => {
                // best_bid_ask or price_change without side — check for best_ask/best_bid fields
                if let Some(ba) = event.get("best_ask").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                    up_ask.store(ba.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(bb) = event.get("best_bid").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                    up_bid.store(bb.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                // If it's just a last_trade_price or similar, use as a mid estimate
                if side.is_empty() && event.get("best_ask").is_none() {
                    // Could be a generic price update — use as both ask and bid approximation
                    up_ask.store(price_bits, std::sync::atomic::Ordering::Relaxed);
                    up_bid.store(price_bits, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    } else if asset_id == dn_token {
        match side {
            "BUY" | "buy" => { dn_bid.store(price_bits, std::sync::atomic::Ordering::Relaxed); }
            "SELL" | "sell" => { dn_ask.store(price_bits, std::sync::atomic::Ordering::Relaxed); }
            _ => {
                if let Some(ba) = event.get("best_ask").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                    dn_ask.store(ba.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(bb) = event.get("best_bid").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                    dn_bid.store(bb.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                if side.is_empty() && event.get("best_ask").is_none() {
                    dn_ask.store(price_bits, std::sync::atomic::Ordering::Relaxed);
                    dn_bid.store(price_bits, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

fn build_top(
    now_ms: i64,
    up_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    up_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_ask: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    dn_bid: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> PolymarketTop {
    let to_f64 = |v: u64| -> Option<f64> {
        if v == 0 { None } else { Some(f64::from_bits(v)) }
    };

    PolymarketTop {
        recv_ts_ms: now_ms,
        up_ask: to_f64(up_ask.load(std::sync::atomic::Ordering::Relaxed)),
        up_bid: to_f64(up_bid.load(std::sync::atomic::Ordering::Relaxed)),
        down_ask: to_f64(dn_ask.load(std::sync::atomic::Ordering::Relaxed)),
        down_bid: to_f64(dn_bid.load(std::sync::atomic::Ordering::Relaxed)),
    }
}

/// REST fallback — poll a few times when WS is down
async fn rest_poll_burst(
    client: &reqwest::Client,
    market: &MarketDescriptor,
    tx: &broadcast::Sender<PolymarketTop>,
    latest_tx: &watch::Sender<PolymarketTop>,
    count: u32,
) {
    for _ in 0..count {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (ua, ub, da, db) = tokio::join!(
            fetch_price(client, &market.up_token_id, "buy"),
            fetch_price(client, &market.up_token_id, "sell"),
            fetch_price(client, &market.down_token_id, "buy"),
            fetch_price(client, &market.down_token_id, "sell"),
        );
        let top = PolymarketTop {
            recv_ts_ms: now_ms,
            up_ask: ua, up_bid: ub, down_ask: da, down_bid: db,
        };
        let _ = latest_tx.send(top.clone());
        let _ = tx.send(top);
        tokio::time::sleep(std::time::Duration::from_millis(REST_POLL_MS)).await;
    }
}

async fn fetch_price(client: &reqwest::Client, token_id: &str, side: &str) -> Option<f64> {
    let url = format!("{}/price?token_id={}&side={}", CLOB_BASE, token_id, side);
    let resp = client.get(&url)
        .header("Cache-Control", "no-cache, no-store")
        .send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse::<f64>().ok())
}
