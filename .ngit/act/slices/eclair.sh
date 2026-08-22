#!/usr/bin/env bash
# Slice 7 — Eclair backend: invoice generation and autodetect over an
# LND -> Eclair channel.
#
# Ports "Start Eclair Node and nginx-eclair" and "Run Integration Tests -
# Eclair" from .github/workflows/tests.yml.
#
# Eclair is the fussiest node in the suite about channel confirmation: it moves
# a channel out of WAIT_FOR_FUNDING_CONFIRMED only on a live ZMQ block
# notification, so blocks mined before it is listening do not count. The
# sequence below — open, confirm, restart to force an RPC catch-up, then mine
# again so a fresh notification arrives — is what makes that reliable, and the
# order matters.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-eclair eclair lndnode redis"
write_env_file

setup_lightning_backbone

eclair_api() {
  local endpoint="$1"
  curl -s -u ":eclairpass" -X POST --max-time 10 "http://localhost:8282/${endpoint}"
}

eclair_block_height() { eclair_api getinfo | jq -r '.blockHeight // 0'; }
bitcoind_block_height() { bitcoin_cli getblockcount; }

_eclair_synced_to() {
  local target="$1" height
  height="$(eclair_block_height 2>/dev/null || echo 0)"
  [ "${height:-0}" -ge "$target" ] && [ "${height:-0}" -gt 0 ]
}

# ---------------------------------------------------------------- Eclair ----

l402_log "== starting Eclair"

# A volume left behind by an earlier run makes Eclair wait for a funding
# transaction that no longer exists on this regtest chain. The lease is fresh,
# but a retried job on the same sandbox is not.
docker compose rm -fsv eclair >/dev/null 2>&1 || true
docker volume rm "${COMPOSE_PROJECT_NAME}_eclair-data" >/dev/null 2>&1 || true

release_serving_port
docker compose up -d --build --no-deps eclair nginx-eclair redis
wait_for_container_running eclair

wait_for 300 "Eclair API" eclair_api getinfo || fail "Eclair never answered its API"
wait_for 300 "Eclair chain sync" _eclair_synced_to "$(bitcoind_block_height)" \
  || fail "Eclair never caught up with bitcoind"

eclair_node_id="$(eclair_api getinfo | jq -r '.nodeId')"
[ -n "$eclair_node_id" ] && [ "$eclair_node_id" != "null" ] || fail "Eclair has no node id"
l402_log "Eclair node id: ${eclair_node_id}"

# ---------------------------------------------------------------- channel ---

l402_log "== opening an LND -> Eclair channel"

fund_lnd_node lnd_cli "lndnode" 10

lnd_cli connect "${eclair_node_id}@eclair:9945" >/dev/null 2>&1 || true
sleep 3
lnd_cli openchannel --node_key="$eclair_node_id" --local_amt=1000000 --min_confs=0 >/dev/null 2>&1 || true

mine_blocks 6

# Restarting forces Eclair to re-scan its channel history over RPC, which is
# how it discovers a funding transaction that confirmed while it was not
# watching.
bitcoind_tip="$(bitcoind_block_height)"
l402_log "restarting Eclair to force an RPC catch-up to block ${bitcoind_tip}"
docker compose restart eclair
wait_for 300 "Eclair API after restart" eclair_api getinfo \
  || fail "Eclair did not come back after the restart"
wait_for 300 "Eclair sync after restart" _eclair_synced_to "$bitcoind_tip" \
  || fail "Eclair did not sync past block ${bitcoind_tip} after the restart"

# Now that it is listening, fresh blocks produce the ZMQ notifications that
# move the channel to NORMAL.
l402_log "mining again so Eclair sees live confirmations"
mine_blocks 6

lnd_cli connect "${eclair_node_id}@eclair:9945" >/dev/null 2>&1 || true

channel_ready=false
deadline=$((SECONDS + 240))
attempt=0
while [ "$SECONDS" -lt "$deadline" ]; do
  attempt=$((attempt + 1))

  eclair_state="$(eclair_api channels | jq -r '.[].state' 2>/dev/null | grep -m1 NORMAL || true)"
  lnd_active="$(lnd_cli listchannels --peer="$eclair_node_id" 2>/dev/null \
    | jq '[.channels[] | select(.active == true)] | length' 2>/dev/null || echo 0)"

  if [ "$eclair_state" = "NORMAL" ] || [ "${lnd_active:-0}" -gt 0 ]; then
    pass "LND <-> Eclair channel is usable (eclair=${eclair_state:-none}, lnd active=${lnd_active})"
    channel_ready=true
    break
  fi

  # Keep the peer connection alive and keep block notifications flowing.
  [ $((attempt % 5)) -eq 1 ] && lnd_cli connect "${eclair_node_id}@eclair:9945" >/dev/null 2>&1 || true
  [ $((attempt % 2)) -eq 0 ] && mine_blocks 1
  sleep 3
done

if [ "$channel_ready" != true ]; then
  l402_log "Eclair channels: $(eclair_api channels)"
  lnd_cli listchannels >&2 || true
  fail "the LND <-> Eclair channel never became usable"
fi

# ------------------------------------------------------------ nginx-eclair --

l402_log "== Eclair-backed routes"

wait_for_container_running nginx-eclair
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-eclair"
wait_for_l402_ready "${BASE_URL}/protected"

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route without credentials"
assert_l402_challenge "Eclair issues an L402 challenge"

invoice="$(l402_invoice)"
case "$invoice" in
  lnbcrt*|lnbc*|lntb*) pass "challenge carries a BOLT11 invoice (${invoice:0:12}...)" ;;
  *) HTTP_RESPONSE="invoice: ${invoice}"; fail "challenge value is not a BOLT11 invoice" ;;
esac

l402_log "== Eclair autodetect"

http_request -L "${BASE_URL}/protected-auto"
assert_status 402 "autodetect route challenge"
auto_macaroon="$(l402_macaroon)"
auto_invoice="$(l402_invoice)"

lnd_cli payinvoice -f "$auto_invoice" >/dev/null 2>&1 || true

redeemed=false
for _ in $(seq 1 10); do
  http_request -H "Authorization: L402 ${auto_macaroon}" "${BASE_URL}/protected-auto"
  if [ "$HTTP_STATUS" = "200" ]; then
    redeemed=true
    break
  fi
  sleep 3
done
[ "$redeemed" = true ] || assert_status 200 "autodetect accepts the macaroon after payment"
pass "autodetect accepts a macaroon whose Eclair invoice was paid"

l402_log "== Eclair slice passed"
