#!/usr/bin/env bash
# Slice 4 — Nostr Wallet Connect: invoice generation over NWC, macaroon
# expiry, and the indefinite-access route.
#
# Ports "Start NWC service" and "Run Integration Tests - NWC" from
# .github/workflows/tests.yml, plus the LND -> CLN channel and NWC URI that
# the GitHub suite creates in its "Verify LND node" step.
#
# Note the external dependency: CLN's nip47 plugin talks to a public relay
# (wss://relay.damus.io, set in docker-compose.yml). If this slice fails at the
# invoice-generation step while every other slice passes, suspect the relay
# before suspecting the module.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-nwc cln lndnode redis"
write_env_file

setup_lightning_backbone

# ------------------------------------------------- CLN and the NWC wallet ---

l402_log "== starting CLN"
docker compose up -d --build --no-deps cln
wait_for_cln

l402_log "== opening an LND -> CLN channel so invoices can be paid"
fund_lnd_node lnd_cli "lndnode" 10

cln_pubkey="$(cln_cli getinfo | jq -r '.id')"
[ -n "$cln_pubkey" ] && [ "$cln_pubkey" != "null" ] || fail "CLN has no node id"

lnd_cli connect "${cln_pubkey}@cln:9835" >/dev/null 2>&1 || true
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

l402_log "== creating the NWC connection"
NWC_URI="$(cln_cli nip47-create label=nwc-for-l402 budget_msat=0 | jq -r '.uri')"
[ -n "$NWC_URI" ] && [ "$NWC_URI" != "null" ] \
  || fail "CLN's nip47 plugin returned no NWC URI (relay unreachable?)"
export NWC_URI
write_env_file

release_serving_port
docker compose up -d --build --no-deps nginx-nwc redis
wait_for_container_running nginx-nwc
wait_for_http_status "${BASE_URL}/" 200 240 "nginx-nwc"

# ------------------------------------------------------------ NWC routes ----

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

l402_log "== paying an NWC-generated invoice"

# /protected-timeout carries l402_macaroon_timeout 15, which the expiry check
# below depends on.
http_request -L "${BASE_URL}/protected-timeout"
assert_status 402 "timeout route without credentials"
assert_l402_challenge "NWC issues an L402 challenge"

timeout_macaroon="$(l402_macaroon)"
timeout_preimage="$(pay_invoice_get_preimage "$(l402_invoice)")"

http_request -H "Authorization: L402 ${timeout_macaroon}:${timeout_preimage}" \
  "${BASE_URL}/protected-timeout"
assert_status 200 "paid credentials accepted"

l402_log "== macaroon expiry (15s)"
sleep 16
http_request -H "Authorization: L402 ${timeout_macaroon}:${timeout_preimage}" \
  "${BASE_URL}/protected-timeout"
assert_status 401 "expired macaroon rejected"

# ---------------------------------------------------- indefinite access -----

l402_log "== indefinite access (same preimage, repeated use)"

http_request -L "${BASE_URL}/protected-indefinite"
assert_status 402 "indefinite route without credentials"
assert_l402_challenge "indefinite route issues an L402 challenge"

indef_macaroon="$(l402_macaroon)"
indef_preimage="$(pay_invoice_get_preimage "$(l402_invoice)")"

http_request -H "Authorization: L402 ${indef_macaroon}:${indef_preimage}" \
  "${BASE_URL}/protected-indefinite"
assert_status 200 "first use of an indefinite-access token"

# This is the whole point of l402_indefinite_access: replay protection must not
# apply here, where it would on /protected.
http_request -H "Authorization: L402 ${indef_macaroon}:${indef_preimage}" \
  "${BASE_URL}/protected-indefinite"
assert_status 200 "same token accepted a second time"

assert_rejects_forged_credentials "${BASE_URL}/protected"

l402_log "== NWC slice passed"
