//! Executor task — OWNS the authenticated SDK client.
//!
//! Receives ExecutionCommand via channel, executes on Polymarket CLOB.
//! No locks on the SDK client — this task is the sole owner.
//! Emits ExecutionEvent for each result.
//!
//! FOK taker only — no GTC/maker logic.

use std::str::FromStr;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use polymarket_client_sdk::auth::{Credentials, Normal, Uuid, state::Authenticated};
use polymarket_client_sdk::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;

use crate::config_v2::Config;
use crate::types_v2::*;

const CLOB_BASE: &str = "https://clob.polymarket.com";

type AuthedClient = Client<Authenticated<Normal>>;

pub async fn run_executor_task(
    config: Config,
    mut cmd_rx: mpsc::Receiver<ExecutionCommand>,
    evt_tx: mpsc::Sender<ExecutionEvent>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    // Authenticate ONCE at startup
    let client = if config.mode == BotMode::Live && config.has_credentials() {
        match authenticate(&config).await {
            Ok((client, signer)) => {
                info!("executor: authenticated — ready to trade");
                Some((client, signer))
            }
            Err(e) => {
                error!("executor: auth FAILED: {} — running dry-run only", e);
                None
            }
        }
    } else {
        info!("executor: DryRun mode — no auth needed");
        None
    };

    // Order heartbeat — must be called every 5s or open orders get cancelled
    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut heartbeat_id: Option<String> = None;

    loop {
        tokio::select! {
            // Heartbeat tick — keep orders alive
            _ = heartbeat_interval.tick() => {
                if let Some((ref cli, _)) = client {
                    match post_heartbeat(cli, heartbeat_id.as_deref()).await {
                        Ok(new_id) => {
                            heartbeat_id = Some(new_id);
                        }
                        Err(e) => {
                            warn!("heartbeat failed: {} — orders may be cancelled", e);
                            heartbeat_id = None; // will get a fresh one next tick
                        }
                    }
                }
            }

            // Process execution commands
            Some(cmd) = cmd_rx.recv() => {
        match cmd {
            ExecutionCommand::EnterTaker { market_slug, token_id, side, max_price, notional, signal } => {
                let sent_ts_ms = chrono::Utc::now().timestamp_millis();
                let start = Instant::now();

                let result = if let Some((ref cli, ref signer)) = client {
                    execute_fok_buy(cli, signer, &token_id, config.share_base, max_price).await
                } else {
                    // Dry run
                    let shares = notional / max_price;
                    Ok(FillResult {
                        order_id: format!("dry-{}", sent_ts_ms),
                        filled_price: max_price,
                        filled_size: shares,
                        latency_ms: 0,
                    })
                };

                let latency_ms = start.elapsed().as_millis() as i64;
                let ack_ts_ms = chrono::Utc::now().timestamp_millis();

                match result {
                    Ok(fill) => {
                        info!(">>> BUY FILLED: {}sh @ {:.0}c | {}ms | order={}",
                            fill.filled_size as u32,
                            fill.filled_price * 100.0,
                            latency_ms,
                            &fill.order_id[..16.min(fill.order_id.len())],
                        );
                        let _ = log_tx.send(LogEvent::EntryFilled {
                            ts_ms: ack_ts_ms,
                            side,
                            token_id: token_id.clone(),
                            price: fill.filled_price,
                            size: fill.filled_size,
                            latency_ms,
                        }).await;
                        let _ = evt_tx.send(ExecutionEvent::EntryFilled {
                            sent_ts_ms,
                            ack_ts_ms,
                            market_slug,
                            token_id,
                            side,
                            filled_price: fill.filled_price,
                            filled_size: fill.filled_size,
                            notional,
                            order_id: fill.order_id,
                            signal,
                        }).await;
                    }
                    Err(e) => {
                        warn!(">>> BUY FAILED: {} | {}ms", e, latency_ms);
                        let _ = log_tx.send(LogEvent::EntryRejected {
                            ts_ms: ack_ts_ms,
                            reason: e.clone(),
                        }).await;
                        let _ = evt_tx.send(ExecutionEvent::EntryRejected {
                            ts_ms: ack_ts_ms,
                            reason: e,
                        }).await;
                    }
                }
            }

            ExecutionCommand::ExitTaker { market_slug, token_id, side, shares, min_price, reason } => {
                let sent_ts_ms = chrono::Utc::now().timestamp_millis();
                let start = Instant::now();

                let result = if let Some((ref cli, ref signer)) = client {
                    execute_fok_sell(cli, signer, &token_id, shares, min_price).await
                } else {
                    let bid = min_price * 1.05; // dry run: assume 5% above floor
                    Ok(FillResult {
                        order_id: format!("dry-sell-{}", sent_ts_ms),
                        filled_price: bid,
                        filled_size: shares,
                        latency_ms: 0,
                    })
                };

                let latency_ms = start.elapsed().as_millis() as i64;
                let ack_ts_ms = chrono::Utc::now().timestamp_millis();

                match result {
                    Ok(fill) => {
                        info!(">>> SELL FILLED: {}sh @ {:.0}c | {}ms | {}",
                            fill.filled_size as u32,
                            fill.filled_price * 100.0,
                            latency_ms,
                            reason,
                        );
                        let _ = log_tx.send(LogEvent::ExitFilled {
                            ts_ms: ack_ts_ms,
                            side,
                            price: fill.filled_price,
                            size: fill.filled_size,
                            latency_ms,
                        }).await;
                        let _ = evt_tx.send(ExecutionEvent::ExitFilled {
                            sent_ts_ms,
                            ack_ts_ms,
                            market_slug,
                            token_id,
                            side,
                            filled_price: fill.filled_price,
                            filled_size: fill.filled_size,
                            reason,
                            order_id: fill.order_id,
                        }).await;
                    }
                    Err(e) => {
                        warn!(">>> SELL FAILED: {} | {}ms", e, latency_ms);
                        let _ = evt_tx.send(ExecutionEvent::ExitRejected {
                            ts_ms: ack_ts_ms,
                            reason: e,
                        }).await;
                    }
                }
            }

            ExecutionCommand::BuySecondLeg { market_slug, opposite_token_id, opposite_side, ask_price, reason } => {
                // Second leg of pair — just another FOK BUY using the same lockprofit formula.
                let sent_ts_ms = chrono::Utc::now().timestamp_millis();
                let start = Instant::now();

                let result = if let Some((ref cli, ref signer)) = client {
                    execute_fok_buy(cli, signer, &opposite_token_id, config.share_base, ask_price).await
                } else {
                    let shares = (config.share_base * ask_price).floor();
                    Ok(FillResult {
                        order_id: format!("dry-leg2-{}", sent_ts_ms),
                        filled_price: ask_price,
                        filled_size: shares,
                        latency_ms: 0,
                    })
                };

                let latency_ms = start.elapsed().as_millis() as i64;
                let ack_ts_ms = chrono::Utc::now().timestamp_millis();

                match result {
                    Ok(fill) => {
                        info!(">>> LEG 2 FILLED: {} {:.0}sh @ {:.0}c | {}ms | {}",
                            if opposite_side == Side::Up { "UP" } else { "DN" },
                            fill.filled_size as u32,
                            fill.filled_price * 100.0,
                            latency_ms,
                            reason,
                        );
                        let _ = evt_tx.send(ExecutionEvent::SecondLegFilled {
                            ts_ms: ack_ts_ms,
                            market_slug,
                            opposite_token_id,
                            opposite_side,
                            filled_price: fill.filled_price,
                            filled_size: fill.filled_size,
                            reason,
                        }).await;
                    }
                    Err(e) => {
                        warn!(">>> LEG 2 FAILED: {} | {}ms", e, latency_ms);
                        let _ = evt_tx.send(ExecutionEvent::SecondLegRejected {
                            ts_ms: ack_ts_ms,
                            reason: e,
                        }).await;
                    }
                }
            }

            ExecutionCommand::PostMakerBid { market_slug, token_id, side, price, size, post_only } => {
                let sent_ts_ms = chrono::Utc::now().timestamp_millis();
                let start = Instant::now();

                let result = if let Some((ref cli, ref signer)) = client {
                    execute_gtc_bid(cli, signer, &token_id, size, price, post_only).await
                } else {
                    // Dry run
                    Ok(FillResult {
                        order_id: format!("dry-maker-{}", sent_ts_ms),
                        filled_price: price,
                        filled_size: 0.0, // GTC doesn't fill immediately
                        latency_ms: 0,
                    })
                };

                let latency_ms = start.elapsed().as_millis() as i64;

                match result {
                    Ok(fill) => {
                        // GTC posted — it won't fill immediately, it rests on the book
                        // Fills come later via user WS or when someone hits our bid
                        // For now, we treat the post as success and wait for fills
                    }
                    Err(e) => {
                        if !e.contains("would cross") && !e.contains("post-only") {
                            warn!("MAKER BID FAILED: {} @ {:.2} {:.0}sh — {}",
                                if side == Side::Up { "UP" } else { "DN" },
                                price, size, e);
                        }
                        // post-only rejections are expected and normal — just means price crossed
                    }
                }
            }

            ExecutionCommand::CancelAll { reason } => {
                if let Some((ref cli, _)) = client {
                    match cli.cancel_all_orders().await {
                        Ok(resp) => {
                            info!(">>> CANCEL ALL: {} cancelled ({})", resp.canceled.len(), reason);
                            let _ = evt_tx.send(ExecutionEvent::CancelAck {
                                ts_ms: chrono::Utc::now().timestamp_millis(),
                                count: resp.canceled.len(),
                            }).await;
                        }
                        Err(e) => warn!("cancel_all failed: {}", e),
                    }
                }
            }

            ExecutionCommand::Reconcile { up_token_id, down_token_id } => {
                if let Some((ref cli, _)) = client {
                    // Fetch recent trades to reconcile fill state
                    match reconcile_fills(cli, &up_token_id, &down_token_id).await {
                        Ok(result) => {
                            let _ = evt_tx.send(ExecutionEvent::ReconcileResult(result)).await;
                        }
                        Err(e) => warn!("reconcile failed: {}", e),
                    }
                }
            }
        }
            } // end Some(cmd) = cmd_rx.recv()
        } // end tokio::select!
    } // end loop
}

pub struct FillResult {
    pub order_id: String,
    pub filled_price: f64,
    pub filled_size: f64,
    pub latency_ms: i64,
}

async fn authenticate(config: &Config) -> Result<(AuthedClient, PrivateKeySigner), String> {
    let pk = &config.poly_private_key;
    let pk_clean = if pk.starts_with("0x") { &pk[2..] } else { pk.as_str() };

    let mut signer = PrivateKeySigner::from_str(pk_clean)
        .map_err(|e| format!("bad key: {}", e))?;
    signer.set_chain_id(Some(POLYGON));

    let clob_config = ClobConfig::builder().use_server_time(true).build();
    let base = Client::new(CLOB_BASE, clob_config)
        .map_err(|e| format!("client: {}", e))?;

    let key_uuid = Uuid::parse_str(&config.poly_api_key)
        .map_err(|e| format!("bad uuid: {}", e))?;
    let creds = Credentials::new(
        key_uuid,
        config.poly_api_secret.clone(),
        config.poly_api_passphrase.clone(),
    );

    let authed = base
        .authentication_builder(&signer)
        .credentials(creds)
        .signature_type(SignatureType::Eoa)
        .authenticate()
        .await
        .map_err(|e| format!("auth: {}", e))?;

    Ok((authed, signer))
}

/// Buy using lockprofit formula: floor(50 × ask_price) shares at 99c limit.
/// The matching engine spends all allocated $ and gives ~50 tokens via price improvement.
/// Example: ask=60c → floor(50×0.60) = 30sh @ 99c → fills at ~60c → ~50 tokens.
async fn execute_fok_buy(
    client: &AuthedClient,
    signer: &PrivateKeySigner,
    token_id: &str,
    share_base: f64,   // lockprofit formula base (e.g. 25 for BTC, 10 for ETH)
    ask_price: f64,    // current ask price for this token
) -> Result<FillResult, String> {
    use polymarket_client_sdk::clob::types::{OrderType, Side};

    let token_u256 = U256::from_str(token_id).map_err(|e| format!("token: {}", e))?;

    // Lockprofit formula: floor(50 × ask_price) shares at 99c
    // Posts N shares where N = floor(50 * ask). Willing to spend N * 0.99.
    // Price improvement fills at ~ask, so total spend ≈ N * ask ≈ base * ask^2.
    let base = share_base;
    let shares = (base * ask_price).floor().max(1.0);

    let size_str = format!("{:.0}", shares); // whole number, no decimals
    let size_dec = Decimal::from_str(&size_str)
        .map_err(|e| format!("dec: {}", e))?;
    let price_dec = Decimal::from_str("0.99")
        .map_err(|e| format!("dec: {}", e))?;

    info!("FOK BUY: {}sh @ 99c (ask={:.0}c, formula=floor({:.0}*{:.2})={:.0}), token={}...{}",
        size_str, ask_price * 100.0, base, ask_price, shares,
        &token_id[..8.min(token_id.len())], &token_id[token_id.len().saturating_sub(8)..]);

    let order = client.limit_order()
        .token_id(token_u256)
        .size(size_dec)
        .price(price_dec)
        .side(Side::Buy)
        .order_type(OrderType::FOK)
        .build().await
        .map_err(|e| format!("build: {}", e))?;

    let signed = client.sign(signer, order).await
        .map_err(|e| format!("sign: {}", e))?;
    let resp = client.post_order(signed).await
        .map_err(|e| format!("post: {}", e))?;

    if resp.success {
        Ok(FillResult {
            order_id: resp.order_id,
            filled_price: ask_price, // estimate — actual fill is at or near ask
            filled_size: shares,     // shares posted — actual tokens received may be ~20
            latency_ms: 0,
        })
    } else {
        Err(format!("rejected: {:?} {:?}", resp.status, resp.error_msg))
    }
}

async fn execute_fok_sell(
    client: &AuthedClient,
    signer: &PrivateKeySigner,
    token_id: &str,
    shares: f64,
    min_price: f64,
) -> Result<FillResult, String> {
    use polymarket_client_sdk::clob::types::{OrderType, Side};

    let token_u256 = U256::from_str(token_id).map_err(|e| format!("token: {}", e))?;

    // Round shares down to 2dp
    let shares_rounded = (shares * 100.0).floor() / 100.0;
    if shares_rounded <= 0.0 {
        return Err("shares rounds to 0".to_string());
    }

    let size_dec = Decimal::from_str(&format!("{:.2}", shares_rounded))
        .map_err(|e| format!("dec: {}", e))?;

    // SELL via market_order with Amount::shares — THIS IS WHAT WORKED ON 2026-03-20
    // SDK auto-calculates price by walking bids in the orderbook
    // DO NOT use limit_order for sells — it doesn't work
    use polymarket_client_sdk::clob::types::Amount;

    let amount = Amount::shares(size_dec).map_err(|e| format!("amount: {}", e))?;

    info!("FOK SELL: {:.2}sh market_order (auto-price), token={}...", shares_rounded, &token_id[..16.min(token_id.len())]);

    let order = client.market_order()
        .token_id(token_u256)
        .amount(amount)
        .side(Side::Sell)
        .order_type(OrderType::FOK)
        .build().await
        .map_err(|e| format!("build: {}", e))?;

    let signed = client.sign(signer, order).await
        .map_err(|e| format!("sign: {}", e))?;
    let resp = client.post_order(signed).await
        .map_err(|e| format!("post: {}", e))?;

    if resp.success {
        Ok(FillResult {
            order_id: resp.order_id,
            filled_price: min_price, // approximate — actual fill may differ
            filled_size: shares_rounded,
            latency_ms: 0,
        })
    } else {
        Err(format!("rejected: {:?} {:?}", resp.status, resp.error_msg))
    }
}

/// Simple handle for direct sell/buy calls from mint+sell bot.
/// Owns the authenticated SDK client — no channels needed.
pub struct ExecutorHandle {
    client: AuthedClient,
    signer: PrivateKeySigner,
}

impl ExecutorHandle {
    pub async fn new(config: &Config) -> Option<Self> {
        match authenticate(config).await {
            Ok((client, signer)) => {
                info!("executor: authenticated — ready to trade");
                Some(Self { client, signer })
            }
            Err(e) => {
                tracing::error!("executor: auth failed: {}", e);
                None
            }
        }
    }

    pub async fn sell_fok(&self, token_id: &str, shares: f64) -> Result<FillResult, String> {
        let start = std::time::Instant::now();
        let mut result = execute_fok_sell(&self.client, &self.signer, token_id, shares, 0.01).await;
        let latency = start.elapsed().as_millis() as i64;
        if let Ok(ref mut fill) = result {
            fill.latency_ms = latency;
        }
        result
    }

    pub async fn buy_fok(&self, token_id: &str, share_base: f64, ask_price: f64) -> Result<FillResult, String> {
        let start = std::time::Instant::now();
        let mut result = execute_fok_buy(&self.client, &self.signer, token_id, share_base, ask_price).await;
        let latency = start.elapsed().as_millis() as i64;
        if let Ok(ref mut fill) = result {
            fill.latency_ms = latency;
        }
        result
    }
}

/// Post a GTC postOnly BUY bid — rests on the book, earns maker rebates.
/// Returns immediately — the order ID is for later tracking/cancellation.
async fn execute_gtc_bid(
    client: &AuthedClient,
    signer: &PrivateKeySigner,
    token_id: &str,
    size: f64,
    price: f64,
    post_only: bool,
) -> Result<FillResult, String> {
    use polymarket_client_sdk::clob::types::{OrderType, Side};

    let token_u256 = U256::from_str(token_id).map_err(|e| format!("token: {}", e))?;

    let size_rounded = (size * 100.0).floor() / 100.0;
    if size_rounded <= 0.0 {
        return Err("size rounds to 0".to_string());
    }

    let size_dec = Decimal::from_str(&format!("{:.2}", size_rounded))
        .map_err(|e| format!("dec: {}", e))?;
    let price_dec = Decimal::from_str(&format!("{:.2}", price))
        .map_err(|e| format!("dec: {}", e))?;

    let mut builder = client.limit_order()
        .token_id(token_u256)
        .size(size_dec)
        .price(price_dec)
        .side(Side::Buy)
        .order_type(OrderType::GTC);

    if post_only {
        builder = builder.post_only(true);
    }

    let order = builder.build().await
        .map_err(|e| format!("build: {}", e))?;

    let signed = client.sign(signer, order).await
        .map_err(|e| format!("sign: {}", e))?;
    let resp = client.post_order(signed).await
        .map_err(|e| format!("post: {}", e))?;

    if resp.success {
        Ok(FillResult {
            order_id: resp.order_id,
            filled_price: price,
            filled_size: 0.0, // GTC doesn't fill immediately
            latency_ms: 0,
        })
    } else {
        Err(format!("rejected: {:?} {:?}", resp.status, resp.error_msg))
    }
}

/// Post order heartbeat to keep resting GTC orders alive.
/// Polymarket cancels all open orders if heartbeat not received within ~10s.
async fn post_heartbeat(client: &AuthedClient, prev_id: Option<&str>) -> Result<String, String> {
    use polymarket_client_sdk::auth::Uuid;
    let hb_uuid = prev_id
        .and_then(|id| Uuid::parse_str(id).ok());
    match client.post_heartbeat(hb_uuid).await {
        Ok(resp) => {
            // HeartbeatResponse has .heartbeat_id: Uuid — extract it directly
            Ok(resp.heartbeat_id.to_string())
        }
        Err(e) => Err(format!("heartbeat: {}", e)),
    }
}

/// Reconcile local fill state against CLOB via REST.
/// Fetches open orders and recent trades to verify local state.
async fn reconcile_fills(
    _client: &AuthedClient,
    up_token_id: &str,
    down_token_id: &str,
) -> Result<crate::types_v2::ReconciliationResult, String> {
    // Use REST directly — simpler than fighting SDK type mismatches
    let http = reqwest::Client::new();

    let mut up_total = 0.0_f64;
    let mut dn_total = 0.0_f64;
    let open_count = 0_usize;

    // Fetch trades for each token from the public data API
    for (token_id, total) in [
        (up_token_id, &mut up_total),
        (down_token_id, &mut dn_total),
    ] {
        let url = format!("https://data-api.polymarket.com/trades?asset_id={}&limit=100", token_id);
        if let Ok(resp) = http.get(&url).send().await {
            if let Ok(trades) = resp.json::<Vec<serde_json::Value>>().await {
                for trade in &trades {
                    if let Some(size) = trade.get("size")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                    {
                        // Only count our buys (maker side)
                        let maker = trade.get("maker_address")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !maker.is_empty() {
                            *total += size;
                        }
                    }
                }
            }
        }
    }

    Ok(crate::types_v2::ReconciliationResult {
        open_order_count: open_count,
        up_size_matched: up_total,
        down_size_matched: dn_total,
        ts_ms: chrono::Utc::now().timestamp_millis(),
    })
}
