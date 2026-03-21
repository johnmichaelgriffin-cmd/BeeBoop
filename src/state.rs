//! Shared state for BeeBoop v2 — narrow lock scopes.

use std::sync::Arc;
use parking_lot::RwLock;
use crate::types_v2::{Position, StrategyState};

#[derive(Clone)]
pub struct SharedState {
    pub strategy_state: Arc<RwLock<StrategyState>>,
    pub current_position: Arc<RwLock<Option<Position>>>,
    pub cooldown_until_ms: Arc<RwLock<Option<i64>>>,
    pub realized_pnl: Arc<RwLock<f64>>,
    pub session_start_balance: Arc<RwLock<f64>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            strategy_state: Arc::new(RwLock::new(StrategyState::Idle)),
            current_position: Arc::new(RwLock::new(None)),
            cooldown_until_ms: Arc::new(RwLock::new(None)),
            realized_pnl: Arc::new(RwLock::new(0.0)),
            session_start_balance: Arc::new(RwLock::new(0.0)),
        }
    }

    pub fn is_idle(&self) -> bool {
        *self.strategy_state.read() == StrategyState::Idle
    }

    pub fn set_state(&self, new_state: StrategyState) {
        *self.strategy_state.write() = new_state;
    }

    pub fn get_state(&self) -> StrategyState {
        *self.strategy_state.read()
    }

    pub fn set_position(&self, pos: Position) {
        *self.current_position.write() = Some(pos);
    }

    pub fn clear_position(&self) {
        *self.current_position.write() = None;
    }

    pub fn get_position(&self) -> Option<Position> {
        self.current_position.read().clone()
    }

    pub fn set_cooldown(&self, until_ms: i64) {
        *self.cooldown_until_ms.write() = Some(until_ms);
    }

    pub fn is_cooling_down(&self, now_ms: i64) -> bool {
        self.cooldown_until_ms.read()
            .map(|until| now_ms < until)
            .unwrap_or(false)
    }

    pub fn add_pnl(&self, pnl: f64) {
        *self.realized_pnl.write() += pnl;
    }

    pub fn get_pnl(&self) -> f64 {
        *self.realized_pnl.read()
    }

    pub fn should_stop(&self, stop_loss: f64) -> bool {
        self.get_pnl() <= stop_loss
    }
}
