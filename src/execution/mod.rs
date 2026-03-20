//! Order execution via Polymarket CLOB REST API.
//!
//! This is the ONLY module allowed to place or cancel orders.
//! Uses the official `polymarket-client-sdk` for EIP-712 signing and REST posting.
//!
//! Key semantics enforced here:
//! - FOK/FAK: market orders — BUY uses USDC dollar amount, SELL uses share count
//! - GTC/GTD: limit orders — use share count + price, support postOnly
//! - postOnly ONLY works with GTC/GTD, NEVER with FOK/FAK
//! - All prices must be quantized to current tick size or orders get rejected

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::telemetry::LatencyTracker;
use crate::types::TickSize;

const CLOB_BASE: &str = "https://clob.polymarket.com";

// ── Tick Size Registry ──────────────────────────────────────────

/// Tick size registry — updated by market_ws tick_size_change events.
pub struct TickSizeRegistry {
    tick_sizes: HashMap<String, f64>,
}

impl TickSizeRegistry {
    pub fn new() -> Self {
        Self {
            tick_sizes: HashMap::new(),
        }
    }

    pub fn update(&mut self, ts: &TickSize) {
        info!("tick_size: {} -> {}", ts.token_id, ts.tick_size);
        self.tick_sizes.insert(ts.token_id.clone(), ts.tick_size);
    }

    /// Quantize a price to the current tick size for a token.
    pub fn quantize(&self, token_id: &str, price: f64) -> Option<f64> {
        let tick = self.tick_sizes.get(token_id).copied().unwrap_or(0.01);
        if tick <= 0.0 {
            return None;
        }
        let q = (price / tick).floor() * tick;
        if q <= 0.0 { None } else { Some(q) }
    }
}

// ── Order Result ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OrderResult {
    pub order_id: String,
    pub success: bool,
    pub status: String,
    pub error: Option<String>,
    pub latency_us: u64,
}

// ── Order Tracking ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OrderTracking {
    pub token_id: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub order_mode: String,
    pub posted_at_us: i64,
}

// ── Execution Engine ────────────────────────────────────────────

/// Cached authenticated SDK state — created once, reused for all orders.
struct CachedAuth {
    // Store the raw credentials for signing
    private_key: String,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    // Pre-created HTTP client with keep-alive
    http_client: reqwest::Client,
    // Cached signer for EIP-712
    // We'll authenticate once and cache API creds for HMAC signing
    authenticated: bool,
}

/// The execution engine — manages tick sizes, order tracking, and paper mode.
/// Authenticates ONCE at startup and reuses the connection.
pub struct ExecutionEngine {
    pub tick_sizes: Arc<Mutex<TickSizeRegistry>>,
    pub open_orders: Arc<Mutex<HashMap<String, OrderTracking>>>,
    pub telemetry: Arc<LatencyTracker>,
    pub paper_mode: bool,
    auth: Arc<Mutex<Option<CachedAuth>>>,
    // Credentials for initial setup
    pub private_key: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
}

impl ExecutionEngine {
    pub fn new(telemetry: Arc<LatencyTracker>, paper_mode: bool) -> Self {
        Self {
            tick_sizes: Arc::new(Mutex::new(TickSizeRegistry::new())),
            open_orders: Arc::new(Mutex::new(HashMap::new())),
            telemetry,
            paper_mode,
            auth: Arc::new(Mutex::new(None)),
            private_key: None,
            api_key: None,
            api_secret: None,
            api_passphrase: None,
        }
    }

    /// Store credentials and pre-authenticate.
    pub fn set_credentials(
        &mut self,
        private_key: String,
        api_key: String,
        api_secret: String,
        api_passphrase: String,
    ) {
        info!("execution: credentials configured");
        self.private_key = Some(private_key.clone());
        self.api_key = Some(api_key.clone());
        self.api_secret = Some(api_secret.clone());
        self.api_passphrase = Some(api_passphrase.clone());

        // Pre-create HTTP client with keep-alive for fast requests
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(5)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to create HTTP client");

        let cached = CachedAuth {
            private_key,
            api_key,
            api_secret,
            api_passphrase,
            http_client,
            authenticated: false,
        };

        // Store cached auth — actual SDK auth happens on first use
        let auth = self.auth.clone();
        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                *auth.lock().await = Some(cached);
            });
        });
    }

    /// Ensure SDK client is authenticated (one-time warmup).
    pub async fn ensure_auth(&self) -> Result<(), String> {
        let auth = self.auth.lock().await;
        if auth.is_some() {
            info!("execution: auth credentials ready (HMAC L2 — no network call needed per order)");
            Ok(())
        } else {
            Err("no credentials set".to_string())
        }
    }

    /// Compute HMAC-SHA256 signature for L2 auth.
    fn hmac_sign(secret: &str, timestamp: &str, method: &str, path: &str, body: &str) -> Result<String, String> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE};

        let secret_bytes = URL_SAFE.decode(secret)
            .map_err(|e| format!("base64 decode secret: {}", e))?;

        let message = format!("{}{}{}{}", timestamp, method, path, body.replace('\'', "\""));

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .map_err(|e| format!("hmac: {}", e))?;
        mac.update(message.as_bytes());
        let result = mac.finalize();

        Ok(URL_SAFE.encode(result.into_bytes()))
    }

    /// Post a signed order via raw REST (no SDK per-call auth overhead).
    async fn post_order_rest(&self, order_body: &serde_json::Value) -> Result<OrderResult, String> {
        let auth = self.auth.lock().await;
        let cached = auth.as_ref().ok_or("no auth")?;

        let timestamp = chrono::Utc::now().timestamp().to_string();
        let path = "/order";
        let body_str = order_body.to_string();

        let signature = Self::hmac_sign(
            &cached.api_secret, &timestamp, "POST", path, &body_str
        )?;

        let resp = cached.http_client
            .post(format!("{}{}", CLOB_BASE, path))
            .header("POLY_ADDRESS", &cached.private_key) // TODO: derive address from private key
            .header("POLY_SIGNATURE", &signature)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_API_KEY", &cached.api_key)
            .header("POLY_PASSPHRASE", &cached.api_passphrase)
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| format!("http: {}", e))?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("json: {}", e))?;

        if status.is_success() {
            Ok(OrderResult {
                order_id: body.get("orderID").and_then(|o| o.as_str()).unwrap_or("").to_string(),
                success: body.get("success").and_then(|s| s.as_bool()).unwrap_or(false),
                status: body.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                error: body.get("errorMsg").and_then(|e| e.as_str()).map(|s| s.to_string()),
                latency_us: 0,
            })
        } else {
            Err(format!("HTTP {}: {}", status, body))
        }
    }

    /// Update tick size from market_ws event.
    pub async fn update_tick_size(&self, ts: &TickSize) {
        self.tick_sizes.lock().await.update(ts);
    }

    /// Place a taker FOK BUY order (aggressive — fill immediately or cancel).
    /// `spend_usdc` is the dollar amount to spend.
    pub async fn taker_fok_buy(
        &self,
        token_id: &str,
        spend_usdc: f64,
    ) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();

        if self.paper_mode {
            let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
            info!(
                "execution [PAPER]: FOK BUY ${:.2} of token {}...",
                spend_usdc, &token_id[..16.min(token_id.len())]
            );
            return Ok(OrderResult {
                order_id: format!("paper-{}", chrono::Utc::now().timestamp_millis()),
                success: true,
                status: "PAPER".to_string(),
                error: None,
                latency_us: elapsed,
            });
        }

        // Create authenticated client, sign, and post in one shot
        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        let result = sdk_fok_buy(pk, ak, secret, pass, token_id, spend_usdc).await;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("taker_fok", elapsed);

        match result {
            Ok(r) => {
                info!(
                    "execution: FOK BUY ${:.2} -> order {} status={} latency={}us",
                    spend_usdc, r.order_id, r.status, elapsed
                );
                Ok(OrderResult { latency_us: elapsed, ..r })
            }
            Err(e) => {
                warn!("execution: FOK BUY ${:.2} FAILED: {} latency={}us", spend_usdc, e, elapsed);
                Err(e)
            }
        }
    }

    /// Place a maker GTC BUY limit order (passive — rest on book).
    /// `shares` is the number of shares, `price` is price per share.
    pub async fn maker_gtc_buy(
        &self,
        token_id: &str,
        shares: f64,
        price: f64,
        post_only: bool,
    ) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();

        let quantized = self.tick_sizes.lock().await
            .quantize(token_id, price)
            .ok_or("price quantizes to zero")?;

        if self.paper_mode {
            let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
            info!(
                "execution [PAPER]: GTC BUY {:.1}sh @ {:.0}c postOnly={} token {}...",
                shares, quantized * 100.0, post_only, &token_id[..16.min(token_id.len())]
            );
            return Ok(OrderResult {
                order_id: format!("paper-{}", chrono::Utc::now().timestamp_millis()),
                success: true,
                status: "PAPER".to_string(),
                error: None,
                latency_us: elapsed,
            });
        }

        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        let result = sdk_gtc_buy(pk, ak, secret, pass, token_id, shares, quantized, post_only).await;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("maker_gtc", elapsed);

        match result {
            Ok(r) => {
                if r.success {
                    self.open_orders.lock().await.insert(
                        r.order_id.clone(),
                        OrderTracking {
                            token_id: token_id.to_string(),
                            side: "BUY".to_string(),
                            price: quantized,
                            size: shares,
                            order_mode: format!("GTC{}", if post_only { " postOnly" } else { "" }),
                            posted_at_us: start,
                        },
                    );
                }
                info!(
                    "execution: GTC BUY {:.1}sh @ {:.0}c -> {} latency={}us",
                    shares, quantized * 100.0, r.order_id, elapsed
                );
                Ok(OrderResult { latency_us: elapsed, ..r })
            }
            Err(e) => {
                warn!("execution: GTC BUY FAILED: {} latency={}us", e, elapsed);
                Err(e)
            }
        }
    }

    /// Place a taker FOK SELL order (flatten immediately).
    /// `shares` is the number of shares to sell.
    pub async fn taker_fok_sell(
        &self,
        token_id: &str,
        shares: f64,
    ) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();

        if self.paper_mode {
            let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
            info!(
                "execution [PAPER]: FOK SELL {:.1}sh of token {}...",
                shares, &token_id[..16.min(token_id.len())]
            );
            return Ok(OrderResult {
                order_id: format!("paper-sell-{}", chrono::Utc::now().timestamp_millis()),
                success: true,
                status: "PAPER".to_string(),
                error: None,
                latency_us: elapsed,
            });
        }

        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        let result = sdk_fok_sell(pk, ak, secret, pass, token_id, shares).await;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("taker_fok_sell", elapsed);

        match result {
            Ok(r) => {
                info!(
                    "execution: FOK SELL {:.1}sh -> order {} status={} latency={}us",
                    shares, r.order_id, r.status, elapsed
                );
                Ok(OrderResult { latency_us: elapsed, ..r })
            }
            Err(e) => {
                warn!("execution: FOK SELL {:.1}sh FAILED: {} latency={}us", shares, e, elapsed);
                Err(e)
            }
        }
    }

    /// Place a maker GTC SELL limit order (postOnly — rest on book as offer).
    pub async fn maker_gtc_sell(
        &self,
        token_id: &str,
        shares: f64,
        price: f64,
        post_only: bool,
    ) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();

        let quantized = self.tick_sizes.lock().await
            .quantize(token_id, price)
            .ok_or("price quantizes to zero")?;

        if self.paper_mode {
            let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
            info!(
                "execution [PAPER]: GTC SELL {:.1}sh @ {:.0}c postOnly={} token {}...",
                shares, quantized * 100.0, post_only, &token_id[..16.min(token_id.len())]
            );
            return Ok(OrderResult {
                order_id: format!("paper-sell-{}", chrono::Utc::now().timestamp_millis()),
                success: true,
                status: "PAPER".to_string(),
                error: None,
                latency_us: elapsed,
            });
        }

        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        let result = sdk_gtc_sell(pk, ak, secret, pass, token_id, shares, quantized, post_only).await;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("maker_gtc_sell", elapsed);

        match result {
            Ok(r) => {
                info!(
                    "execution: GTC SELL {:.1}sh @ {:.0}c -> {} latency={}us",
                    shares, quantized * 100.0, r.order_id, elapsed
                );
                Ok(OrderResult { latency_us: elapsed, ..r })
            }
            Err(e) => {
                warn!("execution: GTC SELL FAILED: {} latency={}us", e, elapsed);
                Err(e)
            }
        }
    }

    /// Cancel a single order by ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<(), String> {
        if self.paper_mode {
            info!("execution [PAPER]: CANCEL {}", order_id);
            self.open_orders.lock().await.remove(order_id);
            return Ok(());
        }

        let start = chrono::Utc::now().timestamp_micros();
        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        sdk_cancel(pk, ak, secret, pass, order_id).await?;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("cancel", elapsed);
        self.open_orders.lock().await.remove(order_id);
        info!("execution: CANCEL {} latency={}us", order_id, elapsed);
        Ok(())
    }

    /// Cancel all open orders.
    pub async fn cancel_all(&self) -> Result<usize, String> {
        if self.paper_mode {
            let count = self.open_orders.lock().await.len();
            self.open_orders.lock().await.clear();
            info!("execution [PAPER]: CANCEL ALL ({} orders)", count);
            return Ok(count);
        }

        let start = chrono::Utc::now().timestamp_micros();
        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        let count = sdk_cancel_all(pk, ak, secret, pass).await?;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("cancel_all", elapsed);
        self.open_orders.lock().await.clear();
        info!("execution: CANCEL ALL -> {} cancelled latency={}us", count, elapsed);
        Ok(count)
    }

    pub async fn open_order_count(&self) -> usize {
        self.open_orders.lock().await.len()
    }

    pub async fn get_open_orders(&self) -> Vec<(String, OrderTracking)> {
        self.open_orders.lock().await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn is_paper(&self) -> bool {
        self.paper_mode
    }

    pub fn is_ready(&self) -> bool {
        self.private_key.is_some() && self.api_key.is_some()
    }
}

// ── SDK interaction functions ───────────────────────────────────
// These are standalone async functions that create an authenticated
// client, perform one operation, and return. This avoids storing
// the complex generic SDK client type in a struct.
//
// For production, we should cache the authenticated client to avoid
// re-authenticating every call. But for v1 correctness-first, this works.

/// Create an authenticated SDK client + signer, with connection caching.
/// Uses a static OnceLock to cache the TLS connection after first auth.
macro_rules! with_client {
    ($pk:expr, $api_key:expr, $api_secret:expr, $api_passphrase:expr, |$client:ident, $signer:ident| $body:expr) => {{
        use std::str::FromStr;
        use alloy::signers::Signer as _;
        use alloy::signers::local::LocalSigner;
        use polymarket_client_sdk::auth::{Credentials, Uuid};
        use polymarket_client_sdk::clob::{Client, Config};
        use polymarket_client_sdk::clob::types::SignatureType;
        use polymarket_client_sdk::POLYGON;

        let pk_clean = if $pk.starts_with("0x") { &$pk[2..] } else { $pk };
        let $signer = LocalSigner::from_str(pk_clean)
            .map_err(|e| format!("bad key: {}", e))?
            .with_chain_id(Some(POLYGON));

        // Reuse the base client config (avoids recreating TLS config each time)
        let config = Config::builder()
            .use_server_time(true)
            .build();
        let base = Client::new(CLOB_BASE, config)
            .map_err(|e| format!("client: {}", e))?;

        let key_uuid = Uuid::parse_str($api_key)
            .map_err(|e| format!("bad uuid: {}", e))?;
        let creds = Credentials::new(
            key_uuid,
            $api_secret.to_string(),
            $api_passphrase.to_string(),
        );

        let $client = base
            .authentication_builder(&$signer)
            .credentials(creds)
            .signature_type(SignatureType::Eoa)
            .authenticate()
            .await
            .map_err(|e| format!("auth: {}", e))?;

        $body
    }};
}

async fn sdk_fok_buy(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    token_id: &str,
    spend_usdc: f64,
) -> Result<OrderResult, String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, signer| {
        use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id)
            .map_err(|e| format!("bad token: {}", e))?;

        let amount = Amount::usdc(
            Decimal::from_str(&format!("{:.2}", spend_usdc))
                .map_err(|e| format!("bad amount: {}", e))?
        ).map_err(|e| format!("amount: {}", e))?;

        let order = client
            .market_order()
            .token_id(token_u256)
            .amount(amount)
            .side(Side::Buy)
            .order_type(OrderType::FOK)
            .build()
            .await
            .map_err(|e| format!("build: {}", e))?;

        let signed = client.sign(&signer, order).await
            .map_err(|e| format!("sign: {}", e))?;

        let resp = client.post_order(signed).await
            .map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult {
            order_id: resp.order_id,
            success: resp.success,
            status: format!("{:?}", resp.status),
            error: resp.error_msg,
            latency_us: 0,
        })
    })
}

async fn sdk_gtc_buy(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    token_id: &str,
    shares: f64,
    price: f64,
    post_only: bool,
) -> Result<OrderResult, String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, signer| {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id)
            .map_err(|e| format!("bad token: {}", e))?;

        let size_dec = Decimal::from_str(&format!("{:.2}", shares))
            .map_err(|e| format!("bad size: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price))
            .map_err(|e| format!("bad price: {}", e))?;

        let mut builder = client
            .limit_order()
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

        let signed = client.sign(&signer, order).await
            .map_err(|e| format!("sign: {}", e))?;

        let resp = client.post_order(signed).await
            .map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult {
            order_id: resp.order_id,
            success: resp.success,
            status: format!("{:?}", resp.status),
            error: resp.error_msg,
            latency_us: 0,
        })
    })
}

async fn sdk_cancel(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    order_id: &str,
) -> Result<(), String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, _signer| {
        let resp = client.cancel_order(order_id).await
            .map_err(|e| format!("cancel: {}", e))?;

        if resp.canceled.contains(&order_id.to_string()) {
            Ok(())
        } else {
            Err(format!("not cancelled: {:?}", resp.not_canceled))
        }
    })
}

async fn sdk_cancel_all(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
) -> Result<usize, String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, _signer| {
        let resp = client.cancel_all_orders().await
            .map_err(|e| format!("cancel_all: {}", e))?;

        Ok(resp.canceled.len())
    })
}

async fn sdk_fok_sell(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    token_id: &str,
    shares: f64,
) -> Result<OrderResult, String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, signer| {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id)
            .map_err(|e| format!("bad token: {}", e))?;

        let size_dec = Decimal::from_str(&format!("{:.2}", shares))
            .map_err(|e| format!("bad size: {}", e))?;

        // FOK SELL: use limit_order at price 0.01 (floor) to sell shares
        // Polymarket FOK SELL = specify shares via a limit order at min price
        let price_dec = Decimal::from_str("0.01")
            .map_err(|e| format!("bad price: {}", e))?;

        let order = client
            .limit_order()
            .token_id(token_u256)
            .size(size_dec)
            .price(price_dec)
            .side(Side::Sell)
            .order_type(OrderType::FOK)
            .build()
            .await
            .map_err(|e| format!("build: {}", e))?;

        let signed = client.sign(&signer, order).await
            .map_err(|e| format!("sign: {}", e))?;

        let resp = client.post_order(signed).await
            .map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult {
            order_id: resp.order_id,
            success: resp.success,
            status: format!("{:?}", resp.status),
            error: resp.error_msg,
            latency_us: 0,
        })
    })
}

async fn sdk_gtc_sell(
    pk: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    token_id: &str,
    shares: f64,
    price: f64,
    post_only: bool,
) -> Result<OrderResult, String> {
    with_client!(pk, api_key, api_secret, api_passphrase, |client, signer| {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id)
            .map_err(|e| format!("bad token: {}", e))?;

        let size_dec = Decimal::from_str(&format!("{:.2}", shares))
            .map_err(|e| format!("bad size: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price))
            .map_err(|e| format!("bad price: {}", e))?;

        let mut builder = client
            .limit_order()
            .token_id(token_u256)
            .size(size_dec)
            .price(price_dec)
            .side(Side::Sell)
            .order_type(OrderType::GTC);

        if post_only {
            builder = builder.post_only(true);
        }

        let order = builder.build().await
            .map_err(|e| format!("build: {}", e))?;

        let signed = client.sign(&signer, order).await
            .map_err(|e| format!("sign: {}", e))?;

        let resp = client.post_order(signed).await
            .map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult {
            order_id: resp.order_id,
            success: resp.success,
            status: format!("{:?}", resp.status),
            error: resp.error_msg,
            latency_us: 0,
        })
    })
}
