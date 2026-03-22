"""
mint_window.py — Mint tokens for a specific window.
Called by the Rust bot at window start.

Usage: python3 mint_window.py <condition_id> <amount_usdc>
Example: python3 mint_window.py 0xabc123... 100

Mints <amount_usdc> worth of UP + DOWN tokens via splitPosition.
Prints JSON result to stdout for the Rust bot to parse.
"""

import os, sys, json, time
from dotenv import load_dotenv
from web3 import Web3
import requests

load_dotenv()

USDC_E = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
CTF = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")

POLYGON_RPCS = [
    "https://polygon.llamarpc.com",
    "https://rpc.ankr.com/polygon",
    "https://polygon-bor-rpc.publicnode.com",
    "https://polygon-rpc.com",
]

CTF_ABI = json.loads("""[
  {"inputs":[
    {"name":"collateralToken","type":"address"},
    {"name":"parentCollectionId","type":"bytes32"},
    {"name":"conditionId","type":"bytes32"},
    {"name":"partition","type":"uint256[]"},
    {"name":"amount","type":"uint256"}
  ],"name":"splitPosition","outputs":[],"stateMutability":"nonpayable","type":"function"},
  {"inputs":[{"name":"owner","type":"address"},{"name":"id","type":"uint256"}],
   "name":"balanceOf","outputs":[{"name":"","type":"uint256"}],
   "stateMutability":"view","type":"function"}
]""")

def connect_web3():
    session = requests.Session()
    session.headers.update({"Content-Type": "application/json"})
    for rpc in POLYGON_RPCS:
        try:
            w3 = Web3(Web3.HTTPProvider(rpc, session=session, request_kwargs={"timeout": 10}))
            if w3.is_connected():
                return w3
        except:
            continue
    return None

def main():
    if len(sys.argv) < 3:
        print(json.dumps({"success": False, "error": "usage: mint_window.py <condition_id> <amount_usdc>"}))
        sys.exit(1)

    condition_id = sys.argv[1]
    amount_usdc = float(sys.argv[2])
    amount_raw = int(amount_usdc * 1e6)

    pk = os.getenv("POLY_PRIVATE_KEY")
    if not pk:
        print(json.dumps({"success": False, "error": "no POLY_PRIVATE_KEY"}))
        sys.exit(1)
    if not pk.startswith("0x"):
        pk = "0x" + pk

    w3 = connect_web3()
    if not w3:
        print(json.dumps({"success": False, "error": "cannot connect to Polygon RPC"}))
        sys.exit(1)

    account = w3.eth.account.from_key(pk)
    address = account.address
    ctf = w3.eth.contract(address=CTF, abi=CTF_ABI)

    try:
        t0 = time.time()
        nonce = w3.eth.get_transaction_count(address)
        gas_price = w3.eth.gas_price

        tx = ctf.functions.splitPosition(
            USDC_E,
            b'\x00' * 32,
            Web3.to_bytes(hexstr=condition_id),
            [1, 2],
            amount_raw,
        ).build_transaction({
            "from": address,
            "nonce": nonce,
            "chainId": 137,
            "maxFeePerGas": gas_price * 2,
            "maxPriorityFeePerGas": w3.to_wei(50, "gwei"),
        })

        tx["gas"] = w3.eth.estimate_gas(tx)
        signed = w3.eth.account.sign_transaction(tx, pk)
        tx_hash = w3.eth.send_raw_transaction(signed.raw_transaction)

        receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=30)
        elapsed_ms = int((time.time() - t0) * 1000)

        if receipt.status == 1:
            print(json.dumps({
                "success": True,
                "tx_hash": tx_hash.hex(),
                "block": receipt.blockNumber,
                "gas_used": receipt.gasUsed,
                "elapsed_ms": elapsed_ms,
                "amount_usdc": amount_usdc,
            }))
        else:
            print(json.dumps({"success": False, "error": "tx reverted", "tx_hash": tx_hash.hex()}))
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))

if __name__ == "__main__":
    main()
