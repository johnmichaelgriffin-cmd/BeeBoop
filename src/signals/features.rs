//! Feature computation helpers.

pub fn compute_move_bps(mid_now: f64, mid_then: f64) -> f64 {
    if mid_then == 0.0 { return 0.0; }
    ((mid_now - mid_then) / mid_then) * 10_000.0
}

pub fn compute_basis_bps(binance_mid: f64, chainlink_price: f64) -> f64 {
    if chainlink_price == 0.0 { return 0.0; }
    ((binance_mid - chainlink_price) / chainlink_price) * 10_000.0
}
