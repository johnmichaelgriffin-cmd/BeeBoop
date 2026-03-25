//! RTDS WebSocket feed — Chainlink + Binance oracle prices.
//!
//! Connects to wss://ws-live-data.polymarket.com
//! Subscribes to crypto_prices_chainlink for BTC/USD oracle price.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types_v2::{ChainlinkTick, LogEvent};

const RTDS_WS: &str = "wss://ws-live-data.polymarket.com";

pub async fn run_rtds_task(
    tx: broadcast::Sender<ChainlinkTick>,
    latest_tx: watch::Sender<Option<ChainlinkTick>>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    run_rtds_task_for("btc/usd", tx, latest_tx, _log_tx).await;
}

pub async fn run_rtds_task_for(
    symbol: &str,
    tx: broadcast::Sender<ChainlinkTick>,
    latest_tx: watch::Sender<Option<ChainlinkTick>>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    let sym = symbol.to_string();
    loop {
        info!("rtds: connecting to {} for {}", RTDS_WS, sym);
        match connect_and_stream_sym(&sym, &tx, &latest_tx).await {
            Ok(()) => warn!("rtds: disconnected, reconnecting..."),
            Err(e) => error!("rtds: error: {}, reconnecting in 2s...", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_and_stream(
    tx: &broadcast::Sender<ChainlinkTick>,
    latest_tx: &watch::Sender<Option<ChainlinkTick>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    connect_and_stream_sym("btc/usd", tx, latest_tx).await
}

async fn connect_and_stream_sym(
    symbol: &str,
    tx: &broadcast::Sender<ChainlinkTick>,
    latest_tx: &watch::Sender<Option<ChainlinkTick>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws, _) = connect_async(RTDS_WS).await?;
    let (mut write, mut read) = ws.split();
    info!("rtds: connected");

    let filter = format!("{{\"symbol\":\"{}\"}}", symbol);
    let sub = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [
            {
                "topic": "crypto_prices_chainlink",
                "type": "*",
                "filters": filter
            }
        ]
    });
    write.send(Message::Text(sub.to_string().into())).await?;
    info!("rtds: subscribed to chainlink {}", symbol);

    // Ping every 5s
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let rtds_connect_time = chrono::Utc::now().timestamp();

    loop {
        // Forced reconnect every 10 minutes
        if chrono::Utc::now().timestamp() - rtds_connect_time >= 600 {
            info!("rtds: FORCED RECONNECT after 600s");
            break;
        }

        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        if let Some(tick) = parse_chainlink(&text, now_ms) {
                            let _ = latest_tx.send(Some(tick.clone()));
                            let _ = tx.send(tick);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        error!("rtds: ws error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![].into())).await?;
            }
        }
    }
    Ok(())
}

fn parse_chainlink(text: &str, recv_ts_ms: i64) -> Option<ChainlinkTick> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let topic = v.get("topic")?.as_str()?;
    if !topic.contains("chainlink") { return None; }

    let payload = v.get("payload")?;
    let price = payload.get("value")
        .or_else(|| payload.get("price"))?
        .as_f64()?;

    let source_ts = v.get("timestamp").and_then(|t| t.as_i64());

    Some(ChainlinkTick {
        recv_ts_ms,
        source_ts_ms: source_ts,
        price,
    })
}
