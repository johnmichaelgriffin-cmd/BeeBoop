//! Order execution via Polymarket CLOB REST API — SPEED OPTIMIZED.
//!
//! This is the ONLY module allowed to place or cancel orders.
//!
//! Speed optimizations:
//! - SDK client authenticated ONCE at startup, cached forever
//! - RwLock instead of Mutex — reads never block each other
//! - Pre-parsed token U256s cached to avoid per-order parsing
//! - Signer cached — signing is pure local crypto, no network
//! - HTTP/2 keep-alive via SDK's internal reqwest client
//! - Minimal allocations in hot path

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::auth::{Credentials, Normal, Uuid, state::Authenticated};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::POLYGON;

use crate::telemetry::LatencyTracker;
use crate::types::TickSize;

type AuthedClient = Client<Authenticated<Normal>>;
type U256 = polymarket_client_sdk::types::U256;
type Decimal = polymarket_client_sdk::types::Decimal;

const CLOB_BASE: &str = "https://clob.polymarket.com";

// ── Tick Size Registry ──────────────────────────────────────────

pub struct TickSizeRegistry {
    tick_sizes: HashMap<String, f64>,
}

impl TickSizeRegistry {
    pub fn new() -> Self { Self { tick_sizes: HashMap::new() } }

    pub fn update(&mut self, ts: &TickSize) {
        self.tick_sizes.insert(ts.token_id.clone(), ts.tick_size);
    }

    pub fn quantize(&self, token_id: &str, price: f64) -> Option<f64> {
        let tick = self.tick_sizes.get(token_id).copied().unwrap_or(0.01);
        if tick <= 0.0 { return None; }
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

// ── Pre-parsed token cache ──────────────────────────────────────

/// Cache parsed U256 token IDs to avoid re-parsing on every order.
struct TokenCache {
    cache: HashMap<String, U256>,
}

impl TokenCache {
    fn new() -> Self { Self { cache: HashMap::with_capacity(8) } }

    fn get_or_parse(&mut self, token_id: &str) -> Result<U256, String> {
        if let Some(v) = self.cache.get(token_id) {
            return Ok(*v);
        }
        let parsed = U256::from_str(token_id).map_err(|e| format!("bad token: {}", e))?;
        self.cache.insert(token_id.to_string(), parsed);
        Ok(parsed)
    }
}

// ── Execution Engine ────────────────────────────────────────────

/// Speed-optimized execution engine.
/// - RwLock on client/signer: multiple reads never block each other
/// - Token cache: U256 parsed once, reused forever
/// - One-time auth: no per-call overhead
pub struct ExecutionEngine {
    pub tick_sizes: Arc<RwLock<TickSizeRegistry>>,
    pub open_orders: Arc<RwLock<HashMap<String, OrderTracking>>>,
    pub telemetry: Arc<LatencyTracker>,
    pub paper_mode: bool,
    /// Authenticated SDK client — created once, read-locked for all orders.
    sdk_client: Arc<RwLock<Option<AuthedClient>>>,
    /// Cached signer — local crypto only, no network.
    signer: Arc<RwLock<Option<PrivateKeySigner>>>,
    /// Pre-parsed token IDs.
    token_cache: Arc<RwLock<TokenCache>>,
    // Credentials
    pub private_key: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
}

impl ExecutionEngine {
    pub fn new(telemetry: Arc<LatencyTracker>, paper_mode: bool) -> Self {
        Self {
            tick_sizes: Arc::new(RwLock::new(TickSizeRegistry::new())),
            open_orders: Arc::new(RwLock::new(HashMap::new())),
            telemetry,
            paper_mode,
            sdk_client: Arc::new(RwLock::new(None)),
            signer: Arc::new(RwLock::new(None)),
            token_cache: Arc::new(RwLock::new(TokenCache::new())),
            private_key: None,
            api_key: None,
            api_secret: None,
            api_passphrase: None,
        }
    }

    pub fn set_credentials(&mut self, private_key: String, api_key: String, api_secret: String, api_passphrase: String) {
        self.private_key = Some(private_key);
        self.api_key = Some(api_key);
        self.api_secret = Some(api_secret);
        self.api_passphrase = Some(api_passphrase);
    }

    /// Authenticate ONCE. All subsequent calls use cached client via RwLock::read().
    pub async fn ensure_auth(&self) -> Result<(), String> {
        {
            if self.sdk_client.read().await.is_some() {
                return Ok(());
            }
        }

        let pk = self.private_key.as_ref().ok_or("no private key")?;
        let ak = self.api_key.as_ref().ok_or("no api key")?;
        let secret = self.api_secret.as_ref().ok_or("no api secret")?;
        let pass = self.api_passphrase.as_ref().ok_or("no api passphrase")?;

        info!("execution: authenticating (one-time)...");
        let start = std::time::Instant::now();

        let pk_clean = if pk.starts_with("0x") { &pk[2..] } else { pk.as_str() };
        let local_signer = PrivateKeySigner::from_str(pk_clean)
            .map_err(|e| format!("bad key: {}", e))?;

        let config = Config::builder().use_server_time(true).build();
        let base = Client::new(CLOB_BASE, config).map_err(|e| format!("client: {}", e))?;

        let key_uuid = Uuid::parse_str(ak).map_err(|e| format!("bad uuid: {}", e))?;
        let creds = Credentials::new(key_uuid, secret.to_string(), pass.to_string());

        let authed = base
            .authentication_builder(&local_signer)
            .credentials(creds)
            .signature_type(SignatureType::Eoa)
            .authenticate()
            .await
            .map_err(|e| format!("auth: {}", e))?;

        let elapsed = start.elapsed();
        info!("execution: authenticated in {:.0}ms — cached forever", elapsed.as_millis());

        *self.sdk_client.write().await = Some(authed);
        *self.signer.write().await = Some(local_signer);

        Ok(())
    }

    /// Pre-warm token cache for known token IDs (call at startup).
    pub async fn warm_token(&self, token_id: &str) {
        if let Ok(_) = self.token_cache.write().await.get_or_parse(token_id) {
            // cached
        }
    }

    pub async fn update_tick_size(&self, ts: &TickSize) {
        self.tick_sizes.write().await.update(ts);
    }

    // ── HOT PATH: Order methods ─────────────────────────────────
    // These acquire READ locks only (non-blocking when no writes).

    pub async fn taker_fok_buy(&self, token_id: &str, spend_usdc: f64) -> Result<OrderResult, String> {
        let start = std::time::Instant::now();

        if self.paper_mode {
            return Ok(OrderResult { order_id: format!("paper-{}", chrono::Utc::now().timestamp_millis()), success: true, status: "PAPER".into(), error: None, latency_us: start.elapsed().as_micros() as u64 });
        }

        // Read locks — non-blocking with each other
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.read().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;

        // Get cached U256
        let token_u256 = self.token_cache.write().await.get_or_parse(token_id)?;

        let result = Self::do_fok_buy(client, signer, token_u256, spend_usdc).await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("taker_fok", elapsed_us);

        match result {
            Ok(r) => { info!(">>> FOK BUY ${:.2} -> {} | {}ms", spend_usdc, r.order_id, elapsed_us/1000); Ok(OrderResult { latency_us: elapsed_us, ..r }) }
            Err(e) => { warn!(">>> FOK BUY ${:.2} FAILED: {} | {}ms", spend_usdc, e, elapsed_us/1000); Err(e) }
        }
    }

    pub async fn maker_gtc_buy(&self, token_id: &str, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        let start = std::time::Instant::now();
        let quantized = self.tick_sizes.read().await.quantize(token_id, price).ok_or("bad price")?;
        if self.paper_mode {
            return Ok(OrderResult { order_id: format!("paper-{}", chrono::Utc::now().timestamp_millis()), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.read().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let token_u256 = self.token_cache.write().await.get_or_parse(token_id)?;
        let result = Self::do_gtc_buy(client, signer, token_u256, shares, quantized, post_only).await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("maker_gtc", elapsed_us);
        match result {
            Ok(r) => { info!(">>> GTC BUY {:.0}sh@{:.0}c -> {} | {}ms", shares, quantized*100.0, r.order_id, elapsed_us/1000); Ok(OrderResult { latency_us: elapsed_us, ..r }) }
            Err(e) => { warn!(">>> GTC BUY FAILED: {} | {}ms", e, elapsed_us/1000); Err(e) }
        }
    }

    pub async fn taker_fok_sell(&self, token_id: &str, shares: f64) -> Result<OrderResult, String> {
        let start = std::time::Instant::now();
        if self.paper_mode {
            return Ok(OrderResult { order_id: format!("paper-sell-{}", chrono::Utc::now().timestamp_millis()), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.read().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let token_u256 = self.token_cache.write().await.get_or_parse(token_id)?;
        let result = Self::do_fok_sell(client, signer, token_u256, shares).await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("taker_fok_sell", elapsed_us);
        match result {
            Ok(r) => { info!(">>> FOK SELL {:.0}sh -> {} | {}ms", shares, r.order_id, elapsed_us/1000); Ok(OrderResult { latency_us: elapsed_us, ..r }) }
            Err(e) => { warn!(">>> FOK SELL FAILED: {} | {}ms", e, elapsed_us/1000); Err(e) }
        }
    }

    pub async fn maker_gtc_sell(&self, token_id: &str, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        let start = std::time::Instant::now();
        let quantized = self.tick_sizes.read().await.quantize(token_id, price).ok_or("bad price")?;
        if self.paper_mode {
            return Ok(OrderResult { order_id: format!("paper-sell-{}", chrono::Utc::now().timestamp_millis()), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.read().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let token_u256 = self.token_cache.write().await.get_or_parse(token_id)?;
        let result = Self::do_gtc_sell(client, signer, token_u256, shares, quantized, post_only).await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("maker_gtc_sell", elapsed_us);
        match result {
            Ok(r) => { info!(">>> GTC SELL {:.0}sh@{:.0}c -> {} | {}ms", shares, quantized*100.0, r.order_id, elapsed_us/1000); Ok(OrderResult { latency_us: elapsed_us, ..r }) }
            Err(e) => { warn!(">>> GTC SELL FAILED: {} | {}ms", e, elapsed_us/1000); Err(e) }
        }
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<(), String> {
        if self.paper_mode { self.open_orders.write().await.remove(order_id); return Ok(()); }
        let start = std::time::Instant::now();
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let _ = client.cancel_order(order_id).await.map_err(|e| format!("cancel: {}", e))?;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("cancel", elapsed_us);
        self.open_orders.write().await.remove(order_id);
        Ok(())
    }

    pub async fn cancel_all(&self) -> Result<usize, String> {
        if self.paper_mode { let c = self.open_orders.read().await.len(); self.open_orders.write().await.clear(); return Ok(c); }
        let start = std::time::Instant::now();
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let resp = client.cancel_all_orders().await.map_err(|e| format!("cancel_all: {}", e))?;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("cancel_all", elapsed_us);
        self.open_orders.write().await.clear();
        info!("execution: CANCEL ALL -> {} | {}ms", resp.canceled.len(), elapsed_us/1000);
        Ok(resp.canceled.len())
    }

    pub async fn open_order_count(&self) -> usize { self.open_orders.read().await.len() }
    pub async fn get_open_orders(&self) -> Vec<(String, OrderTracking)> {
        self.open_orders.read().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
    pub fn is_paper(&self) -> bool { self.paper_mode }
    pub fn is_ready(&self) -> bool { self.private_key.is_some() && self.api_key.is_some() }
}

// ── Static order methods — pure logic, no lock acquisition ──────

impl ExecutionEngine {
    async fn do_fok_buy(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, spend_usdc: f64) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};
        let amount = Amount::usdc(Decimal::from_str(&format!("{:.2}", spend_usdc)).map_err(|e| format!("dec: {}", e))?).map_err(|e| format!("amt: {}", e))?;
        let order = client.market_order().token_id(token_u256).amount(amount).side(Side::Buy).order_type(OrderType::FOK).build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_gtc_buy(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("dec: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price)).map_err(|e| format!("dec: {}", e))?;
        let mut b = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec).side(Side::Buy).order_type(OrderType::GTC);
        if post_only { b = b.post_only(true); }
        let order = b.build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_fok_sell(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, shares: f64) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("dec: {}", e))?;
        let price_dec = Decimal::from_str("0.01").unwrap();
        let order = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec).side(Side::Sell).order_type(OrderType::FOK).build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }

    async fn do_gtc_sell(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, shares: f64, price: f64, post_only: bool) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("dec: {}", e))?;
        let price_dec = Decimal::from_str(&format!("{:.4}", price)).map_err(|e| format!("dec: {}", e))?;
        let mut b = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec).side(Side::Sell).order_type(OrderType::GTC);
        if post_only { b = b.post_only(true); }
        let order = b.build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        Ok(OrderResult { order_id: resp.order_id, success: resp.success, status: format!("{:?}", resp.status), error: resp.error_msg, latency_us: 0 })
    }
}
