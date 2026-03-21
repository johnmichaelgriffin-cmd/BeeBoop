//! Corrected production signal engine — debiased basis, EMA smoothing, no hard confirms.
//!
//! Features:
//!   A. Fast move stack (200/500/800ms weighted composite) — primary trigger
//!   B. 2s return — persistence/transmission feature
//!   C. Debiased oracle basis + basis slope — direct expression of lag edge
//!   D. Smoothed OBI — microstructure filter
//!
//! Key corrections from v1:
//!   - No hard confirm_count >= 2 gate (collapsed trade frequency)
//!   - Basis debiased via 60s EMA (raw basis had persistent offset)
//!   - Basis slope (250ms delta) captures real-time oracle lag change
//!   - OBI smoothed via 250ms EMA (raw was too noisy)
//!   - Lower thresholds: trigger=0.70bps, maker=0.45, taker=0.90
//!   - Ultra-fast override: r200 >= 2.0bps + score >= 0.75 → force taker
//!   - Only hard-veto on strong contradiction (all 3 features strongly opposite)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

// ── Rolling time-series buffer ──────────────────────────────────

pub struct TimeSeriesBuffer {
    data: VecDeque<(i64, f64)>,
    max_age_ms: i64,
}

impl TimeSeriesBuffer {
    pub fn new(max_age_ms: i64) -> Self {
        Self {
            data: VecDeque::new(),
            max_age_ms,
        }
    }

    pub fn push(&mut self, ts_ms: i64, value: f64) {
        self.data.push_back((ts_ms, value));
        while let Some((old_ts, _)) = self.data.front() {
            if ts_ms - *old_ts > self.max_age_ms {
                self.data.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn get_at_or_before(&self, target_ts_ms: i64) -> Option<f64> {
        for (ts, value) in self.data.iter().rev() {
            if *ts <= target_ts_ms {
                return Some(*value);
            }
        }
        None
    }
}

// ── Exponential moving average ──────────────────────────────────

pub struct Ema {
    value: Option<f64>,
    tau_ms: f64,
    last_ts_ms: Option<i64>,
}

impl Ema {
    pub fn new(tau_ms: f64) -> Self {
        Self {
            value: None,
            tau_ms,
            last_ts_ms: None,
        }
    }

    pub fn update(&mut self, ts_ms: i64, x: f64) -> f64 {
        match (self.value, self.last_ts_ms) {
            (None, _) => {
                self.value = Some(x);
                self.last_ts_ms = Some(ts_ms);
                x
            }
            (Some(prev), Some(last_ts)) => {
                let dt = ((ts_ms - last_ts).max(1)) as f64;
                let alpha = 1.0 - (-dt / self.tau_ms).exp();
                let next = prev + alpha * (x - prev);
                self.value = Some(next);
                self.last_ts_ms = Some(ts_ms);
                next
            }
            _ => {
                self.value = Some(x);
                self.last_ts_ms = Some(ts_ms);
                x
            }
        }
    }
}

// ── Operational counters ────────────────────────────────────────

#[derive(Default)]
pub struct SignalCounters {
    pub ticks_processed: AtomicU64,
    pub skip_missing_lookback: AtomicU64,
    pub skip_fast_trigger_floor: AtomicU64,
    pub skip_state_not_idle: AtomicU64,
    pub skip_cooldown: AtomicU64,
    pub skip_near_expiry: AtomicU64,
    pub skip_missing_book: AtomicU64,
    pub skip_spread: AtomicU64,
    pub skip_entry_price_high: AtomicU64,
    pub skip_missing_chainlink: AtomicU64,
    pub skip_score_sign_mismatch: AtomicU64,
    pub skip_strong_contradiction: AtomicU64,
    pub skip_score_floor: AtomicU64,
    pub signals_emitted: AtomicU64,
    pub signals_taker: AtomicU64,
    pub signals_maker: AtomicU64,
}

// ── Helpers ─────────────────────────────────────────────────────

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

fn bps_return(now: f64, then: f64) -> f64 {
    ((now - then) / then) * 10_000.0
}

fn sign_matches(side: Side, x: f64) -> bool {
    match side {
        Side::Up => x > 0.0,
        Side::Down => x < 0.0,
    }
}

fn strong_contradiction(side: Side, basis_dev_bps: f64, dbasis_bps: f64, r2000_bps: f64) -> bool {
    match side {
        Side::Up => basis_dev_bps < -1.5 && dbasis_bps < -0.8 && r2000_bps < -2.0,
        Side::Down => basis_dev_bps > 1.5 && dbasis_bps > 0.8 && r2000_bps > 2.0,
    }
}

// ── Main signal engine task ─────────────────────────────────────

pub async fn run_signal_engine_task(
    config: Config,
    shared: SharedState,
    mut binance_rx: broadcast::Receiver<BinanceTick>,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    chainlink_rx: watch::Receiver<Option<ChainlinkTick>>,
    signal_tx: mpsc::Sender<Signal>,
    log_tx: mpsc::Sender<LogEvent>,
) -> anyhow::Result<()> {
    let mut mid_buf = TimeSeriesBuffer::new(5_000);
    let mut basis_buf = TimeSeriesBuffer::new(5_000);
    let mut basis_ema = Ema::new(config.basis_ema_tau_ms);
    let mut obi_ema = Ema::new(config.obi_ema_tau_ms);

    let counters = Arc::new(SignalCounters::default());
    let c = counters.clone();

    // Heartbeat logger — every 30s
    let log_tx_hb = log_tx.clone();
    let shared_hb = shared.clone();
    let pm_top_hb = pm_top_rx.clone();
    let cl_hb = chainlink_rx.clone();
    tokio::spawn(async move {
        let mut max_fast = 0.0f64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let ticks = c.ticks_processed.load(Ordering::Relaxed);
            let signals = c.signals_emitted.load(Ordering::Relaxed);
            let state = *shared_hb.strategy_state.read();
            let top = pm_top_hb.borrow().clone();
            let cl = cl_hb.borrow().clone();
            let cl_price = cl.map(|c| c.price).unwrap_or(0.0);

            info!(
                "HEARTBEAT: {} ticks, {} signals | CL=${:.2} | UP ask={}c DN ask={}c | {:?} | skips: trigger={} book={} spread={} cl={} score={} contra={}",
                ticks, signals, cl_price,
                top.up_ask.map(|a| format!("{:.0}", a * 100.0)).unwrap_or("?".into()),
                top.down_ask.map(|a| format!("{:.0}", a * 100.0)).unwrap_or("?".into()),
                state,
                c.skip_fast_trigger_floor.load(Ordering::Relaxed),
                c.skip_missing_book.load(Ordering::Relaxed),
                c.skip_spread.load(Ordering::Relaxed),
                c.skip_missing_chainlink.load(Ordering::Relaxed),
                c.skip_score_floor.load(Ordering::Relaxed),
                c.skip_strong_contradiction.load(Ordering::Relaxed),
            );
        }
    });

    info!(
        "signal_engine: started (trigger={:.2}bps, maker={:.2}, taker={:.2})",
        config.trigger_min_fast_move_bps, config.maker_score_min, config.taker_score_min,
    );

    loop {
        let tick = match binance_rx.recv().await {
            Ok(t) => t,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("signal_engine: lagged {} ticks", n);
                continue;
            }
            Err(_) => continue,
        };

        counters.ticks_processed.fetch_add(1, Ordering::Relaxed);
        let now = tick.recv_ts_ms;
        let mid_now = tick.mid;
        mid_buf.push(now, mid_now);

        // Get all lookback returns
        let mid_200 = match mid_buf.get_at_or_before(now - 200) {
            Some(v) => v,
            None => { counters.skip_missing_lookback.fetch_add(1, Ordering::Relaxed); continue; }
        };
        let mid_500 = match mid_buf.get_at_or_before(now - 500) {
            Some(v) => v,
            None => { counters.skip_missing_lookback.fetch_add(1, Ordering::Relaxed); continue; }
        };
        let mid_800 = match mid_buf.get_at_or_before(now - 800) {
            Some(v) => v,
            None => { counters.skip_missing_lookback.fetch_add(1, Ordering::Relaxed); continue; }
        };
        let mid_2000 = match mid_buf.get_at_or_before(now - 2000) {
            Some(v) => v,
            None => { counters.skip_missing_lookback.fetch_add(1, Ordering::Relaxed); continue; }
        };

        let r200 = bps_return(mid_now, mid_200);
        let r500 = bps_return(mid_now, mid_500);
        let r800 = bps_return(mid_now, mid_800);
        let r2000 = bps_return(mid_now, mid_2000);

        // Weighted fast move composite
        let fast_move_bps = 0.55 * r200 + 0.30 * r500 + 0.15 * r800;

        // First gate: ignore tiny moves
        if fast_move_bps.abs() < config.trigger_min_fast_move_bps {
            counters.skip_fast_trigger_floor.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // State gate
        if *shared.strategy_state.read() != StrategyState::Idle {
            counters.skip_state_not_idle.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Cooldown gate
        if let Some(cooldown_until) = *shared.cooldown_until_ms.read() {
            if now < cooldown_until {
                counters.skip_cooldown.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }

        // Market expiry gate
        let market = market_rx.borrow().clone();
        let time_to_expiry_s = market.window_end_ts - (now / 1000);
        if time_to_expiry_s < config.min_time_to_expiry_s as i64 {
            counters.skip_near_expiry.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Direction from fast move
        let side = if fast_move_bps > 0.0 { Side::Up } else { Side::Down };

        // Book gate
        let top = pm_top_rx.borrow().clone();
        let (bid_opt, ask_opt) = match side {
            Side::Up => (top.up_bid, top.up_ask),
            Side::Down => (top.down_bid, top.down_ask),
        };
        let (bid, ask) = match (bid_opt, ask_opt) {
            (Some(b), Some(a)) => (b, a),
            _ => { counters.skip_missing_book.fetch_add(1, Ordering::Relaxed); continue; }
        };

        let spread_cents = (ask - bid) * 100.0;
        if spread_cents > config.max_spread_cents {
            counters.skip_spread.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if ask > config.max_entry_price {
            counters.skip_entry_price_high.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Chainlink basis — corrected with debiasing
        let chainlink = match chainlink_rx.borrow().clone() {
            Some(c) => c,
            None => { counters.skip_missing_chainlink.fetch_add(1, Ordering::Relaxed); continue; }
        };

        let raw_basis_bps = ((mid_now - chainlink.price) / chainlink.price) * 10_000.0;
        let basis_mean = basis_ema.update(now, raw_basis_bps);
        basis_buf.push(now, raw_basis_bps);

        let basis_dev_bps = raw_basis_bps - basis_mean;
        let dbasis_bps = match basis_buf.get_at_or_before(now - config.basis_slope_lookback_ms) {
            Some(prev_basis) => raw_basis_bps - prev_basis,
            None => 0.0,
        };

        // Smoothed OBI
        let obi_raw = tick.obi.unwrap_or(0.0);
        let obi_ema_value = obi_ema.update(now, obi_raw);

        // Normalized feature stack
        let norm_fast = clamp(fast_move_bps / 1.5, -2.0, 2.0);
        let norm_r2s = clamp(r2000 / 3.0, -2.0, 2.0);
        let norm_basis = clamp(basis_dev_bps / 2.0, -2.0, 2.0);
        let norm_dbasis = clamp(dbasis_bps / 1.0, -2.0, 2.0);
        let norm_obi = clamp(obi_ema_value / 0.35, -2.0, 2.0);

        // Combined score — corrected production weighting
        let score =
            0.45 * norm_fast +
            0.15 * norm_r2s +
            0.20 * norm_basis +
            0.10 * norm_dbasis +
            0.10 * norm_obi;

        // Score must agree with side direction
        if !sign_matches(side, score) {
            counters.skip_score_sign_mismatch.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Strong contradiction veto
        let contradiction = strong_contradiction(side, basis_dev_bps, dbasis_bps, r2000);
        if contradiction {
            counters.skip_strong_contradiction.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let abs_score = score.abs();

        // Score floor gate
        if abs_score < config.maker_score_min {
            counters.skip_score_floor.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Maker / taker decision
        let mut entry_mode = if abs_score >= config.taker_score_min {
            EntryMode::Taker
        } else {
            EntryMode::Taker // FOK taker only — GTC/maker doesn't work on sells
        };

        // Ultra-fast override: burst events force taker
        if r200.abs() >= 2.0 && abs_score >= 0.75 && spread_cents <= config.max_spread_cents {
            entry_mode = EntryMode::Taker;
        }

        let confidence = clamp(abs_score / 1.5, 0.0, 2.0);

        let signal = Signal {
            created_ts_ms: now,
            side,
            entry_mode,
            r200_bps: r200,
            r500_bps: r500,
            r800_bps: r800,
            r2000_bps: r2000,
            fast_move_bps,
            raw_basis_bps,
            basis_dev_bps,
            dbasis_bps,
            obi_ema: obi_ema_value,
            score,
            confidence,
            strong_contradiction: contradiction,
        };

        counters.signals_emitted.fetch_add(1, Ordering::Relaxed);
        match entry_mode {
            EntryMode::Taker => { counters.signals_taker.fetch_add(1, Ordering::Relaxed); }
            EntryMode::Maker => { counters.signals_maker.fetch_add(1, Ordering::Relaxed); }
        }

        info!(
            ">>> SIGNAL: {:?} score={:.3} fast={:.2}bps r2s={:.2}bps basis_dev={:.2}bps dbasis={:.2}bps obi={:.3} | {}ask={:.0}c",
            side, score, fast_move_bps, r2000, basis_dev_bps, dbasis_bps, obi_ema_value,
            if side == Side::Up { "UP " } else { "DN " },
            ask * 100.0,
        );

        let _ = signal_tx.send(signal.clone()).await;
        let _ = log_tx.send(LogEvent::SignalSeen {
            ts_ms: now,
            signal,
        }).await;
    }
}
