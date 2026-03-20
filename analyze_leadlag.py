#!/usr/bin/env python3
"""
Lead/Lag Analysis — BeeBoop Data
Compares Binance vs Chainlink pricing to measure oracle lag.
"""

import json
import sys
import numpy as np
from collections import defaultdict
from pathlib import Path

def load_events(path, max_lines=None):
    """Load JSONL events into categorized lists."""
    binance_prices = []  # (local_recv_us, src_ts_ms, mid_price)
    chainlink_prices = []  # (local_recv_us, src_ts_ms, price)
    rtds_binance_prices = []  # (local_recv_us, src_ts_ms, price)
    obi_signals = []  # (local_recv_us, obi_value)

    count = 0
    with open(path, 'r') as f:
        for line in f:
            if max_lines and count >= max_lines:
                break
            count += 1

            try:
                evt = json.loads(line)
            except:
                continue

            local_us = evt.get("local_recv_us", 0)
            source = evt.get("source", "")
            etype = evt.get("event_type", "")
            raw = evt.get("raw_json", "")

            if not raw:
                continue

            try:
                data = json.loads(raw)
            except:
                continue

            # Binance bookTicker — L1 best bid/ask
            if source == "binance_l2" and etype == "bookticker":
                bid = float(data.get("b", 0))
                ask = float(data.get("a", 0))
                src_ts = int(data.get("T", 0))
                if bid > 0 and ask > 0:
                    mid = (bid + ask) / 2.0
                    binance_prices.append((local_us, src_ts, mid))

            # RTDS events
            elif source == "rtds" and etype == "raw":
                topic = data.get("topic", "")
                payload = data.get("payload", {})

                # Chainlink oracle price
                if topic == "crypto_prices_chainlink":
                    symbol = payload.get("symbol", "")
                    if "btc" in symbol.lower():
                        ts = payload.get("timestamp", 0)
                        val = payload.get("value", 0)
                        if val > 0:
                            chainlink_prices.append((local_us, ts, val))

                # RTDS Binance price (per-second snapshots)
                elif topic == "crypto_prices":
                    symbol = payload.get("symbol", "")
                    if "btc" in symbol.lower():
                        # This comes as a time series array
                        price_data = payload.get("data", [])
                        if isinstance(price_data, list) and len(price_data) > 0:
                            # Take the latest point
                            latest = price_data[-1]
                            ts = latest.get("timestamp", 0)
                            val = latest.get("value", 0)
                            if val > 0:
                                rtds_binance_prices.append((local_us, ts, val))

                # Subscribe messages also contain crypto_prices data
                elif topic == "" and data.get("type") == "subscribe":
                    # Initial subscription response
                    pass

            # OBI signals (if logged separately)
            elif etype == "obi":
                try:
                    obi_val = float(raw)
                    obi_signals.append((local_us, obi_val))
                except:
                    pass

    return {
        "binance_l1": binance_prices,
        "chainlink": chainlink_prices,
        "rtds_binance": rtds_binance_prices,
        "obi": obi_signals,
        "total_events": count,
    }


def resample_to_seconds(prices, use_src_ts=False):
    """Resample price series to 1-second bins using last value."""
    if not prices:
        return {}

    bins = {}
    for local_us, src_ts, price in prices:
        # Use source timestamp (ms) for alignment
        if use_src_ts and src_ts > 0:
            sec = src_ts // 1000
        else:
            sec = local_us // 1_000_000
        bins[sec] = price

    return bins


def compute_lag_correlation(series_a, series_b, max_lag_sec=30):
    """
    Compute cross-correlation of returns at different lags.
    Positive lag = series_a leads series_b.
    """
    # Find overlapping seconds
    common_secs = sorted(set(series_a.keys()) & set(series_b.keys()))
    if len(common_secs) < 100:
        return []

    # Compute returns
    returns_a = {}
    returns_b = {}
    for i in range(1, len(common_secs)):
        t = common_secs[i]
        t_prev = common_secs[i-1]
        if t - t_prev == 1:  # Only consecutive seconds
            pa = series_a[t]
            pa_prev = series_a[t_prev]
            pb = series_b[t]
            pb_prev = series_b[t_prev]
            if pa_prev > 0 and pb_prev > 0:
                returns_a[t] = (pa - pa_prev) / pa_prev
                returns_b[t] = (pb - pb_prev) / pb_prev

    common_ret = sorted(set(returns_a.keys()) & set(returns_b.keys()))
    if len(common_ret) < 50:
        return []

    ra = np.array([returns_a[t] for t in common_ret])
    rb = np.array([returns_b[t] for t in common_ret])

    results = []
    for lag in range(-max_lag_sec, max_lag_sec + 1):
        if lag >= 0:
            a_slice = ra[:len(ra)-lag] if lag > 0 else ra
            b_slice = rb[lag:] if lag > 0 else rb
        else:
            a_slice = ra[-lag:]
            b_slice = rb[:len(rb)+lag]

        if len(a_slice) < 30:
            continue

        # Pearson correlation
        if np.std(a_slice) > 0 and np.std(b_slice) > 0:
            corr = np.corrcoef(a_slice, b_slice)[0, 1]
        else:
            corr = 0.0

        results.append((lag, corr, len(a_slice)))

    return results


def compute_basis_stats(binance_sec, chainlink_sec):
    """Compute Binance - Chainlink basis statistics."""
    common = sorted(set(binance_sec.keys()) & set(chainlink_sec.keys()))
    if not common:
        return None

    basis = [binance_sec[t] - chainlink_sec[t] for t in common]
    basis = np.array(basis)

    return {
        "count": len(basis),
        "mean": float(np.mean(basis)),
        "median": float(np.median(basis)),
        "std": float(np.std(basis)),
        "min": float(np.min(basis)),
        "max": float(np.max(basis)),
        "pct_positive": float(np.mean(basis > 0) * 100),
        "abs_mean": float(np.mean(np.abs(basis))),
        "p5": float(np.percentile(basis, 5)),
        "p25": float(np.percentile(basis, 25)),
        "p75": float(np.percentile(basis, 75)),
        "p95": float(np.percentile(basis, 95)),
    }


def compute_oracle_update_frequency(chainlink_prices):
    """Analyze how often Chainlink updates and by how much."""
    if len(chainlink_prices) < 10:
        return None

    # Group by source timestamp to find unique oracle updates
    updates = []
    last_price = None
    last_ts = None

    for local_us, src_ts, price in chainlink_prices:
        if last_price is not None and abs(price - last_price) > 0.01:
            gap_sec = (src_ts - last_ts) / 1000.0 if last_ts else 0
            delta = price - last_price
            updates.append({
                "gap_sec": gap_sec,
                "delta_usd": delta,
                "delta_bps": (delta / last_price) * 10000 if last_price > 0 else 0,
            })
        last_price = price
        last_ts = src_ts

    if not updates:
        return None

    gaps = [u["gap_sec"] for u in updates if 0 < u["gap_sec"] < 300]
    deltas = [abs(u["delta_bps"]) for u in updates]

    return {
        "total_updates": len(updates),
        "avg_gap_sec": float(np.mean(gaps)) if gaps else 0,
        "median_gap_sec": float(np.median(gaps)) if gaps else 0,
        "min_gap_sec": float(np.min(gaps)) if gaps else 0,
        "max_gap_sec": float(np.max(gaps)) if gaps else 0,
        "avg_delta_bps": float(np.mean(deltas)),
        "median_delta_bps": float(np.median(deltas)),
        "max_delta_bps": float(np.max(deltas)),
    }


def main():
    data_dir = Path("C:/Users/johng/Downloads/files/BeeBoop/data")
    files = sorted(data_dir.glob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True)

    if not files:
        print("No data files found!")
        return

    path = files[0]
    print(f"Analyzing: {path.name}")
    print(f"File size: {path.stat().st_size / 1024 / 1024:.1f} MB")
    print()

    # Load all events
    print("Loading events (this may take a minute)...")
    data = load_events(path)

    print(f"Total events: {data['total_events']:,}")
    print(f"Binance L1 ticks: {len(data['binance_l1']):,}")
    print(f"Chainlink updates: {len(data['chainlink']):,}")
    print(f"RTDS Binance snapshots: {len(data['rtds_binance']):,}")
    print()

    # Time range
    if data['binance_l1']:
        t_start = data['binance_l1'][0][0] / 1e6
        t_end = data['binance_l1'][-1][0] / 1e6
        duration_min = (t_end - t_start) / 60
        print(f"Recording duration: {duration_min:.1f} minutes")
        print()

    # Resample to 1-second bins
    print("Resampling to 1-second bins...")
    binance_sec = resample_to_seconds(data['binance_l1'], use_src_ts=True)
    chainlink_sec = resample_to_seconds(data['chainlink'], use_src_ts=True)
    rtds_bin_sec = resample_to_seconds(data['rtds_binance'], use_src_ts=True)

    print(f"Binance seconds: {len(binance_sec):,}")
    print(f"Chainlink seconds: {len(chainlink_sec):,}")
    print(f"RTDS Binance seconds: {len(rtds_bin_sec):,}")
    print()

    # === ORACLE UPDATE FREQUENCY ===
    print("=" * 60)
    print("CHAINLINK ORACLE UPDATE FREQUENCY")
    print("=" * 60)
    oracle_stats = compute_oracle_update_frequency(data['chainlink'])
    if oracle_stats:
        print(f"  Total price changes: {oracle_stats['total_updates']}")
        print(f"  Avg gap between updates: {oracle_stats['avg_gap_sec']:.1f}s")
        print(f"  Median gap: {oracle_stats['median_gap_sec']:.1f}s")
        print(f"  Min gap: {oracle_stats['min_gap_sec']:.1f}s")
        print(f"  Max gap: {oracle_stats['max_gap_sec']:.1f}s")
        print(f"  Avg price change: {oracle_stats['avg_delta_bps']:.1f} bps")
        print(f"  Median price change: {oracle_stats['median_delta_bps']:.1f} bps")
        print(f"  Max price change: {oracle_stats['max_delta_bps']:.1f} bps")
    print()

    # === BASIS (Binance - Chainlink) ===
    print("=" * 60)
    print("BASIS: BINANCE - CHAINLINK (USD)")
    print("=" * 60)
    basis = compute_basis_stats(binance_sec, chainlink_sec)
    if basis:
        print(f"  Observations: {basis['count']:,}")
        print(f"  Mean basis: ${basis['mean']:.2f}")
        print(f"  Median basis: ${basis['median']:.2f}")
        print(f"  Std dev: ${basis['std']:.2f}")
        print(f"  Range: ${basis['min']:.2f} to ${basis['max']:.2f}")
        print(f"  Abs mean: ${basis['abs_mean']:.2f}")
        print(f"  5th pct: ${basis['p5']:.2f}")
        print(f"  25th pct: ${basis['p25']:.2f}")
        print(f"  75th pct: ${basis['p75']:.2f}")
        print(f"  95th pct: ${basis['p95']:.2f}")
        print(f"  Binance above Chainlink: {basis['pct_positive']:.1f}% of the time")
    print()

    # === LEAD/LAG CROSS-CORRELATION ===
    print("=" * 60)
    print("LEAD/LAG: BINANCE vs CHAINLINK")
    print("  Positive lag = Binance leads Chainlink")
    print("=" * 60)

    lag_results = compute_lag_correlation(binance_sec, chainlink_sec, max_lag_sec=15)
    if lag_results:
        # Find peak
        peak_lag, peak_corr, peak_n = max(lag_results, key=lambda x: abs(x[1]))
        print(f"\n  Peak correlation: {peak_corr:.4f} at lag {peak_lag}s (n={peak_n})")
        print()
        print(f"  {'Lag':>6s}  {'Corr':>8s}  {'N':>6s}  Bar")
        print(f"  {'---':>6s}  {'---':>8s}  {'---':>6s}  ---")
        for lag, corr, n in lag_results:
            if -10 <= lag <= 10:
                bar_len = int(abs(corr) * 50)
                bar = "+" * bar_len if corr > 0 else "-" * bar_len
                marker = " <-- PEAK" if lag == peak_lag else ""
                print(f"  {lag:>+4d}s  {corr:>+8.4f}  {n:>6d}  {bar}{marker}")
    print()

    # === LEAD/LAG: RTDS Binance vs Chainlink ===
    print("=" * 60)
    print("LEAD/LAG: RTDS BINANCE vs CHAINLINK")
    print("=" * 60)

    lag_results2 = compute_lag_correlation(rtds_bin_sec, chainlink_sec, max_lag_sec=15)
    if lag_results2:
        peak_lag2, peak_corr2, peak_n2 = max(lag_results2, key=lambda x: abs(x[1]))
        print(f"\n  Peak correlation: {peak_corr2:.4f} at lag {peak_lag2}s (n={peak_n2})")
        print()
        print(f"  {'Lag':>6s}  {'Corr':>8s}  {'N':>6s}  Bar")
        print(f"  {'---':>6s}  {'---':>8s}  {'---':>6s}  ---")
        for lag, corr, n in lag_results2:
            if -10 <= lag <= 10:
                bar_len = int(abs(corr) * 50)
                bar = "+" * bar_len if corr > 0 else "-" * bar_len
                marker = " <-- PEAK" if lag == peak_lag2 else ""
                print(f"  {lag:>+4d}s  {corr:>+8.4f}  {n:>6d}  {bar}{marker}")
    print()

    # === DIRECT BINANCE L1 vs RTDS Binance ===
    print("=" * 60)
    print("LEAD/LAG: DIRECT BINANCE L1 vs RTDS BINANCE")
    print("  (measures RTDS relay delay)")
    print("=" * 60)

    lag_results3 = compute_lag_correlation(binance_sec, rtds_bin_sec, max_lag_sec=10)
    if lag_results3:
        peak_lag3, peak_corr3, peak_n3 = max(lag_results3, key=lambda x: abs(x[1]))
        print(f"\n  Peak correlation: {peak_corr3:.4f} at lag {peak_lag3}s (n={peak_n3})")
        print()
        for lag, corr, n in lag_results3:
            if -5 <= lag <= 5:
                bar_len = int(abs(corr) * 50)
                bar = "+" * bar_len if corr > 0 else "-" * bar_len
                marker = " <-- PEAK" if lag == peak_lag3 else ""
                print(f"  {lag:>+4d}s  {corr:>+8.4f}  {n:>6d}  {bar}{marker}")
    print()

    print("=" * 60)
    print("SUMMARY")
    print("=" * 60)
    if lag_results:
        peak_lag, peak_corr, _ = max(lag_results, key=lambda x: abs(x[1]))
        if peak_lag > 0:
            print(f"  Binance LEADS Chainlink by ~{peak_lag}s (corr={peak_corr:.4f})")
            print(f"  This is the exploitable oracle lag window.")
        elif peak_lag < 0:
            print(f"  Chainlink LEADS Binance by ~{abs(peak_lag)}s (corr={peak_corr:.4f})")
            print(f"  Unexpected! Oracle appears to lead exchange.")
        else:
            print(f"  No significant lead/lag detected at 1s resolution.")

    if basis:
        print(f"  Average basis: ${basis['abs_mean']:.2f} ({basis['abs_mean']/70000*10000:.1f} bps)")
        print(f"  This is the average price divergence you can trade against.")


if __name__ == "__main__":
    main()
