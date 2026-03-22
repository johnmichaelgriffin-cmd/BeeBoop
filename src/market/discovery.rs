//! Market discovery — finds current 5-minute BTC Up/Down markets.
//!
//! Queries Gamma API for active markets, extracts UP/DOWN token IDs,
//! and updates the watch channel when a new window starts.

use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::config_v2::Config;
use crate::types_v2::{LogEvent, MarketDescriptor};

const GAMMA_API: &str = "https://gamma-api.polymarket.com";
const WINDOW_SECONDS: i64 = 300;

pub async fn run_market_discovery_task(
    config: Config,
    market_tx: watch::Sender<MarketDescriptor>,
    _log_tx: mpsc::Sender<LogEvent>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("http client");

    let slug_prefix = match config.market.as_str() {
        "btc" => "btc-updown-5m",
        "eth" => "eth-updown-5m",
        "sol" => "sol-updown-5m",
        _ => "btc-updown-5m",
    };

    let mut last_window_ts: i64 = 0;

    loop {
        let now = chrono::Utc::now().timestamp();
        let current_window = now - (now % WINDOW_SECONDS);
        let next_window = current_window + WINDOW_SECONDS;

        // Only fetch if window changed
        if current_window != last_window_ts {
            // Fetch current and next window
            for wts in [current_window, next_window] {
                let slug = format!("{}-{}", slug_prefix, wts);
                match fetch_market(&client, &slug).await {
                    Some(md) => {
                        info!("market: {} UP={}...{} DN={}...{}",
                            md.slug,
                            &md.up_token_id[..8.min(md.up_token_id.len())],
                            &md.up_token_id[md.up_token_id.len().saturating_sub(8)..],
                            &md.down_token_id[..8.min(md.down_token_id.len())],
                            &md.down_token_id[md.down_token_id.len().saturating_sub(8)..],
                        );
                        // Only update watch if this is the current window
                        if wts == current_window {
                            let _ = market_tx.send(md);
                        }
                    }
                    None => {
                        warn!("market: failed to fetch {}", slug);
                    }
                }
            }
            last_window_ts = current_window;
        }

        // Sleep until next window boundary + 2s (let market get created)
        let sleep_until = next_window + 2;
        let sleep_secs = (sleep_until - chrono::Utc::now().timestamp()).max(1) as u64;
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs.min(30))).await;
    }
}

async fn fetch_market(client: &reqwest::Client, slug: &str) -> Option<MarketDescriptor> {
    let url = format!("{}/events?slug={}", GAMMA_API, slug);
    let resp = client.get(&url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;

    let events = body.as_array()?;
    let event = events.first()?;
    let markets = event.get("markets")?.as_array()?;
    let market = markets.first()?;

    let outcomes_str = market.get("outcomes")?.as_str()?;
    let token_ids_str = market.get("clobTokenIds")?.as_str()?;
    let condition_id = market.get("conditionId")?.as_str()?.to_string();

    // Parse JSON string arrays
    let outcomes: Vec<String> = serde_json::from_str(outcomes_str).ok()?;
    let token_ids: Vec<String> = serde_json::from_str(token_ids_str).ok()?;

    if outcomes.len() < 2 || token_ids.len() < 2 {
        return None;
    }

    let (mut up_id, mut down_id) = (String::new(), String::new());
    for (outcome, token_id) in outcomes.iter().zip(token_ids.iter()) {
        let lower = outcome.to_lowercase();
        if lower.contains("up") {
            up_id = token_id.clone();
        } else if lower.contains("down") {
            down_id = token_id.clone();
        }
    }

    if up_id.is_empty() || down_id.is_empty() {
        return None;
    }

    // Parse window timestamp from slug: "btc-updown-5m-1774012200"
    let window_ts: i64 = slug.rsplit('-').next()?.parse().ok()?;

    Some(MarketDescriptor {
        slug: slug.to_string(),
        condition_id,
        up_token_id: up_id,
        down_token_id: down_id,
        window_start_ts: window_ts,
        window_end_ts: window_ts + WINDOW_SECONDS,
    })
}
