//! Order execution via Polymarket CLOB REST API.
//!
//! This is the ONLY module allowed to place or cancel orders.
//! Uses postOrder / postOrders with postOnly and batch support.
//!
//! Maintains a persistent HTTP/2 connection with keep-alive.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::telemetry::LatencyTracker;
use crate::types::{OrderRequest, OrderSide, OrderState, OrderStatus, OrderType, TickSize};

const CLOB_BASE: &str = "https://clob.polymarket.com";

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
        self.tick_sizes.insert(ts.token_id.clone(), ts.tick_size);
    }

    /// Quantize a price to the current tick size for a token.
    pub fn quantize(&self, token_id: &str, price: f64) -> f64 {
        if let Some(&tick) = self.tick_sizes.get(token_id) {
            if tick > 0.0 {
                return (price / tick).floor() * tick;
            }
        }
        // Default: quantize to 0.01
        (price * 100.0).floor() / 100.0
    }
}

/// The execution engine — holds HTTP client and order state.
pub struct ExecutionEngine {
    http: Client,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    private_key: String,
    tick_sizes: Arc<Mutex<TickSizeRegistry>>,
    open_orders: Arc<Mutex<HashMap<String, OrderState>>>,
    telemetry: Arc<LatencyTracker>,
}

impl ExecutionEngine {
    pub fn new(
        api_key: String,
        api_secret: String,
        api_passphrase: String,
        private_key: String,
        telemetry: Arc<LatencyTracker>,
    ) -> Self {
        // Build persistent HTTP client with keep-alive
        let http = Client::builder()
            .pool_max_idle_per_host(4)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            api_key,
            api_secret,
            api_passphrase,
            private_key,
            tick_sizes: Arc::new(Mutex::new(TickSizeRegistry::new())),
            open_orders: Arc::new(Mutex::new(HashMap::new())),
            telemetry,
        }
    }

    /// Update tick size from market_ws event.
    pub async fn update_tick_size(&self, ts: &TickSize) {
        self.tick_sizes.lock().await.update(ts);
    }

    /// Place a single order via REST.
    pub async fn post_order(&self, req: &OrderRequest) -> Result<String, String> {
        let start = chrono::Utc::now().timestamp_micros();

        // Quantize price to tick size
        let price = self.tick_sizes.lock().await.quantize(&req.token_id, req.price);

        // TODO: EIP-712 sign the order using alloy
        // TODO: POST to /order endpoint
        // For now, return a placeholder

        let elapsed_us = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("order_post", elapsed_us);

        info!(
            "execution: POST order {} {} {}sh @ {}c {} postOnly={}",
            req.token_id,
            match req.side { OrderSide::Buy => "BUY", OrderSide::Sell => "SELL" },
            req.size,
            (price * 100.0) as i64,
            match req.order_type { OrderType::GTC => "GTC", OrderType::FOK => "FOK", OrderType::GTD => "GTD" },
            req.post_only,
        );

        // Placeholder — will be replaced with actual signing + REST call
        Err("execution: not yet implemented — needs EIP-712 signing".to_string())
    }

    /// Batch post up to 15 orders.
    pub async fn post_orders(&self, reqs: &[OrderRequest]) -> Result<Vec<String>, String> {
        if reqs.len() > 15 {
            return Err("max 15 orders per batch".to_string());
        }

        let start = chrono::Utc::now().timestamp_micros();

        // TODO: Sign each order, batch POST to /orders
        let elapsed_us = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("batch_post", elapsed_us);

        Err("execution: batch not yet implemented".to_string())
    }

    /// Cancel a single order.
    pub async fn cancel_order(&self, order_id: &str) -> Result<(), String> {
        let start = chrono::Utc::now().timestamp_micros();

        // TODO: POST cancel to CLOB
        let elapsed_us = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("order_cancel", elapsed_us);

        info!("execution: CANCEL {}", order_id);
        Err("execution: cancel not yet implemented".to_string())
    }

    /// Cancel all open orders.
    pub async fn cancel_all(&self) -> Result<(), String> {
        let start = chrono::Utc::now().timestamp_micros();

        // TODO: POST cancel_all to CLOB
        let elapsed_us = (chrono::Utc::now().timestamp_micros() - start) as u64;
        self.telemetry.record("cancel_all", elapsed_us);

        info!("execution: CANCEL ALL");
        Err("execution: cancel_all not yet implemented".to_string())
    }

    /// Get count of tracked open orders.
    pub async fn open_order_count(&self) -> usize {
        self.open_orders.lock().await.len()
    }
}
