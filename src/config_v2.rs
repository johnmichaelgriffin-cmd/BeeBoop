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
    pub share_base: f64,            // lockprofit formula base: floor(share_base * ask) shares @ 99c
    pub max_pairs_per_window: u32,  // max snipes per window
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

    // Tuning — risk-shaping thresholds
    pub tune: TuneConfig,
}

/// Risk-shaping thresholds — pair cost gates, residual caps, regime filters.
#[derive(Debug, Clone)]
pub struct TuneConfig {
    // Pair-cost gates
    pub pair_cost_full_size_max: f64,
    pub pair_cost_reduce_max: f64,
    pub pair_cost_strong_only_max: f64,
    // Residual controls
    pub weak_signal_max_ratio: f64,
    pub med_signal_max_ratio: f64,
    pub strong_signal_max_ratio: f64,
    pub extreme_signal_max_ratio: f64,
    pub soft_cap_fraction: f64,
    pub soft_freeze_pair_cost: f64,
    // Exp-heavy override
    pub exp_heavy_override_score: f64,
    pub exp_heavy_strong_score: f64,
    pub exp_heavy_pair_cost_max: f64,
    pub base_cheap_target: f64,
    pub neutral_cheap_target: f64,
    pub exp_heavy_cheap_target: f64,
    // Extreme-price filter
    pub extreme_reduce_exp_price: f64,
    pub extreme_reduce_cheap_price: f64,
    pub extreme_hard_exp_price: f64,
    pub extreme_hard_cheap_price: f64,
    pub extreme_size_multiplier: f64,
    pub extreme_edge_add_cents: f64,
    // Rolling regime filter
    pub rolling_short_windows: usize,
    pub rolling_pause_pnl_per_window: f64,
    pub rolling_pause_pair_cost: f64,
    pub rolling_resume_pnl_per_window: f64,
    pub rolling_resume_pair_cost: f64,
    // Min pairs before pair-cost gating activates
    pub min_pairs_for_cost_gate: f64,
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self {
            pair_cost_full_size_max: 0.995,
            pair_cost_reduce_max: 1.010,
            pair_cost_strong_only_max: 1.030,
            weak_signal_max_ratio: 1.20,
            med_signal_max_ratio: 1.50,
            strong_signal_max_ratio: 2.25,
            extreme_signal_max_ratio: 3.00,
            soft_cap_fraction: 0.80,
            soft_freeze_pair_cost: 0.995,
            exp_heavy_override_score: 1.40,
            exp_heavy_strong_score: 2.00,
            exp_heavy_pair_cost_max: 0.990,
            base_cheap_target: 0.545,
            neutral_cheap_target: 0.500,
            exp_heavy_cheap_target: 0.470,
            extreme_reduce_exp_price: 0.80,
            extreme_reduce_cheap_price: 0.20,
            extreme_hard_exp_price: 0.90,
            extreme_hard_cheap_price: 0.10,
            extreme_size_multiplier: 0.50,
            extreme_edge_add_cents: 0.005,
            rolling_short_windows: 6,
            rolling_pause_pnl_per_window: -5.0,
            rolling_pause_pair_cost: 1.010,
            rolling_resume_pnl_per_window: 5.0,
            rolling_resume_pair_cost: 0.995,
            min_pairs_for_cost_gate: 50.0,
        }
    }
}

/// Signal strength band
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalBand {
    Weak,
    Medium,
    Strong,
    Extreme,
}

/// Pair cost quoting mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PairCostMode {
    Full,
    Reduced,
    StrongOnly,
    Stop,
}

impl TuneConfig {
    pub fn signal_band(&self, score: f64) -> SignalBand {
        let abs_score = score.abs();
        if abs_score >= 2.00 { SignalBand::Extreme }
        else if abs_score >= 1.40 { SignalBand::Strong }
        else if abs_score >= 0.80 { SignalBand::Medium }
        else { SignalBand::Weak }
    }

    pub fn max_ratio_for_band(&self, band: SignalBand) -> f64 {
        match band {
            SignalBand::Weak => self.weak_signal_max_ratio,
            SignalBand::Medium => self.med_signal_max_ratio,
            SignalBand::Strong => self.strong_signal_max_ratio,
            SignalBand::Extreme => self.extreme_signal_max_ratio,
        }
    }

    pub fn pair_cost_mode(&self, pair_cost: f64, matched_pairs: f64) -> PairCostMode {
        if matched_pairs < self.min_pairs_for_cost_gate {
            return PairCostMode::Full; // not enough data yet
        }
        if pair_cost <= self.pair_cost_full_size_max { PairCostMode::Full }
        else if pair_cost <= self.pair_cost_reduce_max { PairCostMode::Reduced }
        else if pair_cost <= self.pair_cost_strong_only_max { PairCostMode::StrongOnly }
        else { PairCostMode::Stop }
    }

    pub fn cheap_target(&self, score: f64, pred_winner_is_exp: bool, pair_cost: Option<f64>) -> f64 {
        let abs_score = score.abs();
        let pc_ok = pair_cost.map_or(true, |pc| pc <= self.exp_heavy_pair_cost_max);

        if abs_score >= self.exp_heavy_strong_score && pred_winner_is_exp && pc_ok {
            self.exp_heavy_cheap_target
        } else if abs_score >= self.exp_heavy_override_score && pred_winner_is_exp && pc_ok {
            self.neutral_cheap_target
        } else {
            self.base_cheap_target
        }
    }
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
            trigger_min_fast_move_bps: 0.85,
            maker_score_min: 0.55,
            taker_score_min: 1.00,
            max_signal_age_ms: 1400,
            maker_max_signal_age_ms: 900,

            // Exit — production defaults
            take_profit_cents: 4.0,
            stop_loss_cents: 3.0,
            max_hold_ms: 2_500,
            cooldown_ms: 1_500,
            opposite_signal_exit_score: 1.2,

            // Sizing
            share_base: 20.0,           // BTC default: 20sh
            max_pairs_per_window: 50,   // BTC default: 50 snipes
            base_notional: 25.0,
            max_notional: 150.0,

            // Risk
            max_one_position: true,
            session_stop_loss: -2000.0,
            max_spread_cents: 4.0,
            min_entry_price: 0.15,
            max_entry_price: 0.97,
            min_time_to_expiry_s: 60,
            wallet_check_interval_s: 300,

            // Window
            window_seconds: 300,
            start_delay_s: 15,
            cancel_at_s: 180,

            // Credentials
            poly_private_key: String::new(),
            poly_api_key: String::new(),
            poly_api_secret: String::new(),
            poly_api_passphrase: String::new(),

            logs_dir: "logs".to_string(),

            tune: TuneConfig::default(),
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
