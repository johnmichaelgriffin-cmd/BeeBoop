//! Signal engine — event-driven, fires on Binance ticks.
//!
//! Maintains a rolling buffer of Binance mid prices.
//! On every tick, computes move_bps vs lookback and basis vs Chainlink.
//! Emits Signal only when threshold is crossed and state is Idle.

use std::collections::VecDeque;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::info;

use crate::config_v2::Config;
use crate::signals::features::{compute_basis_bps, compute_move_bps};
use crate::state::SharedState;
use crate::types_v2::*;

/// Rolling buffer entry.
struct MidEntry {
    ts_ms: i64,
    mid: f64,
}

pub async fn run_signal_engine_task(
    config: Config,
    shared: SharedState,
    mut binance_rx: broadcast::Receiver<BinanceTick>,
    _market_rx: watch::Receiver<MarketDescriptor>,
    pm_top_rx: watch::Receiver<PolymarketTop>,
    chainlink_rx: watch::Receiver<Option<ChainlinkTick>>,
    signal_tx: mpsc::Sender<Signal>,
    log_tx: mpsc::Sender<LogEvent>,
) {
    let mut buffer: VecDeque<MidEntry> = VecDeque::with_capacity(1000);
    let max_age_ms = 5_000i64; // keep 5s of history
    let mut tick_count: u64 = 0;
    let mut last_heartbeat_ms: i64 = 0;
    let mut last_max_move: f64 = 0.0;

    info!("signal_engine: started (lookback={}ms, threshold={}bps)",
        config.lookback_ms, config.entry_threshold_bps);

    loop {
        let tick = match binance_rx.recv().await {
            Ok(t) => t,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("signal_engine: lagged {} messages", n);
                continue;
            }
            Err(_) => break,
        };

        // Add to rolling buffer
        buffer.push_back(MidEntry {
            ts_ms: tick.recv_ts_ms,
            mid: tick.mid,
        });

        // Trim old entries
        let cutoff = tick.recv_ts_ms - max_age_ms;
        while buffer.front().map(|e| e.ts_ms < cutoff).unwrap_or(false) {
            buffer.pop_front();
        }

        // Need enough history
        if buffer.len() < 10 {
            continue;
        }

        // Find mid from lookback_ms ago
        let lookback_ts = tick.recv_ts_ms - config.lookback_ms;
        let lookback_mid = buffer.iter()
            .rev()
            .find(|e| e.ts_ms <= lookback_ts)
            .map(|e| e.mid);

        let Some(past_mid) = lookback_mid else { continue };

        // Compute features
        let move_bps = compute_move_bps(tick.mid, past_mid);
        tick_count += 1;

        // Track max move for heartbeat
        if move_bps.abs() > last_max_move.abs() {
            last_max_move = move_bps;
        }

        let chainlink = chainlink_rx.borrow().clone();
        let basis_bps = chainlink
            .as_ref()
            .map(|cl| compute_basis_bps(tick.mid, cl.price))
            .unwrap_or(0.0);

        // Heartbeat every 30s
        let now_ms_hb = chrono::Utc::now().timestamp_millis();
        if now_ms_hb - last_heartbeat_ms > 30_000 {
            let pm_top = pm_top_rx.borrow().clone();
            let cl_price = chainlink.as_ref().map(|c| c.price).unwrap_or(0.0);
            info!("HEARTBEAT: {} ticks | BN=${:.2} CL=${:.2} | max_move={:.1}bps | UP ask={} DN ask={} | state={:?}",
                tick_count, tick.mid, cl_price, last_max_move,
                pm_top.up_ask.map(|a| format!("{:.0}c", a*100.0)).unwrap_or("?".into()),
                pm_top.down_ask.map(|a| format!("{:.0}c", a*100.0)).unwrap_or("?".into()),
                shared.get_state(),
            );
            last_heartbeat_ms = now_ms_hb;
            last_max_move = 0.0;
        }

        // Check threshold
        if move_bps.abs() < config.entry_threshold_bps {
            continue;
        }

        // Only emit if state is Idle
        if !shared.is_idle() {
            continue;
        }

        // Check cooldown
        let now_ms = chrono::Utc::now().timestamp_millis();
        if shared.is_cooling_down(now_ms) {
            continue;
        }

        // Window timing gate — don't enter too early or too late
        {
            let market = _market_rx.borrow().clone();
            if market.window_end_ts > 0 {
                let now_s = now_ms / 1000;
                let elapsed = now_s - market.window_start_ts;
                let time_to_end = market.window_end_ts - now_s;

                // Don't enter before T+15s (let orderbook populate)
                if elapsed < 15 {
                    continue;
                }
                // Don't enter after T+240s (too close to resolution)
                if elapsed >= 240 || time_to_end < 20 {
                    continue;
                }
            }
        }

        // Check Polymarket book exists
        let pm_top = pm_top_rx.borrow().clone();
        let has_book = if move_bps > 0.0 {
            pm_top.up_ask.is_some()
        } else {
            pm_top.down_ask.is_some()
        };
        if !has_book {
            continue;
        }

        // Determine side and confidence
        let side = if move_bps > 0.0 { Side::Up } else { Side::Down };
        let confidence = (move_bps.abs() / config.entry_threshold_bps).min(2.0);

        let signal = Signal {
            created_ts_ms: now_ms,
            move_bps,
            basis_bps,
            confidence,
            side,
            entry_mode: EntryMode::Taker,
            reason: format!(
                "move={:.1}bps basis={:.1}bps conf={:.2}",
                move_bps, basis_bps, confidence
            ),
        };

        info!("SIGNAL: {} move={:.1}bps basis={:.1}bps | BN=${:.2} CL=${:.2}",
            if side == Side::Up { "UP" } else { "DN" },
            move_bps, basis_bps, tick.mid,
            chainlink.as_ref().map(|c| c.price).unwrap_or(0.0),
        );

        let _ = log_tx.send(LogEvent::SignalSeen {
            ts_ms: now_ms,
            signal: signal.clone(),
        }).await;

        let _ = signal_tx.send(signal).await;
    }
}
