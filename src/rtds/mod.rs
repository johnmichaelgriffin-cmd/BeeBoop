//! Polymarket Real-Time Data Socket (RTDS) ingestion.
//!
//! Connects to `wss://ws-live-data.polymarket.com`
//! Subscribes to:
//!   - crypto_prices (Binance reference prices)
//!   - crypto_prices_chainlink (Chainlink oracle prices)
//!
//! This module is PASSIVE — emits RefPrice events.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types::{RecordedEvent, RefPrice, RefSource};

const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";
const PING_INTERVAL_S: u64 = 5;

/// Events emitted by the RTDS connection.
#[derive(Debug, Clone)]
pub enum RtdsEvent {
    Price(RefPrice),
    Raw(RecordedEvent),
}

#[derive(Debug, Serialize)]
struct RtdsSubscription {
    action: String,
    subscriptions: Vec<RtdsSubTopic>,
}

#[derive(Debug, Serialize)]
struct RtdsSubTopic {
    topic: String,
    r#type: String,
    filters: String,
}

/// Run the RTDS connection loop with auto-reconnect.
pub async fn run(
    symbol: String, // e.g. "btc/usd"
    tx: mpsc::UnboundedSender<RtdsEvent>,
) {
    loop {
        info!("rtds: connecting to {}", RTDS_WS_URL);
        match connect_and_stream(&symbol, &tx).await {
            Ok(()) => warn!("rtds: connection closed, reconnecting..."),
            Err(e) => error!("rtds: error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_and_stream(
    symbol: &str,
    tx: &mpsc::UnboundedSender<RtdsEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(RTDS_WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("rtds: connected");

    // Subscribe to Binance prices
    let binance_symbol = symbol.replace("/", "").to_lowercase() + "t"; // btc/usd -> btcusdt
    let sub = RtdsSubscription {
        action: "subscribe".to_string(),
        subscriptions: vec![
            RtdsSubTopic {
                topic: "crypto_prices".to_string(),
                r#type: "*".to_string(),
                filters: format!("{{\"symbol\":\"{}\"}}", binance_symbol),
            },
            RtdsSubTopic {
                topic: "crypto_prices_chainlink".to_string(),
                r#type: "*".to_string(),
                filters: format!("{{\"symbol\":\"{}\"}}", symbol),
            },
        ],
    };
    let sub_json = serde_json::to_string(&sub)?;
    write.send(Message::Text(sub_json.into())).await?;
    info!("rtds: subscribed to {} + chainlink {}", binance_symbol, symbol);

    // Spawn ping task
    let ping_write = write;
    let (ping_tx, mut ping_rx) = mpsc::channel::<Message>(16);

    let ping_handle = tokio::spawn(async move {
        let mut write = ping_write;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_S));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if write.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
                msg = ping_rx.recv() => {
                    match msg {
                        Some(m) => { let _ = write.send(m).await; }
                        None => break,
                    }
                }
            }
        }
    });

    while let Some(msg) = read.next().await {
        let local_recv_us = chrono::Utc::now().timestamp_micros();

        match msg? {
            Message::Text(text) => {
                let text_str: &str = text.as_ref();

                // Record raw
                let raw = RecordedEvent {
                    local_recv_us,
                    source: "rtds".to_string(),
                    event_type: "raw".to_string(),
                    raw_json: text_str.to_string(),
                };
                let _ = tx.send(RtdsEvent::Raw(raw));

                // Parse
                if let Ok(price) = parse_rtds_price(text_str, local_recv_us) {
                    let _ = tx.send(RtdsEvent::Price(price));
                }
            }
            Message::Ping(data) => {
                let _ = ping_tx.send(Message::Pong(data)).await;
            }
            Message::Close(_) => {
                info!("rtds: received close frame");
                break;
            }
            _ => {}
        }
    }

    ping_handle.abort();
    Ok(())
}

fn parse_rtds_price(text: &str, local_recv_us: i64) -> Result<RefPrice, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;

    let topic = v.get("topic").and_then(|t| t.as_str()).unwrap_or("");
    let payload = v.get("payload").ok_or("no payload")?;

    let price = payload.get("value")
        .or_else(|| payload.get("price"))
        .and_then(|p| p.as_f64())
        .ok_or("no price value")?;

    let timestamp = payload.get("timestamp")
        .and_then(|t| t.as_i64())
        .or_else(|| v.get("timestamp").and_then(|t| t.as_i64()))
        .unwrap_or(0);

    let symbol = payload.get("symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    let source = match topic {
        "crypto_prices" => RefSource::RtdsBinance,
        "crypto_prices_chainlink" => RefSource::RtdsChainlink,
        _ => return Err(format!("unknown topic: {}", topic)),
    };

    Ok(RefPrice { source, symbol, price, timestamp, local_recv_ts: local_recv_us })
}
