//! Configuration for BeeBoop v2 sniper bot — production defaults.

use crate::types_v2::BotMode;

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: BotMode,
    pub market: String,

    // Signal params
    pub lookback_ms: i64,
    pub basis_slope_lookback_ms: i64,
    pub basis_ema_tau_ms: f64,
    pub obi_ema_tau_ms: f64,
    pub trigger_min_fast_move_bps: f64,
    pub maker_score_min: f64,
    pub taker_score_min: f64,
    pub max_signal_age_ms: i64,
    pub maker_max_signal_age_ms: i64,

    // Exit params
    pub take_profit_cents: f64,
    pub stop_loss_cents: f64,
    pub max_hold_ms: i64,
    pub cooldown_ms: i64,
    pub opposite_signal_exit_score: f64,

    // Sizing
    pub base_notional: f64,
    pub max_notional: f64,

    // Risk
    pub max_one_position: bool,
    pub session_stop_loss: f64,
    pub max_spread_cents: f64,
    pub min_entry_price: f64,
    pub max_entry_price: f64,
    pub min_time_to_expiry_s: u64,
    pub wallet_check_interval_s: u64,

    // Window timing
    pub window_seconds: u64,
    pub start_delay_s: u64,
    pub cancel_at_s: u64,

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

            // Signal — corrected production defaults
            lookback_ms: 800,
            basis_slope_lookback_ms: 250,
            basis_ema_tau_ms: 60_000.0,
            obi_ema_tau_ms: 250.0,
            trigger_min_fast_move_bps: 0.55,
            maker_score_min: 0.35,
            taker_score_min: 0.80,
            max_signal_age_ms: 1400,
            maker_max_signal_age_ms: 900,

            // Exit — production defaults
            take_profit_cents: 4.0,
            stop_loss_cents: 3.0,
            max_hold_ms: 2_500,
            cooldown_ms: 2_000,
            opposite_signal_exit_score: 1.2,

            // Sizing
            base_notional: 25.0,
            max_notional: 150.0,

            // Risk
            max_one_position: true,
            session_stop_loss: -200.0,
            max_spread_cents: 4.0,
            min_entry_price: 0.15,
            max_entry_price: 0.97,
            min_time_to_expiry_s: 20,
            wallet_check_interval_s: 300,

            // Window
            window_seconds: 300,
            start_delay_s: 15,
            cancel_at_s: 240,

            // Credentials
            poly_private_key: String::new(),
            poly_api_key: String::new(),
            poly_api_secret: String::new(),
            poly_api_passphrase: String::new(),

            logs_dir: "logs".to_string(),
        }
    }
}

impl Config {
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
