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

        let chainlink = chainlink_rx.borrow().clone();
        let basis_bps = chainlink
            .as_ref()
            .map(|cl| compute_basis_bps(tick.mid, cl.price))
            .unwrap_or(0.0);

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
