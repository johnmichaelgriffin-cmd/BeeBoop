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

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            ExecutionCommand::EnterTaker { market_slug, token_id, side, max_price, notional, signal } => {
                let sent_ts_ms = chrono::Utc::now().timestamp_millis();
                let start = Instant::now();

                let result = if let Some((ref cli, ref signer)) = client {
                    execute_fok_buy(cli, signer, &token_id, notional, max_price).await
                } else {
                    // Dry run
                    let shares = notional / max_price;
                    Ok(FillResult {
                        order_id: format!("dry-{}", sent_ts_ms),
                        filled_price: max_price,
                        filled_size: shares,
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
        }
    }
}

struct FillResult {
    order_id: String,
    filled_price: f64,
    filled_size: f64,
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

async fn execute_fok_buy(
    client: &AuthedClient,
    signer: &PrivateKeySigner,
    token_id: &str,
    spend_usdc: f64,
    _max_price: f64,
) -> Result<FillResult, String> {
    use polymarket_client_sdk::clob::types::{OrderType, Side};

    let token_u256 = U256::from_str(token_id).map_err(|e| format!("token: {}", e))?;

    // Buy 20 shares at ask + 3c limit (not 99c — that sweeps into expensive levels)
    // Price improvement fills us at the actual ask, limit just caps the max we'll pay
    let shares = 20.0_f64;
    let limit_price = (_max_price + 0.03).min(0.97);
    let limit_price_rounded = (limit_price * 100.0).round() / 100.0;

    let size_dec = Decimal::from_str(&format!("{:.2}", shares))
        .map_err(|e| format!("dec: {}", e))?;
    let price_dec = Decimal::from_str(&format!("{:.2}", limit_price_rounded))
        .map_err(|e| format!("dec: {}", e))?;

    info!("FOK BUY: {:.0}sh limit@{:.0}c, token={}...{}",
        shares, limit_price_rounded * 100.0,
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
        // Estimate: 20 shares at approximately the ask price
        let estimated_price = limit_price_rounded;
        let estimated_shares = shares;
        Ok(FillResult {
            order_id: resp.order_id,
            filled_price: estimated_price,
            filled_size: estimated_shares,
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
        })
    } else {
        Err(format!("rejected: {:?} {:?}", resp.status, resp.error_msg))
    }
}
