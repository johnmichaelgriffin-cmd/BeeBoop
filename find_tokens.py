"""Find current BTC 5-min market token IDs from Gamma API."""
import requests, json, time

# The slug format is: btc-updown-5m-{window_timestamp}
# Window timestamp = floor(now / 300) * 300
now = int(time.time())
wts = now - (now % 300)

slug = f"btc-updown-5m-{wts}"
print(f"Looking for slug: {slug}")
print(f"Window timestamp: {wts}")

r = requests.get(f"https://gamma-api.polymarket.com/events?slug={slug}", timeout=10)
events = r.json()
print(f"Events found: {len(events)}")

if events:
    m = events[0]["markets"][0]
    tokens = m.get("clobTokenIds", "")
    outcomes = m.get("outcomes", "")
    if isinstance(tokens, str):
        tokens = json.loads(tokens)
    if isinstance(outcomes, str):
        outcomes = json.loads(outcomes)
    print(f"Outcomes: {outcomes}")
    print(f"Token IDs:")
    for o, t in zip(outcomes, tokens):
        print(f"  {o}: {t}")

# Also check next window
wts_next = wts + 300
slug_next = f"btc-updown-5m-{wts_next}"
print(f"\nNext window slug: {slug_next}")
r2 = requests.get(f"https://gamma-api.polymarket.com/events?slug={slug_next}", timeout=10)
events2 = r2.json()
print(f"Next window events found: {len(events2)}")
if events2:
    m2 = events2[0]["markets"][0]
    tokens2 = m2.get("clobTokenIds", "")
    if isinstance(tokens2, str):
        tokens2 = json.loads(tokens2)
    print(f"Token IDs: {tokens2}")
