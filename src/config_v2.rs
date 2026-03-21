//! Configuration for BeeBoop v2 sniper bot.

use crate::types_v2::BotMode;

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: BotMode,
    pub market: String,

    // Signal params
    pub lookback_ms: i64,
    pub entry_threshold_bps: f64,
    pub max_signal_age_ms: i64,

    // Exit params
    pub take_profit_pct: f64,      // 0.10 = 10%
    pub max_hold_ms: i64,
    pub cooldown_ms: i64,

    // Sizing
    pub base_notional: f64,        // USDC per entry

    // Risk
    pub max_one_position: bool,
    pub session_stop_loss: f64,    // negative number, e.g. -200.0
    pub max_spread_cents: f64,
    pub max_entry_price: f64,
    pub min_time_to_expiry_s: u64,
    pub wallet_check_interval_s: u64,

    // Window timing
    pub window_seconds: u64,
    pub start_delay_s: u64,        // T+15s
    pub cancel_at_s: u64,          // T+240s

    // API credentials
    pub poly_private_key: String,
    pub poly_api_key: String,
    pub poly_api_secret: String,
    pub poly_api_passphrase: String,

    // Logging
    pub logs_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: BotMode::DryRun,
            market: "btc".to_string(),

            lookback_ms: 800,
            entry_threshold_bps: 3.0,
            max_signal_age_ms: 1200,

            take_profit_pct: 0.10,
            max_hold_ms: 8_000,
            cooldown_ms: 3_000,

            base_notional: 20.0,

            max_one_position: true,
            session_stop_loss: -200.0,
            max_spread_cents: 5.0,
            max_entry_price: 0.95,
            min_time_to_expiry_s: 20,
            wallet_check_interval_s: 300,

            window_seconds: 300,
            start_delay_s: 15,
            cancel_at_s: 240,

            poly_private_key: String::new(),
            poly_api_key: String::new(),
            poly_api_secret: String::new(),
            poly_api_passphrase: String::new(),

            logs_dir: "logs".to_string(),
        }
    }
}

impl Config {
    /// Load credentials from environment variables.
    pub fn load_credentials(&mut self) {
        if let Ok(v) = std::env::var("POLY_PRIVATE_KEY") { self.poly_private_key = v; }
        if let Ok(v) = std::env::var("POLY_API_KEY") { self.poly_api_key = v; }
        if let Ok(v) = std::env::var("POLY_SECRET") { self.poly_api_secret = v; }
        if let Ok(v) = std::env::var("POLY_PASSPHRASE") { self.poly_api_passphrase = v; }
    }

    pub fn has_credentials(&self) -> bool {
        !self.poly_private_key.is_empty()
            && !self.poly_api_key.is_empty()
            && !self.poly_api_secret.is_empty()
            && !self.poly_api_passphrase.is_empty()
    }
}
