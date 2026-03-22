#!/usr/bin/env python3
"""
sniper_sweeper.py - Nuke-Method Sweeper
=========================================
Uses the same approach as nuke_sweep.py but runs on a loop.
Pulls recent trades from Data API, checks token balances on-chain, redeems.
No Gamma API dependency. No slug construction. Just raw trades + balances.

python sniper_sweeper.py
python sniper_sweeper.py --lookback 10

ENV VARS: POLY_PRIVATE_KEY
"""

import os, sys, time, json, logging, argparse
from datetime import datetime, timezone
from collections import defaultdict
import requests
from dotenv import load_dotenv
from web3 import Web3

load_dotenv()

DATA_API = "https://data-api.polymarket.com"
USDC_E = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
CTF    = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")
WINDOW_SECONDS = 300

POLYGON_RPCS = [
    "https://polygon.llamarpc.com",
    "https://rpc.ankr.com/polygon",
    "https://polygon-bor-rpc.publicnode.com",
    "https://polygon-rpc.com",
]

CTF_ABI = json.loads('[{"inputs":[{"name":"collateralToken","type":"address"},{"name":"parentCollectionId","type":"bytes32"},{"name":"conditionId","type":"bytes32"},{"name":"indexSets","type":"uint256[]"}],"name":"redeemPositions","outputs":[],"stateMutability":"nonpayable","type":"function"},{"inputs":[{"name":"owner","type":"address"},{"name":"id","type":"uint256"}],"name":"balanceOf","outputs":[{"name":"","type":"uint256"}],"stateMutability":"view","type":"function"}]')
USDC_ABI = json.loads('[{"inputs":[{"name":"account","type":"address"}],"name":"balanceOf","outputs":[{"name":"","type":"uint256"}],"stateMutability":"view","type":"function"}]')

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s", datefmt="%H:%M:%S")
log = logging.getLogger("sweeper")

SESSION = requests.Session()
SESSION.headers.update({"Connection": "keep-alive"})

_rpc_idx = 0

def connect_web3():
    """Try all RPCs, rotating from last known good. Never exits — retries forever."""
    global _rpc_idx
    for attempt in range(3):  # 3 full rotations through all RPCs
        for i in range(len(POLYGON_RPCS)):
            idx = (_rpc_idx + i) % len(POLYGON_RPCS)
            try:
                session = requests.Session()
                w3 = Web3(Web3.HTTPProvider(POLYGON_RPCS[idx], session=session, request_kwargs={"timeout": 10}))
                if w3.is_connected():
                    _rpc_idx = idx
                    log.info(f"Connected via {POLYGON_RPCS[idx]}")
                    return w3
            except:
                continue
        log.warning(f"All RPCs failed (rotation {attempt+1}/3), waiting 10s...")
        time.sleep(10)
    log.error("Cannot connect after all retries")
    return None

def get_fresh_nonce(w3, address):
    """Always fetch a fresh nonce right before sending."""
    return w3.eth.get_transaction_count(address, 'pending')

def pull_recent_trades(wallet, lookback_seconds):
    cutoff = time.time() - lookback_seconds
    all_trades = []
    offset = 0
    batch = 100
    while True:
        try:
            resp = SESSION.get(f"{DATA_API}/activity", params={
                "user": wallet.lower(), "type": "TRADE", "limit": batch,
                "sortBy": "TIMESTAMP", "sortDirection": "DESC", "offset": offset,
            }, timeout=10)
            if resp.status_code != 200: break
            data = resp.json()
            if not data: break
            hit_cutoff = False
            for t in data:
                ts = t.get("timestamp", 0)
                if isinstance(ts, (int, float)) and ts < cutoff:
                    hit_cutoff = True; break
                all_trades.append(t)
            if hit_cutoff or len(data) < batch: break
            offset += batch
            time.sleep(0.2)
        except: break
    return all_trades

def extract_tokens(trades):
    tokens = {}
    for t in trades:
        asset = t.get("asset", "")
        cid = t.get("conditionId", "")
        if asset and cid:
            tokens[asset] = cid
    return tokens

def is_redeemable(ctf_contract, w3, address, cid):
    """Dry-run redeemPositions via estimate_gas. If it reverts, not ready."""
    try:
        tx = ctf_contract.functions.redeemPositions(
            USDC_E, b'\x00' * 32,
            Web3.to_bytes(hexstr=cid), [1, 2],
        ).build_transaction({
            "from": address,
            "nonce": 0,  # dummy nonce for estimate
            "chainId": 137,
            "maxFeePerGas": w3.to_wei(100, "gwei"),
            "maxPriorityFeePerGas": w3.to_wei(50, "gwei"),
        })
        w3.eth.estimate_gas(tx)
        return True
    except Exception as e:
        msg = str(e).lower()
        if "result for condition not received yet" in msg:
            return False
        if "execution reverted" in msg:
            return False
        # Network error — assume redeemable, let the real tx handle it
        return True

def main():
    parser = argparse.ArgumentParser(description="Nuke-Method Sweeper")
    parser.add_argument("--lookback", type=int, default=5,
                        help="Windows to look back (default: 5 = 25 min)")
    args = parser.parse_args()

    pk = os.getenv("POLY_PRIVATE_KEY")
    if not pk: log.error("Set POLY_PRIVATE_KEY"); sys.exit(1)
    if not pk.startswith("0x"): pk = "0x" + pk

    w3 = connect_web3()
    if not w3:
        sys.exit(1)
    account = w3.eth.account.from_key(pk)
    ADDRESS = Web3.to_checksum_address(account.address)
    ctf_contract = w3.eth.contract(address=CTF, abi=CTF_ABI)
    usdc_contract = w3.eth.contract(address=USDC_E, abi=USDC_ABI)

    lookback_seconds = args.lookback * WINDOW_SECONDS
    sweep_count = 0
    total_recovered = 0

    start_bal = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6

    log.info("=" * 60)
    log.info("SWEEPER (nuke method)")
    log.info(f"   Wallet:    {ADDRESS}")
    log.info(f"   USDC.e:    ${start_bal:.2f}")
    log.info(f"   Lookback:  {args.lookback} windows ({lookback_seconds}s)")
    log.info(f"   Cycle:     every 5 min at T+240s")
    log.info("=" * 60)

    last_swept_window = None

    try:
        while True:
            now = int(time.time())
            current_window = now - (now % WINDOW_SECONDS)
            target_time = current_window + 240

            wait = target_time - time.time()
            if wait > 0:
                log.info(f"Waiting {wait:.0f}s until T+240s...")
                time.sleep(wait)

            # Re-check window after sleeping — it may have advanced
            now = int(time.time())
            current_window = now - (now % WINDOW_SECONDS)

            if current_window == last_swept_window:
                time.sleep(1)
                continue
            last_swept_window = current_window

            sweep_count += 1

            # Step 1: Pull recent trades
            trades = pull_recent_trades(ADDRESS, lookback_seconds)
            if not trades:
                log.info(f"Sweep #{sweep_count}: no trades found")
                continue

            # Step 2: Extract tokens
            tokens = extract_tokens(trades)

            # Step 3: Check balances — reconnect if RPC is stale
            try:
                bal_before = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
            except:
                w3 = connect_web3()
                if not w3:
                    log.error("All RPCs down, sleeping 60s...")
                    time.sleep(60)
                    continue
                ctf_contract = w3.eth.contract(address=CTF, abi=CTF_ABI)
                usdc_contract = w3.eth.contract(address=USDC_E, abi=USDC_ABI)
                try:
                    bal_before = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
                except:
                    log.error("Still can't read balance, skipping sweep")
                    continue

            conditions_to_redeem = defaultdict(list)
            for token_id, cid in tokens.items():
                try:
                    bal = ctf_contract.functions.balanceOf(ADDRESS, int(token_id)).call() / 1e6
                    if bal > 0.01:
                        conditions_to_redeem[cid].append((token_id, bal))
                except: pass

            if not conditions_to_redeem:
                try:
                    bal_now = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
                except:
                    bal_now = bal_before
                log.info(f"Sweep #{sweep_count}: {len(trades)} trades, "
                         f"{len(tokens)} tokens checked, nothing | Bal: ${bal_now:.2f}")
                continue

            # Step 4: Dry-run check which conditions are actually redeemable
            ready = {}
            skipped = 0
            for cid, token_list in conditions_to_redeem.items():
                if is_redeemable(ctf_contract, w3, ADDRESS, cid):
                    ready[cid] = token_list
                else:
                    skipped += 1
                    shares = sum(b for _, b in token_list)
                    log.info(f"   Skipping {cid[:16]}... ({shares:.0f}sh) — not resolved yet")

            if skipped > 0:
                log.info(f"   {skipped} conditions not resolved, {len(ready)} ready")

            if not ready:
                continue

            # Step 5: Redeem only the ready ones
            redeemed_count = 0
            total_tokens = 0

            for cid, token_list in ready.items():
                shares = sum(b for _, b in token_list)
                total_tokens += shares

                for attempt in range(3):
                    try:
                        if attempt > 0:
                            w3 = connect_web3()
                            if not w3:
                                log.error("Cannot reconnect, skipping condition")
                                break
                            ctf_contract = w3.eth.contract(address=CTF, abi=CTF_ABI)
                            usdc_contract = w3.eth.contract(address=USDC_E, abi=USDC_ABI)

                        # Fresh nonce every attempt to avoid nonce collisions
                        nonce = get_fresh_nonce(w3, ADDRESS)
                        tx_params = {
                            "from": ADDRESS,
                            "nonce": nonce,
                            "chainId": 137,
                            "maxFeePerGas": w3.eth.gas_price * 2,
                            "maxPriorityFeePerGas": w3.to_wei(50, "gwei"),
                        }
                        tx = ctf_contract.functions.redeemPositions(
                            USDC_E, b'\x00' * 32,
                            Web3.to_bytes(hexstr=cid), [1, 2],
                        ).build_transaction(tx_params)
                        tx["gas"] = w3.eth.estimate_gas(tx)
                        signed = w3.eth.account.sign_transaction(tx, pk)
                        tx_hash = w3.eth.send_raw_transaction(signed.raw_transaction)
                        receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=30)

                        if receipt.status == 1:
                            redeemed_count += 1
                            log.info(f"   Redeemed {shares:.0f} tokens ({cid[:16]}...)")
                            break
                        else:
                            log.warning(f"   Reverted {cid[:16]}... (attempt {attempt+1}/3)")
                    except Exception as e:
                        msg = str(e)
                        # If condition not resolved, don't retry — skip it
                        if "result for condition not received yet" in msg.lower():
                            log.info(f"   {cid[:16]}... not resolved yet, skipping")
                            break
                        # Nonce errors: wait a beat then retry with fresh nonce
                        if any(x in msg.lower() for x in ["nonce too low", "replacement transaction", "gapped-nonce"]):
                            log.warning(f"   Nonce issue ({cid[:16]}...), refreshing: {msg[:60]}")
                            time.sleep(3)
                            continue
                        log.warning(f"   Error {cid[:16]}... attempt {attempt+1}/3: {msg[:80]}")
                    time.sleep(3)

                time.sleep(2)  # nonce spacing between conditions

            try:
                bal_after = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
            except:
                w3 = connect_web3()
                if w3:
                    usdc_contract = w3.eth.contract(address=USDC_E, abi=USDC_ABI)
                    try:
                        bal_after = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
                    except:
                        bal_after = bal_before
                else:
                    bal_after = bal_before

            recovered = bal_after - bal_before
            total_recovered += max(0, recovered)

            log.info(f"Sweep #{sweep_count}: {total_tokens:.0f} tokens, "
                     f"{redeemed_count} redeemed, ${recovered:+.2f} | "
                     f"Bal: ${bal_after:.2f} | Session: ${bal_after - start_bal:+.2f}")

    except KeyboardInterrupt:
        try:
            final_bal = usdc_contract.functions.balanceOf(ADDRESS).call() / 1e6
        except: final_bal = start_bal
        log.info(f"\nStopped. Recovered: ${total_recovered:+.2f} | "
                 f"Bal: ${final_bal:.2f} | Session: ${final_bal - start_bal:+.2f}")

if __name__ == "__main__":
    main()
