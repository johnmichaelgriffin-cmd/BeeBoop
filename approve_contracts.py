"""
One-time approval script for mint+sell strategy.

Sets three approvals:
  1. USDC.e → CTF contract  (so splitPosition can pull your USDC)
  2. CTF tokens → CTF Exchange  (so CLOB can move your conditional tokens)
  3. USDC.e → CTF Exchange  (for any CLOB trading)

Requirements:
  pip install web3 python-dotenv

Usage:
  1. Make sure .env has POLY_PRIVATE_KEY (MetaMask private key)
  2. python approve_contracts.py
"""

import os, json, sys
from dotenv import load_dotenv
from web3 import Web3

load_dotenv()

# ─── Config ──────────────────────────────────────────────
POLYGON_RPCS = [
    "https://polygon.llamarpc.com",
    "https://rpc.ankr.com/polygon",
    "https://polygon-bor-rpc.publicnode.com",
    "https://polygon-rpc.com",
]
CHAIN_ID = 137

USDC_E              = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
CTF                 = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")
CTF_EXCHANGE        = Web3.to_checksum_address("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E")
NEG_RISK_EXCHANGE   = Web3.to_checksum_address("0xC5d563A36AE78145C45a50134d48A1215220f80a")

MAX_UINT256 = 2**256 - 1

# Minimal ERC20 ABI (approve + allowance)
ERC20_ABI = json.loads("""[
  {"inputs":[{"name":"spender","type":"address"},{"name":"amount","type":"uint256"}],
   "name":"approve","outputs":[{"name":"","type":"bool"}],
   "stateMutability":"nonpayable","type":"function"},
  {"inputs":[{"name":"owner","type":"address"},{"name":"spender","type":"address"}],
   "name":"allowance","outputs":[{"name":"","type":"uint256"}],
   "stateMutability":"view","type":"function"},
  {"inputs":[{"name":"account","type":"address"}],
   "name":"balanceOf","outputs":[{"name":"","type":"uint256"}],
   "stateMutability":"view","type":"function"}
]""")

# CTF is ERC1155 — uses setApprovalForAll (not approve)
ERC1155_ABI = json.loads("""[
  {"inputs":[{"name":"operator","type":"address"},{"name":"approved","type":"bool"}],
   "name":"setApprovalForAll","outputs":[],
   "stateMutability":"nonpayable","type":"function"},
  {"inputs":[{"name":"account","type":"address"},{"name":"operator","type":"address"}],
   "name":"isApprovedForAll","outputs":[{"name":"","type":"bool"}],
   "stateMutability":"view","type":"function"}
]""")

# ─── Setup ───────────────────────────────────────────────
pk = os.getenv("POLY_PRIVATE_KEY")
if not pk:
    print("❌ Set POLY_PRIVATE_KEY in your .env")
    sys.exit(1)

if not pk.startswith("0x"):
    pk = "0x" + pk

w3 = None
for rpc in POLYGON_RPCS:
    try:
        _w3 = Web3(Web3.HTTPProvider(rpc, request_kwargs={"timeout": 10}))
        if _w3.is_connected():
            w3 = _w3
            print(f"🌐 Connected via {rpc}")
            break
    except Exception:
        continue

if not w3:
    print("❌ Cannot connect to any Polygon RPC")
    sys.exit(1)

account = w3.eth.account.from_key(pk)
ADDRESS = account.address

print(f"🦊 Wallet: {ADDRESS}")
print(f"   Chain:  Polygon (137)")
print()

# Check balances
usdc = w3.eth.contract(address=USDC_E, abi=ERC20_ABI)
pol_balance = w3.eth.get_balance(ADDRESS)
usdc_balance = usdc.functions.balanceOf(ADDRESS).call()

print(f"💰 POL balance:    {w3.from_wei(pol_balance, 'ether'):.4f} POL")
print(f"💵 USDC.e balance: {usdc_balance / 1e6:.2f} USDC")
print()

if pol_balance == 0:
    print("⚠️  You have 0 POL — you need a tiny amount for gas fees.")
    print("   Send ~$1-2 of POL to this address, or swap on QuickSwap.")
    print("   Continuing anyway to check approvals...\n")


def send_tx(tx_builder):
    """Build, sign and send a transaction, return receipt."""
    tx = tx_builder.build_transaction({
        "from": ADDRESS,
        "nonce": w3.eth.get_transaction_count(ADDRESS),
        "chainId": CHAIN_ID,
        "maxFeePerGas": w3.eth.gas_price * 2,
        "maxPriorityFeePerGas": w3.to_wei(30, "gwei"),
    })
    tx["gas"] = w3.eth.estimate_gas(tx)
    
    signed = w3.eth.account.sign_transaction(tx, pk)
    tx_hash = w3.eth.send_raw_transaction(signed.raw_transaction)
    print(f"   Tx: {tx_hash.hex()}")
    receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=60)
    if receipt.status == 1:
        print(f"   ✅ Confirmed in block {receipt.blockNumber}")
    else:
        print(f"   ❌ Transaction FAILED")
    return receipt


# ─── Approval 1: USDC.e → CTF (for minting) ────────────
print("━" * 50)
print("1️⃣  USDC.e → CTF contract (for splitPosition minting)")
allowance1 = usdc.functions.allowance(ADDRESS, CTF).call()
if allowance1 >= MAX_UINT256 // 2:
    print(f"   Already approved ✓")
else:
    print(f"   Current allowance: {allowance1 / 1e6:.2f} USDC")
    print(f"   Approving max...")
    send_tx(usdc.functions.approve(CTF, MAX_UINT256))
print()

# ─── Approval 2: CTF tokens → CTF Exchange (for selling) ─
print("━" * 50)
print("2️⃣  CTF tokens → CTF Exchange (for CLOB selling)")
ctf = w3.eth.contract(address=CTF, abi=ERC1155_ABI)
approved2 = ctf.functions.isApprovedForAll(ADDRESS, CTF_EXCHANGE).call()
if approved2:
    print(f"   Already approved ✓")
else:
    print(f"   Approving...")
    send_tx(ctf.functions.setApprovalForAll(CTF_EXCHANGE, True))
print()

# ─── Approval 3: USDC.e → CTF Exchange (for CLOB trades) ─
print("━" * 50)
print("3️⃣  USDC.e → CTF Exchange (for CLOB trading)")
allowance3 = usdc.functions.allowance(ADDRESS, CTF_EXCHANGE).call()
if allowance3 >= MAX_UINT256 // 2:
    print(f"   Already approved ✓")
else:
    print(f"   Current allowance: {allowance3 / 1e6:.2f} USDC")
    print(f"   Approving max...")
    send_tx(usdc.functions.approve(CTF_EXCHANGE, MAX_UINT256))
print()

# ─── Approval 4: CTF tokens → NegRisk CTF Exchange (for SELLING on neg-risk markets) ─
print("━" * 50)
print("4️⃣  CTF tokens → NegRisk CTF Exchange (for CLOB selling on 5-min markets)")
approved4 = ctf.functions.isApprovedForAll(ADDRESS, NEG_RISK_EXCHANGE).call()
if approved4:
    print(f"   Already approved ✓")
else:
    print(f"   Approving...")
    send_tx(ctf.functions.setApprovalForAll(NEG_RISK_EXCHANGE, True))
print()

# ─── Approval 5: USDC.e → NegRisk CTF Exchange (for trading) ─
print("━" * 50)
print("5️⃣  USDC.e → NegRisk CTF Exchange (for CLOB trading on 5-min markets)")
allowance5 = usdc.functions.allowance(ADDRESS, NEG_RISK_EXCHANGE).call()
if allowance5 >= MAX_UINT256 // 2:
    print(f"   Already approved ✓")
else:
    print(f"   Current allowance: {allowance5 / 1e6:.2f} USDC")
    print(f"   Approving max...")
    send_tx(usdc.functions.approve(NEG_RISK_EXCHANGE, MAX_UINT256))
print()

print("━" * 50)
print("✅ All approvals done! Ready to buy + sell on neg-risk markets.")
print(f"   Wallet: {ADDRESS}")
print(f"   USDC.e: {usdc_balance / 1e6:.2f}")
