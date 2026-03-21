//! Binance bookTicker WebSocket feed — lowest latency BTC BBO.
//!
//! Connects to wss://fstream.binance.com/ws/btcusdt@bookTicker
//! Emits BinanceTick on every tick — event-driven, no polling.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types_v2::{BinanceTick, LogEvent};

const BINANCE_WS: &str = "wss://fstream.binance.com/ws/btcusdt@bookTicker";

pub async fn run_binance_feed_task(
    tx: broadcast::Sender<BinanceTick>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    loop {
        info!("binance: connecting to {}", BINANCE_WS);
        match connect_and_stream(&tx, &log_tx).await {
            Ok(()) => warn!("binance: disconnected, reconnecting..."),
            Err(e) => error!("binance: error: {}, reconnecting in 1s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn connect_and_stream(
    tx: &broadcast::Sender<BinanceTick>,
    _log_tx: &mpsc::Sender<LogEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws, _) = connect_async(BINANCE_WS).await?;
    let (mut write, mut read) = ws.split();
    info!("binance: connected");

    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(text) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                if let Some(tick) = parse_book_ticker(&text, now_ms) {
                    let _ = tx.send(tick);
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

fn parse_book_ticker(text: &str, recv_ts_ms: i64) -> Option<BinanceTick> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let bid = v.get("b")?.as_str()?.parse::<f64>().ok()?;
    let ask = v.get("a")?.as_str()?.parse::<f64>().ok()?;
    let event_ts = v.get("E")?.as_i64()?;
    let mid = (bid + ask) / 2.0;

    Some(BinanceTick {
        recv_ts_ms,
        event_ts_ms: event_ts,
        bid,
        ask,
        mid,
    })
}
