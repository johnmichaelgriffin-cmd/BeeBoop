//! Polymarket token price feed — fast REST polling.
//!
//! Polls GET /price for best ask/bid every 200ms for UP and DOWN tokens.
//! Uses /price endpoint (lighter than /book), no-cache headers, 1s timeout.

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{warn};

use crate::types_v2::{LogEvent, MarketDescriptor, PolymarketTop};

const CLOB_BASE: &str = "https://clob.polymarket.com";
const POLL_INTERVAL_MS: u64 = 200;

pub async fn run_polymarket_ws_task(
    market_rx: watch::Receiver<MarketDescriptor>,
    tx: broadcast::Sender<PolymarketTop>,
    latest_tx: watch::Sender<PolymarketTop>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(2)
        .timeout(std::time::Duration::from_millis(1000))
        .build()
        .expect("http client");

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(POLL_INTERVAL_MS));

    loop {
        interval.tick().await;

        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() || market.down_token_id.is_empty() {
            continue;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Fetch both prices concurrently — /price is much lighter than /book
        let (up_ask, up_bid, dn_ask, dn_bid) = tokio::join!(
            fetch_price(&client, &market.up_token_id, "buy"),  // buy side = ask
            fetch_price(&client, &market.up_token_id, "sell"), // sell side = bid
            fetch_price(&client, &market.down_token_id, "buy"),
            fetch_price(&client, &market.down_token_id, "sell"),
        );

        let top = PolymarketTop {
            recv_ts_ms: now_ms,
            up_ask: up_ask,
            up_bid: up_bid,
            down_ask: dn_ask,
            down_bid: dn_bid,
        };

        let _ = latest_tx.send(top.clone());
        let _ = tx.send(top);
    }
}

/// Fetch best price from GET /price?token_id={id}&side={side}
/// side="buy" returns the best ask (what you'd pay to buy)
/// side="sell" returns the best bid (what you'd get selling)
async fn fetch_price(client: &reqwest::Client, token_id: &str, side: &str) -> Option<f64> {
    let url = format!("{}/price?token_id={}&side={}", CLOB_BASE, token_id, side);
    let resp = client.get(&url)
        .header("Cache-Control", "no-cache, no-store")
        .header("Pragma", "no-cache")
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;

    // /price returns {"price": "0.55"}
    body.get("price")
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok())
}
