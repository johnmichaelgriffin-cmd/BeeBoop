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
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use alloy::signers::Signer as _;
use alloy::signers::local::LocalSigner;
use polymarket_client_sdk::auth::{Credentials, Normal, Uuid, state::Authenticated};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::POLYGON;

use crate::telemetry::LatencyTracker;
use crate::types::TickSize;

/// Type alias for the authenticated Polymarket CLOB client.
type AuthedClient = Client<Authenticated<Normal>>;

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

/// The execution engine — manages tick sizes, order tracking, and paper mode.
/// Authenticates ONCE at startup and reuses the SDK client for all orders.
pub struct ExecutionEngine {
    pub tick_sizes: Arc<Mutex<TickSizeRegistry>>,
    pub open_orders: Arc<Mutex<HashMap<String, OrderTracking>>>,
    pub telemetry: Arc<LatencyTracker>,
    pub paper_mode: bool,
    /// Authenticated SDK client — created once via ensure_auth(), reused forever.
    sdk_client: Arc<Mutex<Option<AuthedClient>>>,
    /// Cached signer for order signing (local crypto, no network call).
    signer: Arc<Mutex<Option<alloy::signers::local::PrivateKeySigner>>>,
    // Credentials
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
            sdk_client: Arc::new(Mutex::new(None)),
            signer: Arc::new(Mutex::new(None)),
            private_key: None,
            api_key: None,
            api_secret: None,
            api_passphrase: None,
        }
    }

    /// Store credentials for later use.
    pub fn set_credentials(
        &mut self,
        private_key: String,
        api_key: String,
        api_secret: String,
        api_passphrase: String,
    ) {
        info!("execution: credentials stored");
        self.private_key = Some(private_key);
        self.api_key = Some(api_key);
        self.api_secret = Some(api_secret);
        self.api_passphrase = Some(api_passphrase);
    }

    /// Authenticate ONCE with the Polymarket CLOB and cache the client.
    /// All subsequent order calls reuse this client — no re-auth overhead.
    pub async fn ensure_auth(&self) -> Result<(), String> {
        {
            let existing = self.sdk_client.lock().await;
            if existing.is_some() {
                info!("execution: SDK client already authenticated");
                return Ok(());
            }
        }

        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        info!("execution: authenticating with Polymarket CLOB (one-time)...");
        let start = chrono::Utc::now().timestamp_micros();

        let pk_clean = if pk.starts_with("0x") { &pk[2..] } else { pk.as_str() };
        let local_signer = alloy::signers::local::PrivateKeySigner::from_str(pk_clean)
            .map_err(|e| format!("bad key: {}", e))?;

        let config = Config::builder().use_server_time(true).build();
        let base = Client::new(CLOB_BASE, config)
            .map_err(|e| format!("client: {}", e))?;

        let key_uuid = Uuid::parse_str(ak)
            .map_err(|e| format!("bad uuid: {}", e))?;
        let creds = Credentials::new(
            key_uuid,
            secret.to_string(),
            pass.to_string(),
        );

        let authed = base
            .authentication_builder(&local_signer)
            .credentials(creds)
            .signature_type(SignatureType::Eoa)
            .authenticate()
            .await
            .map_err(|e| format!("auth: {}", e))?;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        info!("execution: authenticated in {}ms — cached for all future orders", elapsed / 1000);

        *self.sdk_client.lock().await = Some(authed);
        *self.signer.lock().await = Some(local_signer);

        Ok(())
    }

    /// Update tick size from market_ws event.
    pub async fn update_tick_size(&self, ts: &TickSize) {
        self.tick_sizes.lock().await.update(ts);
    }

    /// Place a taker FOK BUY order (aggressive — fill immediately or cancel).
    /// Uses cached authenticated client — no per-call auth overhead.
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

        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("SDK client not initialized — call ensure_auth first")?;
        let signer_guard = self.signer.lock().await;
        let signer = signer_guard.as_ref().ok_or("signer not initialized")?;

        let result = Self::do_fok_buy(client, signer, token_id, spend_usdc).await;

        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("taker_fok", elapsed);

        match result {
            Ok(r) => {
                info!(
                    "execution: FOK BUY ${:.2} -> order {} status={} latency={}ms",
                    spend_usdc, r.order_id, r.status, elapsed / 1000
                );
                Ok(OrderResult { latency_us: elapsed, ..r })
            }
            Err(e) => {
                warn!("execution: FOK BUY ${:.2} FAILED: {} latency={}ms", spend_usdc, e, elapsed / 1000);
                Err(e)
            }
        }
    }

    /// Place a maker GTC BUY limit order (passive — rest on book).
    pub async fn maker_gtc_buy(&self, token_id: &str, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();
        let quantized = self.tick_sizes.lock().await.quantize(token_id, price).ok_or("price quantizes to zero")?;
        if self.paper_mode {
            info!("execution [PAPER]: GTC BUY {:.1}sh @ {:.0}c postOnly={}", shares, quantized * 100.0, post_only);
            return Ok(OrderResult { order_id: format!("paper-{}", start), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.lock().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let result = Self::do_gtc_buy(client, signer, token_id, shares, quantized, post_only).await;
        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("maker_gtc", elapsed);
        match result {
            Ok(r) => { info!("execution: GTC BUY {:.1}sh @ {:.0}c -> {} latency={}ms", shares, quantized*100.0, r.order_id, elapsed/1000); Ok(OrderResult { latency_us: elapsed, ..r }) }
            Err(e) => { warn!("execution: GTC BUY FAILED: {} latency={}ms", e, elapsed/1000); Err(e) }
        }
    }

    /// Place a taker FOK SELL order (flatten immediately).
    pub async fn taker_fok_sell(&self, token_id: &str, shares: f64) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();
        if self.paper_mode {
            info!("execution [PAPER]: FOK SELL {:.1}sh", shares);
            return Ok(OrderResult { order_id: format!("paper-sell-{}", start), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.lock().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let result = Self::do_fok_sell(client, signer, token_id, shares).await;
        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("taker_fok_sell", elapsed);
        match result {
            Ok(r) => { info!("execution: FOK SELL {:.1}sh -> {} latency={}ms", shares, r.order_id, elapsed/1000); Ok(OrderResult { latency_us: elapsed, ..r }) }
            Err(e) => { warn!("execution: FOK SELL FAILED: {} latency={}ms", e, elapsed/1000); Err(e) }
        }
    }

    /// Place a maker GTC SELL limit order.
    pub async fn maker_gtc_sell(&self, token_id: &str, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        let start = chrono::Utc::now().timestamp_micros();
        let quantized = self.tick_sizes.lock().await.quantize(token_id, price).ok_or("price quantizes to zero")?;
        if self.paper_mode {
            info!("execution [PAPER]: GTC SELL {:.1}sh @ {:.0}c", shares, quantized*100.0);
            return Ok(OrderResult { order_id: format!("paper-sell-{}", start), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.lock().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let result = Self::do_gtc_sell(client, signer, token_id, shares, quantized, post_only).await;
        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("maker_gtc_sell", elapsed);
        match result {
            Ok(r) => { info!("execution: GTC SELL {:.1}sh @ {:.0}c -> {} latency={}ms", shares, quantized*100.0, r.order_id, elapsed/1000); Ok(OrderResult { latency_us: elapsed, ..r }) }
            Err(e) => { warn!("execution: GTC SELL FAILED: {} latency={}ms", e, elapsed/1000); Err(e) }
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
        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let resp = client.cancel_order(order_id).await.map_err(|e| format!("cancel: {}", e))?;
        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("cancel", elapsed);
        self.open_orders.lock().await.remove(order_id);
        info!("execution: CANCEL {} latency={}ms", order_id, elapsed/1000);
        if resp.canceled.contains(&order_id.to_string()) { Ok(()) }
        else { Err(format!("not cancelled: {:?}", resp.not_canceled)) }
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
        let client_guard = self.sdk_client.lock().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let resp = client.cancel_all_orders().await.map_err(|e| format!("cancel_all: {}", e))?;
        let elapsed = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("cancel_all", elapsed);
        self.open_orders.lock().await.clear();
        info!("execution: CANCEL ALL -> {} cancelled latency={}ms", resp.canceled.len(), elapsed/1000);
        Ok(resp.canceled.len())
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

// ── Cached client order methods ─────────────────────────────────
// These methods use the pre-authenticated client stored in sdk_client.
// No per-call authentication overhead — signing is local crypto only.

impl ExecutionEngine {
    async fn do_fok_buy(
        client: &AuthedClient,
        signer: &alloy::signers::local::PrivateKeySigner,
        token_id: &str,
        spend_usdc: f64,
    ) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id).map_err(|e| format!("bad token: {}", e))?;
        let amount = Amount::usdc(
            Decimal::from_str(&format!("{:.2}", spend_usdc)).map_err(|e| format!("bad amount: {}", e))?
        ).map_err(|e| format!("amount: {}", e))?;

        let order = client.market_order().token_id(token_u256).amount(amount)
            .side(Side::Buy).order_type(OrderType::FOK)
            .build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_gtc_buy(
        client: &AuthedClient,
        signer: &alloy::signers::local::PrivateKeySigner,
        token_id: &str,
        shares: f64,
        price: f64,
        post_only: bool,
    ) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id).map_err(|e| format!("bad token: {}", e))?;
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("bad size: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price)).map_err(|e| format!("bad price: {}", e))?;

        let mut builder = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec).side(Side::Buy).order_type(OrderType::GTC);
        if post_only { builder = builder.post_only(true); }
        let order = builder.build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_fok_sell(
        client: &AuthedClient,
        signer: &alloy::signers::local::PrivateKeySigner,
        token_id: &str,
        shares: f64,
    ) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id).map_err(|e| format!("bad token: {}", e))?;
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("bad size: {}", e))?;
        let price_dec = Decimal::from_str("0.01").map_err(|e| format!("bad price: {}", e))?;

        let order = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec)
            .side(Side::Sell).order_type(OrderType::FOK)
            .build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_gtc_sell(
        client: &AuthedClient,
        signer: &alloy::signers::local::PrivateKeySigner,
        token_id: &str,
        shares: f64,
        price: f64,
        post_only: bool,
    ) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        use polymarket_client_sdk::types::{Decimal, U256};

        let token_u256 = U256::from_str(token_id).map_err(|e| format!("bad token: {}", e))?;
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("bad size: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price)).map_err(|e| format!("bad price: {}", e))?;

        let mut builder = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec).side(Side::Sell).order_type(OrderType::GTC);
        if post_only { builder = builder.post_only(true); }
        let order = builder.build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;

        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }
}
