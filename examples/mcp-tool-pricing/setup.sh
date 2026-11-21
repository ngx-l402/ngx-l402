#!/usr/bin/env bash
# Starts the stack, funds both Lightning nodes on regtest, and opens a channel
# from the client's wallet to the gateway's node so the client can pay.
# Safe to re-run: an existing channel is kept.
set -euo pipefail
cd "$(dirname "$0")"

btc() { docker compose exec -T bitcoind bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass "$@"; }
lncli() { local node=$1; shift; docker compose exec -T "$node" lncli -n regtest "$@"; }
mine() { btc -rpcwallet=miner generatetoaddress "$1" "$(btc -rpcwallet=miner getnewaddress)" >/dev/null; }
# wait_for <seconds> <what> <command...>
wait_for() {
    local secs=$1 what=$2
    shift 2
    for _ in $(seq "$secs"); do "$@" >/dev/null 2>&1 && return 0; sleep 1; done
    echo "timed out waiting for $what" >&2
    return 1
}

# The gateway's macaroon root key. Anyone who knows it can mint tokens that
# pass, so each checkout gets its own, kept in .env (gitignored).
[ -f .env ] || echo "ROOT_KEY=$(openssl rand -hex 32)" > .env

echo "Starting the stack (the first run builds the module, which takes a few minutes)..."
docker compose up -d --build --wait
# The client only runs on demand, so `up` doesn't build it; build it here so
# `docker compose run --rm client` always runs the current client.py.
docker compose --profile client build client >/dev/null

wait_for 60 bitcoind btc getblockchaininfo
btc createwallet miner >/dev/null 2>&1 || btc loadwallet miner >/dev/null 2>&1 || true
[ "$(btc getblockcount)" -ge 101 ] || mine 101

synced() { lncli "$1" getinfo | grep -q '"synced_to_chain": *true'; }
for node in lnd-gateway lnd-wallet; do wait_for 120 "$node to sync" synced "$node"; done

pubkey=$(lncli lnd-gateway getinfo | sed -n 's/.*"identity_pubkey": *"\([^"]*\)".*/\1/p')
# Also after a restart, when the nodes don't reconnect on their own right away.
connected() {
    lncli lnd-wallet connect "$pubkey@lnd-gateway:9735" >/dev/null 2>&1 || true
    lncli lnd-wallet listpeers | grep -q "$pubkey"
}
wait_for 60 "the wallet to connect to the gateway" connected
# Any channel to the gateway counts, inactive or still pending: an inactive one
# only needs the peers to reconnect, and opening another would leave two.
open_chan() { lncli lnd-wallet listchannels --peer "$pubkey" | grep -q '"channel_point"'; }
pending_chan() { lncli lnd-wallet pendingchannels | grep -q "$pubkey"; }

if open_chan || pending_chan; then
    echo "Channel to the gateway already exists."
else
    echo "Funding the wallet and opening a 1,000,000 sat channel to the gateway..."
    addr=$(lncli lnd-wallet newaddress p2wkh | sed -n 's/.*"address": *"\([^"]*\)".*/\1/p')
    btc -rpcwallet=miner sendtoaddress "$addr" 1 >/dev/null
    mine 6
    funded() { lncli lnd-wallet walletbalance | grep -q '"confirmed_balance": *"[1-9]'; }
    wait_for 120 "the wallet's funds to confirm" funded
    wait_for 60 "the wallet to sync" synced lnd-wallet
    wait_for 60 "the wallet to connect to the gateway" connected
    lncli lnd-wallet openchannel --node_key="$pubkey" --local_amt=1000000 >/dev/null
fi
if pending_chan; then mine 6; fi
active() { lncli lnd-wallet listchannels --peer "$pubkey" --active_only | grep -q '"active": *true'; }
wait_for 120 "the channel to be active" active
# A channel can read active a moment before it carries a payment, so finish
# with a 1-sat payment to the gateway's node.
pays() {
    local inv
    inv=$(lncli lnd-gateway addinvoice --amt 1 | sed -n 's/.*"payment_request": *"\([^"]*\)".*/\1/p')
    lncli lnd-wallet payinvoice --force "$inv" | grep -q SUCCEEDED
}
wait_for 60 "a test payment to the gateway" pays

echo "Ready: the gateway listens on http://localhost:8402/mcp"
echo "Run the paying client with: docker compose run --rm client"
