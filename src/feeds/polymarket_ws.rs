//! Polymarket token price feed — REST polling for reliability.
//!
//! Polls GET /book?token_id={id} every 500ms for UP and DOWN tokens.
//! Updates PolymarketTop watch channel on every poll.
//! WS was too flaky — REST is reliable and fast enough.

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{error, info, warn};

use crate::types_v2::{LogEvent, MarketDescriptor, PolymarketTop};

const CLOB_BASE: &str = "https://clob.polymarket.com";
const POLL_INTERVAL_MS: u64 = 500;

pub async fn run_polymarket_ws_task(
    market_rx: watch::Receiver<MarketDescriptor>,
    tx: broadcast::Sender<PolymarketTop>,
    latest_tx: watch::Sender<PolymarketTop>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(5)
        .timeout(std::time::Duration::from_secs(3))
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

        // Fetch both books concurrently
        let (up_result, dn_result) = tokio::join!(
            fetch_best_ask_bid(&client, &market.up_token_id),
            fetch_best_ask_bid(&client, &market.down_token_id),
        );

        let mut top = PolymarketTop {
            recv_ts_ms: now_ms,
            ..Default::default()
        };

        if let Some((ask, bid)) = up_result {
            top.up_ask = Some(ask);
            top.up_bid = Some(bid);
        }
        if let Some((ask, bid)) = dn_result {
            top.down_ask = Some(ask);
            top.down_bid = Some(bid);
        }

        let _ = latest_tx.send(top.clone());
        let _ = tx.send(top);
    }
}

async fn fetch_best_ask_bid(client: &reqwest::Client, token_id: &str) -> Option<(f64, f64)> {
    let url = format!("{}/book?token_id={}", CLOB_BASE, token_id);
    let resp = client.get(&url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;

    let asks = body.get("asks")?.as_array()?;
    let bids = body.get("bids")?.as_array()?;

    let best_ask = asks.iter()
        .filter_map(|a| a.get("price")?.as_str()?.parse::<f64>().ok())
        .reduce(f64::min)?;

    let best_bid = bids.iter()
        .filter_map(|b| b.get("price")?.as_str()?.parse::<f64>().ok())
        .reduce(f64::max)?;

    Some((best_ask, best_bid))
}
