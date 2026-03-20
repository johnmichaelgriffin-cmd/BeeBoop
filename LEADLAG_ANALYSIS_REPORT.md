# BeeBoop Lead/Lag Analysis Report
## Data Collection: March 19, 2026

---

## 1. Data Collection Setup

**Recording Location:** Windows desktop (residential internet, US East Coast)
**Secondary Recording:** AWS Lightsail Dublin (eu-west-1) — running but not yet analyzed
**Duration:** 65.6 minutes of continuous recording
**Total Events Recorded:** 3,395,992

### Event Breakdown by Source
| Source | Event Type | Count | Notes |
|--------|-----------|-------|-------|
| Binance L2 | bookTicker (L1 best bid/ask) | 1,790,922 | ~455 updates/sec avg |
| Binance L2 | depthUpdate (10-level book) | ~500,000 est | Every 100ms |
| RTDS | Chainlink btc/usd | 3,880 | ~1 per second |
| RTDS | Binance btcusdt | 2 | Only initial subscribe snapshots |

### Feed Details
- **Binance L2 WebSocket:** `wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/btcusdt@depth10@100ms`
  - bookTicker: real-time, no throttle, includes bid/ask/qty and transaction timestamp (ms)
  - depth10@100ms: 10-level order book snapshot every 100ms
- **Polymarket RTDS:** `wss://ws-live-data.polymarket.com`
  - Subscribed to `crypto_prices_chainlink` (btc/usd) and `crypto_prices` (btcusdt)
  - Chainlink updates arrived ~1/sec with source timestamps
  - RTDS Binance relay only delivered 2 events (initial subscription snapshots with 120-second history arrays) — not useful for real-time analysis

---

## 2. Chainlink Oracle Update Frequency

| Metric | Value |
|--------|-------|
| Total price changes detected | 3,554 |
| Average gap between updates | 1.0 seconds |
| Median gap | 1.0 seconds |
| Minimum gap | 1.0 seconds |
| Maximum gap | 9.0 seconds |
| Average price change per update | 0.3 bps |
| Median price change per update | 0.1 bps |
| Maximum single price change | 5.9 bps |

### Interpretation
Chainlink's BTC/USD feed via RTDS updates approximately once per second. This is consistent with Chainlink Data Streams (pull-based, sub-second) rather than the on-chain push feed (60-second heartbeat). Despite the high update frequency, the oracle still exhibits significant lag relative to Binance — the updates arrive with stale prices, not real-time prices.

---

## 3. Basis Analysis: Binance - Chainlink

The "basis" is defined as `Binance_mid_price - Chainlink_price` at each overlapping second.

| Metric | Value |
|--------|-------|
| Overlapping observations | 3,880 seconds |
| Mean basis | -$29.25 |
| Median basis | -$29.25 |
| Standard deviation | $7.86 |
| Minimum (most negative) | -$77.62 |
| Maximum (most positive) | +$26.87 |
| Mean absolute basis | $29.30 |
| 5th percentile | -$42.30 |
| 25th percentile | -$33.07 |
| 75th percentile | -$25.41 |
| 95th percentile | -$17.00 |
| Binance above Chainlink | 0.3% of the time |

### Interpretation
During this recording window, Chainlink was consistently reporting prices ~$29 higher than Binance. This persistent offset means:
1. Chainlink's price source diverges from Binance perpetual futures mid-price (expected — different underlying, spot vs perps, different aggregation)
2. The absolute level difference is less important than the **dynamics** — when does the basis widen/narrow, and does a change in Binance predict a subsequent change in Chainlink?

---

## 4. Lead/Lag Cross-Correlation Analysis

### Methodology
1. Both Binance L1 mid-price and Chainlink price were resampled to 1-second bins (last price per second)
2. Returns were computed as `(price_t - price_{t-1}) / price_{t-1}` for consecutive seconds only
3. Pearson correlation was computed between Binance returns at time `t` and Chainlink returns at time `t + lag`
4. Positive lag means Binance leads (Binance moves first, Chainlink follows)

### Results: Binance L1 vs Chainlink

| Lag | Correlation | N (pairs) | Interpretation |
|-----|------------|-----------|---------------|
| -10s | -0.0170 | 3,833 | No relationship |
| -9s | +0.0120 | 3,834 | No relationship |
| -8s | +0.0099 | 3,835 | No relationship |
| -7s | +0.0277 | 3,836 | No relationship |
| -6s | +0.0294 | 3,837 | No relationship |
| -5s | -0.0083 | 3,838 | No relationship |
| -4s | -0.0027 | 3,839 | No relationship |
| -3s | -0.0349 | 3,840 | No relationship |
| -2s | -0.0150 | 3,841 | No relationship |
| -1s | -0.0011 | 3,842 | No relationship |
| **0s** | **+0.0384** | **3,843** | **Weak contemporaneous** |
| **+1s** | **+0.3238** | **3,842** | **Strong — Binance leads by 1s** |
| **+2s** | **+0.7973** | **3,841** | **Very strong — peak lag** |
| **+3s** | **+0.2448** | **3,840** | **Moderate — trailing effect** |
| +4s | +0.0698 | 3,839 | Weak residual |
| +5s | +0.0322 | 3,838 | Negligible |
| +6s | -0.0008 | 3,837 | No relationship |
| +7s | +0.0110 | 3,836 | No relationship |
| +8s | -0.0068 | 3,835 | No relationship |
| +9s | +0.0093 | 3,834 | No relationship |
| +10s | +0.0148 | 3,833 | No relationship |

### Visual Representation (ASCII)
```
Lag  Correlation
-10s  |
 -9s  |
 -8s  |
 -7s  |
 -6s  |
 -5s  |
 -4s  |
 -3s  |
 -2s  |
 -1s  |
  0s  |++
 +1s  |++++++++++++++++
 +2s  |+++++++++++++++++++++++++++++++++++++++++  <-- PEAK (0.80)
 +3s  |++++++++++++
 +4s  |+++
 +5s  |+
 +6s  |
 +7s  |
 +8s  |
 +9s  |
+10s  |
```

### Key Finding
**Binance leads Chainlink by exactly 2 seconds with a correlation of 0.7973.** This is an extremely strong and clean signal:
- At lag 0 (contemporaneous): correlation is only 0.04 — the two are essentially uncorrelated at the same instant
- At lag +1s: correlation jumps to 0.32 — Binance's move 1 second ago already predicts Chainlink's current move
- At lag +2s: correlation peaks at 0.80 — this is the primary transmission channel
- At lag +3s: correlation drops to 0.24 — residual effect
- Beyond +3s: no significant correlation

This means: **when Binance moves, you have a 1-3 second window before Chainlink reflects that move.** The strongest signal is at the 2-second mark.

---

## 5. Missing Data / Limitations

### RTDS Binance Relay
The RTDS `crypto_prices` (Binance source) only delivered 2 events — both were initial subscription responses containing 120-second historical price arrays. It did not stream real-time updates the way `crypto_prices_chainlink` did. This means:
- We could not compute RTDS Binance vs Chainlink lead/lag
- We could not compute direct Binance L1 vs RTDS Binance relay delay
- **Action needed:** Investigate whether RTDS Binance requires a different subscription format, or if it only provides historical snapshots on subscribe

### Single Location
This data was recorded from a US residential connection. The Dublin VPS recording is running but not yet analyzed. Comparing both will reveal:
- Whether the lead/lag is consistent across locations
- How much local network latency affects the measured lag
- Whether Dublin (closer to Polymarket CLOB) sees the signals sooner

### Short Duration
65 minutes of data across a relatively stable BTC period (BTC ranged ~$69,500-$69,600). We should validate across:
- High volatility periods (major price swings)
- Different times of day (US market hours vs Asia hours)
- Multiple days

### No Polymarket Orderbook Data Yet
The market WebSocket channel for Polymarket token orderbooks is implemented but was not included in this analysis. The critical next step is correlating:
- Binance move → Chainlink update → Polymarket token price change
- This three-way lag tells us the full pipeline delay

---

## 6. Implications for Trading Strategy

### The Exploitable Window
If Polymarket's 5-minute BTC Up/Down markets resolve using Chainlink Data Streams prices, and Chainlink lags Binance by ~2 seconds:

1. **At T+0s:** Binance price moves (e.g., drops $50)
2. **At T+0.5s:** Our bot detects the move via direct Binance WebSocket
3. **At T+1s:** Chainlink begins reflecting the move (corr=0.32)
4. **At T+2s:** Chainlink fully reflects the move (corr=0.80)
5. **At T+2-3s:** Other bots watching Chainlink/RTDS react
6. **At T+2-5s:** Polymarket token orderbook adjusts

**Our theoretical edge:** We see the move at T+0.5s, post orders by T+1s, and fill before the orderbook adjusts at T+2-5s.

### Strategy Implications
- **Taker, not maker:** When the signal fires (large Binance move), we should aggressively take available liquidity (FOK at best ask), not passively post GTCs
- **Direction prediction:** If Binance drops, buy DOWN tokens; if Binance rises, buy UP tokens
- **Signal threshold:** Not every 0.3 bps Binance tick is tradeable — we need a minimum move size that reliably predicts Chainlink's next update direction
- **Position sizing:** The basis data shows divergences of $30-80, suggesting the signal is meaningful even after execution costs

### Open Questions for Strategy Design
1. What is the minimum Binance move (in bps) that reliably predicts the Chainlink direction 2 seconds later?
2. Does the 2-second lag hold during high volatility, or does it compress?
3. How fast can we actually execute on Polymarket's CLOB via REST? If order-to-ack is >2s, the edge evaporates.
4. What is the Polymarket token orderbook's reaction time to Chainlink updates? (Need market WS data to answer)
5. Is the signal strong enough to overcome Polymarket's bid-ask spread + fees?

---

## 7. Next Steps

1. **Analyze Dublin VPS data** — compare lead/lag from closer to Polymarket's servers
2. **Fix RTDS Binance subscription** — need real-time streaming, not just historical snapshots
3. **Add Polymarket market WebSocket analysis** — measure token orderbook reaction to Chainlink updates
4. **Extend recording to 24+ hours** — validate across different market conditions
5. **Compute signal profitability** — simulate: "if we bought the predicted side at current ask every time Binance moved >X bps, what would our P&L be after spreads and fees?"
6. **Measure execution latency** — benchmark REST order placement from Dublin VPS to Polymarket CLOB

---

## Raw Analysis Output

```
Analyzing: events_20260319_174413.jsonl
File size: 2036.3 MB

Total events: 3,395,992
Binance L1 ticks: 1,790,922
Chainlink updates: 3,880
RTDS Binance snapshots: 2

Recording duration: 65.6 minutes

Binance seconds: 3,938
Chainlink seconds: 3,880
RTDS Binance seconds: 2

CHAINLINK ORACLE UPDATE FREQUENCY
  Total price changes: 3,554
  Avg gap between updates: 1.0s
  Median gap: 1.0s
  Min/Max gap: 1.0s / 9.0s
  Avg/Median/Max price change: 0.3 / 0.1 / 5.9 bps

BASIS: BINANCE - CHAINLINK (USD)
  Mean: -$29.25 | Median: -$29.25 | Std: $7.86
  Range: -$77.62 to +$26.87
  Abs mean: $29.30 (4.2 bps at $70k BTC)

LEAD/LAG PEAK: Binance leads Chainlink by 2 seconds (corr=0.7973)
```
