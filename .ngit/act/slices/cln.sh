#!/usr/bin/env bash
# Slice 3 — Core Lightning: the CLN backend, BOLT12 offers, and autodetect
# over a real LND -> CLN payment.
#
# Ports the "Start cln services", "Run Integration Tests - CLN", the BOLT12
# steps and "Run Integration Tests - CLN Autodetect" from
# .github/workflows/tests.yml.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-cln nginx-bolt12 cln lndnode redis"
write_env_file

setup_lightning_backbone

# The CLN socket is created root-only; nginx reads it from a shared volume, so
# the directory has to be traversable from the nginx container too.
open_cln_socket_permissions() {
  docker exec "$1" chmod 777 /root /root/.lightning /root/.lightning/regtest
}

# ------------------------------------------------------------------- CLN ----

l402_log "== starting CLN and nginx-cln"
docker compose up -d --build --no-deps cln
wait_for_cln

release_serving_port
docker compose up -d --build --no-deps nginx-cln redis
wait_for_container_running nginx-cln
open_cln_socket_permissions nginx-cln
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-cln"

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

flush_redis

# The paid path is covered by the autodetect section below, once an LND -> CLN
# channel exists; at this point there is nothing that can pay a CLN invoice.
assert_rejects_forged_credentials "${BASE_URL}/protected"

http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route without credentials"
assert_l402_challenge "CLN issues an L402 challenge"

docker compose stop nginx-cln

# ---------------------------------------------------------------- BOLT12 ----

l402_log "== BOLT12 offer"

BOLT12_OFFER="$(cln_cli offer any "L402 Access" | jq -r '.bolt12')"
[ -n "$BOLT12_OFFER" ] && [ "$BOLT12_OFFER" != "null" ] \
  || fail "CLN did not return a BOLT12 offer"
export BOLT12_OFFER
write_env_file

release_serving_port
docker compose up -d --build --no-deps nginx-bolt12
wait_for_container_running nginx-bolt12
open_cln_socket_permissions nginx-bolt12
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-bolt12"

http_request -L "${BASE_URL}/protected"
assert_status 402 "BOLT12 protected route"
assert_l402_challenge "BOLT12 route issues an L402 challenge"

bolt12_invoice="$(l402_invoice)"
case "$bolt12_invoice" in
  lno*|lni*|lnbc*|lntb*) pass "challenge carries a BOLT12 offer/invoice or BOLT11 fallback" ;;
  *) HTTP_RESPONSE="invoice: ${bolt12_invoice}"
     fail "challenge value is neither a BOLT12 offer/invoice nor a BOLT11 invoice" ;;
esac

docker compose stop nginx-bolt12

# ------------------------------------------------------------ autodetect ----

l402_log "== opening an LND -> CLN channel for the autodetect test"

# The backbone funds lndnode with 1 BTC; the channel below plus fees needs
# more headroom than that leaves after the receiver channel.
fund_lnd_node lnd_cli "lndnode" 10

cln_pubkey="$(cln_cli getinfo | jq -r '.id')"
[ -n "$cln_pubkey" ] && [ "$cln_pubkey" != "null" ] || fail "CLN has no node id"

lnd_cli connect "${cln_pubkey}@cln:9835" >/dev/null 2>&1 || true

# Channel opens occasionally lose the race with peer connection setup.
opened=false
for attempt in 1 2 3; do
  if lnd_cli openchannel "$cln_pubkey" 1000000 >/dev/null 2>&1; then
    opened=true
    break
  fi
  l402_log "channel open attempt ${attempt} failed; reconnecting"
  lnd_cli connect "${cln_pubkey}@cln:9835" >/dev/null 2>&1 || true
  sleep 10
done
[ "$opened" = true ] || fail "could not open the LND -> CLN channel"

mine_blocks 6
wait_for_channel_active lnd_cli "$cln_pubkey" "lndnode -> cln"

l402_log "== CLN autodetect"

release_serving_port
docker compose up -d --no-deps nginx-cln
wait_for_container_running nginx-cln
open_cln_socket_permissions nginx-cln
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-cln"

http_request -L "${BASE_URL}/protected-auto"
assert_status 402 "autodetect route challenge"
auto_macaroon="$(l402_macaroon)"
auto_invoice="$(l402_invoice)"

# The invoice is a CLN one, so it is paid rather than tracked through lncli's
# payment list; autodetect then has to notice the settlement on the CLN side.
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
pass "autodetect accepts a macaroon whose CLN invoice was paid"

l402_log "== CLN slice passed"
