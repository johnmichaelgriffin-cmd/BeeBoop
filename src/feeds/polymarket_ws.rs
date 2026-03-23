//! Polymarket token price feed — 100ms REST polling.
//!
//! Polls GET /price for best ask/bid every 100ms for UP and DOWN tokens.
//! 10 reads/sec per token — near real-time pricing.

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{warn};

use crate::types_v2::{LogEvent, MarketDescriptor, PolymarketTop};

const CLOB_BASE: &str = "https://clob.polymarket.com";
const POLL_INTERVAL_MS: u64 = 100;

pub async fn run_polymarket_ws_task(
    market_rx: watch::Receiver<MarketDescriptor>,
    tx: broadcast::Sender<PolymarketTop>,
    latest_tx: watch::Sender<PolymarketTop>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_millis(800))
        .build()
        .expect("http client");

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    // Don't pile up if a poll takes longer than 100ms
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let market = market_rx.borrow().clone();
        if market.up_token_id.is_empty() || market.down_token_id.is_empty() {
            continue;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();

        // Fetch all 4 prices concurrently — ask+bid for both tokens
        let (up_ask, up_bid, dn_ask, dn_bid) = tokio::join!(
            fetch_price(&client, &market.up_token_id, "buy"),
            fetch_price(&client, &market.up_token_id, "sell"),
            fetch_price(&client, &market.down_token_id, "buy"),
            fetch_price(&client, &market.down_token_id, "sell"),
        );

        let top = PolymarketTop {
            recv_ts_ms: now_ms,
            up_ask,
            up_bid,
            down_ask: dn_ask,
            down_bid: dn_bid,
            up_tick_size: None,
            down_tick_size: None,
        };

        let _ = latest_tx.send(top.clone());
        let _ = tx.send(top);
    }
}

async fn fetch_price(client: &reqwest::Client, token_id: &str, side: &str) -> Option<f64> {
    let url = format!("{}/price?token_id={}&side={}", CLOB_BASE, token_id, side);
    let resp = client.get(&url)
        .header("Cache-Control", "no-cache, no-store")
        .header("Pragma", "no-cache")
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;

    body.get("price")
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok())
}
