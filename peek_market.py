"""Check market_ws events in latest JSONL."""
import json, os
from collections import Counter

datadir = os.path.join(os.path.dirname(__file__), "data")
latest = sorted([f for f in os.listdir(datadir) if f.endswith(".jsonl")])[-1]
path = os.path.join(datadir, latest)
print(f"File: {latest}")

sources = Counter()
market_types = Counter()
market_samples = []
count = 0

with open(path) as f:
    for line in f:
        count += 1
        try:
            evt = json.loads(line)
            src = evt.get("source", "?")
            sources[src] += 1
            if src == "market_ws":
                # Parse the inner raw_json to see event types
                raw = json.loads(evt.get("raw_json", "[]"))
                if isinstance(raw, list):
                    for inner in raw:
                        et = inner.get("event_type", "?")
                        market_types[et] += 1
                elif isinstance(raw, dict):
                    et = raw.get("event_type", "?")
                    market_types[et] += 1
                if len(market_samples) < 3:
                    preview = evt.get("raw_json", "")[:300]
                    market_samples.append(preview)
        except:
            pass

print(f"Total events: {count:,}")
print(f"\nBy source:")
for s, c in sources.most_common():
    print(f"  {s}: {c:,}")

print(f"\nMarket WS event types:")
for t, c in market_types.most_common():
    print(f"  {t}: {c:,}")

print(f"\nSample market_ws events:")
for s in market_samples:
    print(f"  {s}...")
