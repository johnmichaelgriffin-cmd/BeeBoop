"""Quick data check on the JSONL recording."""
import json, os, sys
from collections import Counter

datadir = os.path.join(os.path.dirname(__file__), "data")
files = sorted([f for f in os.listdir(datadir) if f.endswith(".jsonl")])

for fname in files:
    path = os.path.join(datadir, fname)
    size_mb = os.path.getsize(path) / 1_000_000

    sources = Counter()
    first_ts = None
    last_ts = None
    count = 0

    with open(path, "r") as f:
        for line in f:
            count += 1
            try:
                evt = json.loads(line)
                src = evt.get("source", "unknown")
                sources[src] += 1
                ts = evt.get("recv_ts") or evt.get("timestamp") or evt.get("ts")
                if ts:
                    if first_ts is None:
                        first_ts = ts
                    last_ts = ts
            except:
                pass

    print(f"\n{'='*60}")
    print(f"File: {fname}")
    print(f"Size: {size_mb:.1f} MB | Lines: {count:,}")
    if first_ts and last_ts:
        print(f"Time range: {first_ts} → {last_ts}")
    print(f"Events by source:")
    for src, cnt in sources.most_common():
        print(f"  {src}: {cnt:,}")

# Sample last 5 events from the latest file
if files:
    path = os.path.join(datadir, files[-1])
    print(f"\n{'='*60}")
    print(f"Last 5 events from {files[-1]}:")
    lines = []
    with open(path, "r") as f:
        for line in f:
            lines.append(line)
            if len(lines) > 5:
                lines.pop(0)
    for line in lines:
        try:
            evt = json.loads(line)
            # Truncate large fields
            preview = json.dumps(evt, indent=None)
            if len(preview) > 200:
                preview = preview[:200] + "..."
            print(f"  {preview}")
        except:
            print(f"  {line.strip()[:200]}")
