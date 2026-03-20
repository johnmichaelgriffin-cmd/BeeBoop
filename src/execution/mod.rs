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

// Polygon RPC + CTF contract for on-chain balance checks
const POLYGON_RPC: &str = "https://polygon-bor-rpc.publicnode.com";
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";

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
        let mut local_signer = PrivateKeySigner::from_str(pk_clean)
            .map_err(|e| format!("bad key: {}", e))?;
        use alloy::signers::Signer as _;
        local_signer.set_chain_id(Some(POLYGON));

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

    /// Check on-chain token balance for a specific token ID.
    /// Returns the number of shares we hold (as f64, already divided by 1e6).
    /// Uses the CTF contract's balanceOf(address, tokenId) view call.
    pub async fn check_token_balance(&self, wallet_address: &str, token_id: &str) -> Result<f64, String> {
        // balanceOf(address,uint256) selector = 0x00fdd58e
        let addr_clean = if wallet_address.starts_with("0x") {
            &wallet_address[2..]
        } else {
            wallet_address
        };
        let addr_padded = format!("{:0>64}", addr_clean.to_lowercase());

        // Token ID as uint256 hex
        let token_u256 = U256::from_str(token_id)
            .map_err(|e| format!("bad token id: {}", e))?;
        let token_hex = format!("{:064x}", token_u256);

        let call_data = format!("0x00fdd58e{}{}", addr_padded, token_hex);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "to": CTF_CONTRACT,
                "data": call_data
            }, "latest"],
            "id": 1
        });

        // Use a simple one-shot HTTP client for RPC (not on hot path)
        let client = reqwest::Client::new();
        let resp = client.post(POLYGON_RPC)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("rpc http: {}", e))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| format!("rpc json: {}", e))?;

        if let Some(error) = json.get("error") {
            return Err(format!("rpc error: {}", error));
        }

        let result_hex = json.get("result")
            .and_then(|r| r.as_str())
            .ok_or("no result in rpc response")?;

        // Parse hex result → u128 → f64 (CTF uses 1e6 decimals)
        let hex_clean = result_hex.trim_start_matches("0x");
        let raw = u128::from_str_radix(hex_clean, 16)
            .map_err(|e| format!("parse hex: {}", e))?;
        let balance = raw as f64 / 1_000_000.0;

        Ok(balance)
    }

    /// Get the wallet address from the private key.
    pub fn wallet_address(&self) -> Option<String> {
        let pk = self.private_key.as_ref()?;
        let pk_clean = if pk.starts_with("0x") { &pk[2..] } else { pk };
        let signer = PrivateKeySigner::from_str(pk_clean).ok()?;
        Some(format!("{:?}", signer.address()))
    }

    /// Check USDC.e balance on-chain (for wallet monitoring).
    pub async fn check_usdc_balance(&self, wallet_address: &str) -> Result<f64, String> {
        let addr_clean = if wallet_address.starts_with("0x") { &wallet_address[2..] } else { wallet_address };
        let addr_padded = format!("{:0>64}", addr_clean.to_lowercase());
        // balanceOf(address) selector = 0x70a08231
        let call_data = format!("0x70a08231{}", addr_padded);
        let body = serde_json::json!({
            "jsonrpc": "2.0", "method": "eth_call",
            "params": [{"to": "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174", "data": call_data}, "latest"],
            "id": 1
        });
        let client = reqwest::Client::new();
        let resp = client.post(POLYGON_RPC).json(&body).send().await.map_err(|e| format!("rpc: {}", e))?;
        let json: serde_json::Value = resp.json().await.map_err(|e| format!("json: {}", e))?;
        let hex = json.get("result").and_then(|r| r.as_str()).ok_or("no result")?;
        let raw = u128::from_str_radix(hex.trim_start_matches("0x"), 16).map_err(|e| format!("hex: {}", e))?;
        Ok(raw as f64 / 1_000_000.0)
    }

    /// Check CLOB balance-allowance for a conditional token (what the CLOB thinks is available).
    /// This is more reliable than on-chain checks because it accounts for reserved amounts.
    pub async fn check_clob_balance(&self, token_id: &str) -> Result<f64, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/balance-allowance?asset_type=CONDITIONAL&token_id={}", CLOB_BASE, token_id);
        let resp = client.get(&url).send().await.map_err(|e| format!("http: {}", e))?;
        let json: serde_json::Value = resp.json().await.map_err(|e| format!("json: {}", e))?;
        let balance = json.get("balance").and_then(|b| b.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        Ok(balance)
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

    /// FOK SELL with a minimum price floor (won't sell below this).
    /// Pass entry_price to never sell at a loss.
    pub async fn taker_fok_sell(&self, token_id: &str, shares: f64, min_price: f64) -> Result<OrderResult, String> {
        let start = std::time::Instant::now();
        if self.paper_mode {
            return Ok(OrderResult { order_id: format!("paper-sell-{}", chrono::Utc::now().timestamp_millis()), success: true, status: "PAPER".into(), error: None, latency_us: 0 });
        }
        let client_guard = self.sdk_client.read().await;
        let client = client_guard.as_ref().ok_or("not authenticated")?;
        let signer_guard = self.signer.read().await;
        let signer = signer_guard.as_ref().ok_or("no signer")?;
        let token_u256 = self.token_cache.write().await.get_or_parse(token_id)?;
        let result = Self::do_fok_sell_at_price(client, signer, token_u256, shares, min_price).await;
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.telemetry.record("taker_fok_sell", elapsed_us);
        match result {
            Ok(r) => { info!(">>> FOK SELL {:.0}sh floor={:.0}c -> {} | {}ms", shares, min_price*100.0, r.order_id, elapsed_us/1000); Ok(OrderResult { latency_us: elapsed_us, ..r }) }
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
    /// FOK BUY: limit order at 99c with FOK type (same as lockprofit.py).
    /// `spend_usdc` / 0.99 ≈ shares to buy. Fills at market ask via price improvement.
    async fn do_fok_buy(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, spend_usdc: f64) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{OrderType, Side};
        // Limit order at 99c — effectively a market order via price improvement
        // shares = spend / 0.99 (we'll get filled at the actual ask, not 99c)
        let shares = (spend_usdc / 0.99).floor();
        let size_dec = Decimal::from_str(&format!("{:.2}", shares)).map_err(|e| format!("dec: {}", e))?;
        let price_dec = Decimal::from_str("0.99").unwrap();
        let order = client.limit_order().token_id(token_u256).size(size_dec).price(price_dec)
            .side(Side::Buy).order_type(OrderType::FOK)
            .build().await.map_err(|e| format!("build: {}", e))?;
        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        info!("FOK BUY result: id={} success={} status={:?} err={:?}", resp.order_id, resp.success, resp.status, resp.error_msg);
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

    /// FOK SELL at a specific min price — won't fill below this price.
    /// Use entry_price as floor so we never sell at a loss.
    async fn do_fok_sell(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, shares: f64) -> Result<OrderResult, String> {
        // This version is a stub — callers should use do_fok_sell_at_price instead
        Self::do_fok_sell_at_price(client, signer, token_u256, shares, 0.01).await
    }

    /// FOK SELL at a minimum price — won't dump below this.
    pub async fn do_fok_sell_at_price(client: &AuthedClient, signer: &PrivateKeySigner, token_u256: U256, shares: f64, min_price: f64) -> Result<OrderResult, String> {
        use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};

        // Use market_order() for FOK SELL — NOT limit_order()
        // Per Polymarket docs: FOK/FAK are market orders, SELL specifies shares as amount
        let shares_rounded = (shares * 100.0).floor() / 100.0; // round down to avoid over-selling
        let amount = Amount::shares(
            Decimal::from_str(&format!("{:.2}", shares_rounded)).map_err(|e| format!("dec: {}", e))?
        ).map_err(|e| format!("amount: {}", e))?;

        // Price floor as worst acceptable price
        let price_rounded = (min_price * 100.0).round() / 100.0;
        let price_dec = Decimal::from_str(&format!("{:.2}", price_rounded)).map_err(|e| format!("dec: {}", e))?;

        info!("FOK SELL building: {:.2}sh market_order, price_floor={:.2}, token={}", shares_rounded, price_rounded, token_u256);

        let order = client.market_order()
            .token_id(token_u256)
            .amount(amount)
            .price(price_dec)
            .side(Side::Sell)
            .order_type(OrderType::FOK)
            .build().await
            .map_err(|e| format!("build: {}", e))?;

        let signed = client.sign(signer, order).await.map_err(|e| format!("sign: {}", e))?;
        let resp = client.post_order(signed).await.map_err(|e| format!("post: {}", e))?;
        info!("FOK SELL result: id={} success={} status={:?} err={:?}", resp.order_id, resp.success, resp.status, resp.error_msg);
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
