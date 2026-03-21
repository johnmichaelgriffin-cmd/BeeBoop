//! Production signal engine — 4-feature stacked directional scoring.
//!
//! Features:
//!   A. Fast move stack (200/500/800ms) — primary trigger
//!   B. 2s return — persistence/transmission confirmation
//!   C. Oracle basis — direct expression of lag edge
//!   D. OBI — microstructure filter
//!
//! Event-driven: fires on every Binance tick, no polling.
//! Only emits Signal when all gates pass.

use std::collections::VecDeque;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::config_v2::Config;
use crate::state::SharedState;
use crate::types_v2::*;

// ── Rolling mid buffer ──────────────────────────────────────────

struct MidBuffer {
    data: VecDeque<(i64, f64)>,
    max_age_ms: i64,
}

impl MidBuffer {
    fn new(max_age_ms: i64) -> Self {
        Self { data: VecDeque::with_capacity(2000), max_age_ms }
    }

    fn push(&mut self, ts_ms: i64, mid: f64) {
        self.data.push_back((ts_ms, mid));
        while self.data.front().map(|(t, _)| ts_ms - *t > self.max_age_ms).unwrap_or(false) {
            self.data.pop_front();
        }
    }

    fn get_at_or_before(&self, target_ts_ms: i64) -> Option<f64> {
        self.data.iter().rev().find(|(ts, _)| *ts <= target_ts_ms).map(|(_, mid)| *mid)
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn bps_return(now: f64, then: f64) -> f64 {
    if then == 0.0 { return 0.0; }
    ((now - then) / then) * 10_000.0
}

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

fn sign_matches(side: Side, x: f64) -> bool {
    match side {
        Side::Up => x > 0.0,
        Side::Down => x < 0.0,
    }
}

// ── Signal Engine Task ──────────────────────────────────────────

pub async fn run_signal_engine_task(
    config: Config,
    shared: SharedState,
    mut binance_rx: broadcast::Receiver<BinanceTick>,
    market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    chainlink_rx: watch::Receiver<Option<ChainlinkTick>>,
    signal_tx: mpsc::Sender<Signal>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    let mut mids = MidBuffer::new(5_000);
    let mut tick_count: u64 = 0;
    let mut last_heartbeat_ms: i64 = 0;
    let mut last_max_fast_move: f64 = 0.0;
    let mut signals_emitted: u64 = 0;

    info!("signal_engine: started (trigger={:.2}bps, maker={:.2}, taker={:.2})",
        config.trigger_min_fast_move_bps, config.maker_score_min, config.taker_score_min);

    loop {
        let tick = match binance_rx.recv().await {
            Ok(t) => t,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("signal_engine: lagged {} messages", n);
                continue;
            }
            Err(_) => break,
        };

        let now = tick.recv_ts_ms;
        let mid_now = tick.mid;
        mids.push(now, mid_now);
        tick_count += 1;

        // Need all 4 lookbacks available (2s of history minimum)
        if mids.len() < 50 { continue; }

        let mid_200  = match mids.get_at_or_before(now - 200)  { Some(v) => v, None => continue };
        let mid_500  = match mids.get_at_or_before(now - 500)  { Some(v) => v, None => continue };
        let mid_800  = match mids.get_at_or_before(now - 800)  { Some(v) => v, None => continue };
        let mid_2000 = match mids.get_at_or_before(now - 2000) { Some(v) => v, None => continue };

        // ── Feature A: Fast move composite ──────────────────
        let r200  = bps_return(mid_now, mid_200);
        let r500  = bps_return(mid_now, mid_500);
        let r800  = bps_return(mid_now, mid_800);
        let r2000 = bps_return(mid_now, mid_2000);

        let fast_move_bps = 0.50 * r200 + 0.30 * r500 + 0.20 * r800;

        // Track for heartbeat
        if fast_move_bps.abs() > last_max_fast_move.abs() {
            last_max_fast_move = fast_move_bps;
        }

        // ── Heartbeat every 30s ─────────────────────────────
        if now - last_heartbeat_ms > 30_000 {
            let pm_top = pm_top_rx.borrow().clone();
            let cl = chainlink_rx.borrow().clone();
            let cl_price = cl.as_ref().map(|c| c.price).unwrap_or(0.0);
            info!("HEARTBEAT: {} ticks, {} signals | BN=${:.2} CL=${:.2} | max_fast={:.1}bps | UP={} DN={} | {:?}",
                tick_count, signals_emitted, mid_now, cl_price, last_max_fast_move,
                pm_top.up_ask.map(|a| format!("{:.0}c", a*100.0)).unwrap_or("?".into()),
                pm_top.down_ask.map(|a| format!("{:.0}c", a*100.0)).unwrap_or("?".into()),
                shared.get_state(),
            );
            last_heartbeat_ms = now;
            last_max_fast_move = 0.0;
        }

        // ── Gate 1: Trigger minimum ─────────────────────────
        if fast_move_bps.abs() < config.trigger_min_fast_move_bps {
            continue;
        }

        // ── Gate 2: State check ─────────────────────────────
        if !shared.is_idle() {
            continue;
        }

        // ── Gate 3: Cooldown ────────────────────────────────
        if shared.is_cooling_down(now) {
            continue;
        }

        // ── Gate 4: Window timing ───────────────────────────
        {
            let market = market_rx.borrow().clone();
            if market.window_end_ts > 0 {
                let now_s = now / 1000;
                let elapsed = now_s - market.window_start_ts;
                let time_to_end = market.window_end_ts - now_s;
                if elapsed < config.start_delay_s as i64 { continue; }
                if elapsed >= config.cancel_at_s as i64 || time_to_end < config.min_time_to_expiry_s as i64 { continue; }
            }
        }

        // ── Gate 5: Book exists + spread check ──────────────
        let side = if fast_move_bps > 0.0 { Side::Up } else { Side::Down };
        let pm_top = pm_top_rx.borrow().clone();
        let (bid_opt, ask_opt) = match side {
            Side::Up => (pm_top.up_bid, pm_top.up_ask),
            Side::Down => (pm_top.down_bid, pm_top.down_ask),
        };
        let (bid, ask) = match (bid_opt, ask_opt) {
            (Some(b), Some(a)) => (b, a),
            _ => continue,
        };
        let spread_cents = (ask - bid) * 100.0;
        if spread_cents > config.max_spread_cents { continue; }
        if ask > config.max_entry_price { continue; }

        // ── Gate 6: Chainlink available ─────────────────────
        let chainlink = match chainlink_rx.borrow().clone() {
            Some(c) => c,
            None => continue,
        };

        // ── Feature C: Oracle basis ─────────────────────────
        let basis_bps = ((mid_now - chainlink.price) / chainlink.price) * 10_000.0;

        // ── Feature D: OBI ──────────────────────────────────
        let obi = tick.obi.unwrap_or(0.0);

        // ── Gate 7: Confirmation (2 of 3 must agree) ────────
        let confirms_r2s = sign_matches(side, r2000);
        let confirms_basis = sign_matches(side, basis_bps);
        let confirms_obi = sign_matches(side, obi);
        let confirm_count = confirms_r2s as u8 + confirms_basis as u8 + confirms_obi as u8;

        if confirm_count < 2 {
            continue;
        }

        // ── Normalized feature stack ────────────────────────
        let norm_fast  = clamp(fast_move_bps / 3.0, -2.0, 2.0);
        let norm_r2s   = clamp(r2000 / 4.0, -2.0, 2.0);
        let norm_basis = clamp(basis_bps / 3.0, -2.0, 2.0);
        let norm_obi   = clamp(obi / 0.35, -2.0, 2.0);

        let score = 0.35 * norm_fast + 0.25 * norm_r2s + 0.25 * norm_basis + 0.15 * norm_obi;

        // Score must agree with side
        if !sign_matches(side, score) { continue; }

        let abs_score = score.abs();

        // ── Gate 8: Score minimum ───────────────────────────
        if abs_score < config.maker_score_min {
            continue;
        }

        // ── Entry mode selection ────────────────────────────
        let entry_mode = if abs_score >= config.taker_score_min {
            EntryMode::Taker
        } else {
            EntryMode::Taker // FOK only for now — maker not working yet
            // TODO: switch to EntryMode::Maker when GTC sells work
        };

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
            basis_bps,
            obi,
            score,
            confidence,
            confirms_r2s,
            confirms_basis,
            confirms_obi,
        };

        signals_emitted += 1;

        info!("SIGNAL #{}: {} score={:.2} fast={:.1}bps r2s={:.1}bps basis={:.1}bps obi={:.2} conf={}/{} | BN=${:.2}",
            signals_emitted,
            if side == Side::Up { "UP" } else { "DN" },
            score, fast_move_bps, r2000, basis_bps, obi,
            confirm_count, 3,
            mid_now,
        );

        let _ = log_tx.send(LogEvent::SignalSeen { ts_ms: now, signal: signal.clone() }).await;
        let _ = signal_tx.send(signal).await;
    }
}
