#!/usr/bin/env bash
# The regtest Lightning backbone every payment slice needs: a funded bitcoind,
# two LND nodes with a channel between them, and a Cashu mint with balance.
#
# The GitHub original builds this once inside a single 46-step job and every
# later step inherits it. Slices each get a fresh sandbox, so the setup runs
# per slice and has to be reliable rather than merely repeatable — hence the
# waits here poll for the state they need instead of sleeping a fixed number of
# seconds and hoping.

# shellcheck shell=bash

BITCOIN_WALLET="${BITCOIN_WALLET:-my_wallet}"

bitcoin_cli() {
  docker exec bitcoind bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass "$@"
}

bitcoin_wallet_cli() {
  docker exec bitcoind bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass \
    -rpcwallet="$BITCOIN_WALLET" "$@"
}

lnd_cli() { docker exec lndnode lncli -n regtest "$@"; }

# The receiver listens on 10010, so every call has to say so.
lnd_receiver_cli() {
  docker exec lndnode-receiver lncli -n regtest --rpcserver=127.0.0.1:10010 "$@"
}

cln_cli() { docker exec cln lightning-cli --network=regtest "$@"; }

cashu_cli() { docker exec cashu-mint poetry run cashu "$@"; }

# Mint a token of `amount` sats and echo just the token.
#
# `cashu send` prints two lines — the token, then "Balance: N sat" — so
# capturing its stdout wholesale yields "cashu...satBalance: 24970 sat", which
# the module rejects with a decode error at the colon. That reads like a
# malformed-token bug rather than a shell mistake, which is what makes it worth
# a function.
cashu_send() {
  local amount="$1" output token
  output="$(docker exec cashu-mint sh -c "poetry run cashu send ${amount} 2>/dev/null" || true)"
  # awk rather than `head -1`: head exits at the first line, the producer dies
  # of SIGPIPE, and pipefail turns that into a failed command substitution.
  token="$(grep -o 'cashu[A-Za-z0-9_-]*' <<< "$output" | awk 'NR==1' || true)"
  [ -n "$token" ] || fail "the mint issued no token for ${amount} sat: ${output}"
  printf '%s' "$token"
}

# ------------------------------------------------------------- readiness ---

wait_for_bitcoind() {
  wait_for 180 "bitcoind RPC" bitcoin_cli getblockchaininfo \
    || fail "bitcoind never answered RPC"
}

# `getinfo` succeeding is not enough: LND answers before it has caught up with
# the chain, and a wallet operation issued in that window fails in ways that
# read like a funding bug.
#
# The predicates are functions rather than `bash -c` strings because the CLI
# wrappers above are shell functions, which a fresh `bash -c` would not have.
_lnd_is_synced() { "$1" getinfo 2>/dev/null | jq -e '.synced_to_chain == true' >/dev/null; }
_lnd_has_confirmed_balance() {
  [ "$("$1" walletbalance 2>/dev/null | jq -r '.confirmed_balance // 0')" -gt 0 ] 2>/dev/null
}

wait_for_lnd_synced() {
  local cli="$1" name="$2"
  wait_for 300 "${name} RPC" "$cli" getinfo || fail "${name} never answered RPC"
  wait_for 300 "${name} chain sync" _lnd_is_synced "$cli" \
    || fail "${name} never synced to chain"
}

wait_for_cln() {
  wait_for 300 "CLN RPC" cln_cli getinfo || fail "CLN never answered RPC"
}

wait_for_cashu_mint() {
  wait_for 300 "cashu mint" curl -sf --max-time 5 http://localhost:3338/v1/info \
    || fail "cashu mint never became reachable"
}

# ------------------------------------------------------------------ chain ---

mine_blocks() {
  local count="${1:-6}" addr
  addr="$(bitcoin_wallet_cli getnewaddress)"
  bitcoin_wallet_cli generatetoaddress "$count" "$addr" >/dev/null
}

ensure_bitcoin_wallet() {
  if grep -q "\"${BITCOIN_WALLET}\"" <<< "$(bitcoin_cli listwallets)"; then
    return 0
  fi
  # A wallet can exist on disk from an earlier container start without being
  # loaded; create-then-load covers both.
  bitcoin_cli createwallet "$BITCOIN_WALLET" >/dev/null 2>&1 \
    || bitcoin_cli loadwallet "$BITCOIN_WALLET" >/dev/null 2>&1 \
    || fail "could not create or load bitcoin wallet ${BITCOIN_WALLET}"
}

# ------------------------------------------------------------------ nodes ---

# Send `amount` BTC to a node's fresh address and confirm it.
fund_lnd_node() {
  local cli="$1" name="$2" amount="${3:-1}" addr
  addr="$($cli newaddress p2wkh | jq -r '.address')"
  [ -n "$addr" ] && [ "$addr" != "null" ] || fail "${name} produced no funding address"
  l402_log "funding ${name} with ${amount} BTC at ${addr}"
  bitcoin_wallet_cli sendtoaddress "$addr" "$amount" >/dev/null
  mine_blocks 6
  wait_for 120 "${name} confirmed balance" _lnd_has_confirmed_balance "$cli" \
    || fail "${name} never saw its funding confirm"
}

# Wait until the funding transaction has confirmed and both sides call the
# channel usable. Without this the first payment fails with "no route", which
# is indistinguishable from a routing bug in the module under test.
wait_for_channel_active() {
  local cli="$1" peer_pubkey="$2" name="$3"
  local deadline=$((SECONDS + 180))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if [ "$($cli listchannels --peer="$peer_pubkey" 2>/dev/null \
      | jq '[.channels[] | select(.active == true)] | length' 2>/dev/null || echo 0)" -gt 0 ]; then
      pass "${name} channel active"
      return 0
    fi
    mine_blocks 1
    sleep 2
  done
  fail "${name} channel never became active"
}

# ---------------------------------------------------------------- backbone --

# bitcoind + two LND nodes + a funded channel + a funded Cashu mint. Everything
# after this point can pay an invoice.
setup_lightning_backbone() {
  # When several slices share one lease the backbone is already funded and
  # channelled by the time the second one runs. Mining another 101 blocks and
  # re-funding would be harmless but costs a couple of minutes per slice, which
  # on a paid lease is the whole reason for sharing it in the first place.
  if lnd_receiver_cli listchannels 2>/dev/null \
    | jq -e '[.channels[] | select(.active == true)] | length > 0' >/dev/null 2>&1; then
    pass "lightning backbone already up (reusing it)"
    return 0
  fi

  l402_log "== bringing up bitcoind, tor, LND nodes and the Cashu mint"
  docker compose up -d --no-deps bitcoind tor lndnode lndnode-receiver cashu-mint

  wait_for_bitcoind
  ensure_bitcoin_wallet

  # 101 blocks is the coinbase maturity, so this is the minimum that leaves a
  # spendable balance.
  l402_log "mining 101 blocks to make coinbase spendable"
  mine_blocks 101

  wait_for_lnd_synced lnd_cli "lndnode"
  wait_for_lnd_synced lnd_receiver_cli "lndnode-receiver"

  fund_lnd_node lnd_cli "lndnode" 1
  fund_lnd_node lnd_receiver_cli "lndnode-receiver" 1

  local lnd_pubkey
  lnd_pubkey="$(lnd_cli getinfo | jq -r '.identity_pubkey')"
  [ -n "$lnd_pubkey" ] && [ "$lnd_pubkey" != "null" ] || fail "lndnode has no identity pubkey"

  l402_log "opening channel lndnode-receiver -> lndnode"
  lnd_receiver_cli connect "${lnd_pubkey}@lndnode:9735" >/dev/null 2>&1 || true
  lnd_receiver_cli openchannel --node_key="$lnd_pubkey" --local_amt=1000000 >/dev/null \
    || fail "could not open the receiver -> lndnode channel"
  mine_blocks 6
  wait_for_channel_active lnd_receiver_cli "$lnd_pubkey" "lndnode-receiver -> lndnode"

  setup_cashu_balance

  # The mint pays out over the receiver's channel, so the receiver needs
  # inbound liquidity on lndnode's side of it.
  l402_log "pushing liquidity lndnode -> lndnode-receiver"
  local invoice
  invoice="$(lnd_cli addinvoice --amt=8000 | jq -r '.payment_request')"
  lnd_receiver_cli payinvoice --force "$invoice" >/dev/null 2>&1 || true

  pass "lightning backbone ready"
}

# Mint 25k sats into the Cashu wallet by paying its own invoice from the
# receiver node.
setup_cashu_balance() {
  wait_for_cashu_mint

  l402_log "minting Cashu balance"
  local invoice
  # `cashu invoice` blocks until the invoice is paid, so it runs in the
  # background and the invoice is read out of its output.
  docker exec cashu-mint sh -c "poetry run cashu invoice 25000 2>&1" > /tmp/cashu_invoice.txt 2>&1 &
  local cashu_pid=$!

  invoice=""
  local deadline=$((SECONDS + 60))
  while [ "$SECONDS" -lt "$deadline" ] && [ -z "$invoice" ]; do
    invoice="$(grep 'Invoice:' /tmp/cashu_invoice.txt 2>/dev/null | tail -1 | cut -d' ' -f2 || true)"
    [ -n "$invoice" ] || sleep 2
  done

  if [ -z "$invoice" ]; then
    l402_log "WARNING: the mint produced no invoice; slices needing Cashu balance will fail"
    kill "$cashu_pid" 2>/dev/null || true
    return 0
  fi

  lnd_receiver_cli sendpayment --pay_req="$invoice" -f >/dev/null 2>&1 || true
  wait "$cashu_pid" 2>/dev/null || true
  rm -f /tmp/cashu_invoice.txt

  local balance
  balance="$(cashu_cli balance 2>/dev/null | grep 'Balance:' | awk '{print $2}' || echo 0)"
  l402_log "Cashu wallet balance: ${balance} sats"
}

# --------------------------------------------------------------- payments ---

# Pay a BOLT11 invoice from lndnode and echo the preimage.
#
# Selected by payment hash rather than "the last payment in the list": several
# slices pay more than once and listpayments is ordered by settle time, so the
# last entry is not reliably the one just paid.
pay_invoice_get_preimage() {
  local invoice="$1" payment_hash preimage
  payment_hash="$(lnd_cli decodepayreq "$invoice" | jq -r '.payment_hash')"
  [ -n "$payment_hash" ] && [ "$payment_hash" != "null" ] || fail "could not decode invoice"

  lnd_cli payinvoice -f "$invoice" >/dev/null 2>&1 || true

  local deadline=$((SECONDS + 60))
  while [ "$SECONDS" -lt "$deadline" ]; do
    preimage="$(lnd_cli listpayments 2>/dev/null \
      | jq -r --arg h "$payment_hash" \
        '.payments[] | select(.payment_hash == $h and .status == "SUCCEEDED") | .payment_preimage' \
      | tail -1)"
    if [ -n "$preimage" ] && [ "$preimage" != "null" ]; then
      printf '%s' "$preimage"
      return 0
    fi
    sleep 2
  done

  fail "payment for hash ${payment_hash} never settled"
}

# msat amount encoded in an invoice, for the pricing assertions.
invoice_amount_msat() {
  lnd_cli decodepayreq "$1" | jq -r '.num_msat'
}
