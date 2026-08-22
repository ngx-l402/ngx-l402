#!/usr/bin/env bash
# Slice 5 — everything that depends on Redis state or on the Cashu redemption
# loop: replay prevention, invoice rate limiting, redemption back out over
# Lightning, mint parameter discovery, and multi-tenant LNURL mapping.
#
# Ports the "Redis Replay Attack Prevention", "Invoice Rate Limiting", "Cashu
# redemption on Lightning", "Dynamic Parameter Discovery" and "Multi-tenant
# LNURL Cashu Redemption" steps of .github/workflows/tests.yml.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-lnd nginx-lnurl cashu-mint lndnode lndnode-receiver redis"
write_env_file

setup_lightning_backbone

redis_key_count() {
  docker exec redis redis-cli --no-raw keys "$1" 2>/dev/null | grep -c . || true
}

# ------------------------------------------------------------ nginx-lnd -----

l402_log "== starting nginx-lnd"
docker exec lndnode-receiver chmod a+rx \
  /root /root/.lnd /root/.lnd/data /root/.lnd/data/chain \
  /root/.lnd/data/chain/bitcoin /root/.lnd/data/chain/bitcoin/regtest
docker exec lndnode-receiver chmod a+r \
  /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon /root/.lnd/tls.cert

release_serving_port
docker compose up -d --build --no-deps nginx-lnd redis
wait_for_container_running nginx-lnd
docker exec nginx-lnd chmod 755 /root
docker exec nginx-lnd chmod 770 /app/data
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-lnd"
wait_for_l402_ready "${BASE_URL}/protected"

# ------------------------------------------------------ L402 replay ---------

l402_log "== L402 preimage replay prevention"

flush_redis

http_request "${BASE_URL}/protected"
assert_status 402 "challenge for the replay test"
replay_macaroon="$(l402_macaroon)"
replay_preimage="$(pay_invoice_get_preimage "$(l402_invoice)")"

http_request -H "Authorization: L402 ${replay_macaroon}:${replay_preimage}" "${BASE_URL}/protected"
assert_status 200 "first use of a paid preimage"

# The preimage is written after the response is served, so give it a moment
# before asserting on Redis rather than racing the write.
wait_for 30 "preimage recorded in redis" \
  bash -c '[ "$(docker exec redis redis-cli keys "l402:preimage:*" | grep -c .)" -ge 1 ]' \
  || fail "no l402:preimage:* key was written to redis"

http_request -H "Authorization: L402 ${replay_macaroon}:${replay_preimage}" "${BASE_URL}/protected"
assert_status 401 "replayed preimage rejected"

l402_log "== Cashu token replay prevention"

cashu_token="$(cashu_send 10)"

cashu_redeem "$cashu_token" "${BASE_URL}/protected"
assert_status 200 "first use of a Cashu token"

wait_for 30 "cashu token recorded in redis" \
  bash -c '[ "$(docker exec redis redis-cli keys "l402:cashu_token:*" | grep -c .)" -ge 1 ]' \
  || fail "no l402:cashu_token:* key was written to redis"

http_request -L -H "Authorization: Cashu ${cashu_token}" "${BASE_URL}/protected"
assert_status 401 "replayed Cashu token rejected"

# --------------------------------------------------------- rate limiting ---

l402_log "== invoice rate limiting (2r/m on /rate-limited)"

flush_redis

http_request "${BASE_URL}/rate-limited"
assert_status 402 "first invoice request is within the limit"

http_request "${BASE_URL}/rate-limited"
assert_status 402 "second invoice request is at the limit"

http_request "${BASE_URL}/rate-limited"
assert_status 429 "third invoice request is refused"
assert_response_matches '^Retry-After:' "429 carries a Retry-After header"

[ "$(redis_key_count 'l402:invoice_rate:*')" -ge 1 ] \
  || fail "no l402:invoice_rate:* counter in redis"
pass "rate limit counter persisted in redis"

# The limiter exists to stop unauthenticated invoice minting; a request that
# presents credentials must reach the L402 handler even when the limit is hit,
# whatever that handler then decides.
http_request -H "Authorization: L402 invalid_token:deadbeef" "${BASE_URL}/rate-limited"
[ "$HTTP_STATUS" != "429" ] \
  || fail "an authenticated request was rate-limited (429); the limiter should bypass it"
pass "authenticated request bypassed the limiter (got ${HTTP_STATUS})"

# ------------------------------------------------- redemption over LN -------

l402_log "== Cashu redemption back out over Lightning"

receiver_balance_before="$(lnd_receiver_cli channelbalance | jq -r '.local_balance.sat')"
l402_log "receiver local channel balance before: ${receiver_balance_before} sats"

redemption_token="$(cashu_send 100)"
cashu_redeem "$redemption_token" "${BASE_URL}/protected"
assert_status 200 "100 sat Cashu token accepted"

# CASHU_REDEMPTION_INTERVAL_SECS is 15 in docker-compose.yml; read it from the
# container so this does not silently stop waiting long enough if that changes.
redemption_interval="$(docker exec nginx-lnd sh -c 'echo "$CASHU_REDEMPTION_INTERVAL_SECS"' | tr -d '\r')"
redemption_interval="${redemption_interval:-15}"
l402_log "waiting up to $((redemption_interval * 4 + 60))s for the redemption loop"

redeemed=false
deadline=$((SECONDS + redemption_interval * 4 + 60))
while [ "$SECONDS" -lt "$deadline" ]; do
  receiver_balance_now="$(lnd_receiver_cli channelbalance | jq -r '.local_balance.sat')"
  if [ "$receiver_balance_now" -gt "$receiver_balance_before" ]; then
    l402_log "receiver local channel balance after: ${receiver_balance_now} sats"
    pass "redemption moved $((receiver_balance_now - receiver_balance_before)) sats over Lightning"
    redeemed=true
    break
  fi
  sleep 5
done

if [ "$redeemed" != true ]; then
  l402_log "receiver channel balance never increased; dumping redemption state"
  docker exec nginx-lnd cat /var/log/nginx/cashu_redemption.log 2>&1 | tail -50 || true
  fail "the Cashu redemption loop did not settle a payment"
fi

l402_log "== mint parameter discovery"

redemption_log="/var/log/nginx/cashu_redemption.log"
docker exec nginx-lnd test -f "$redemption_log" \
  || fail "redemption log ${redemption_log} does not exist"

docker exec nginx-lnd grep -q "Fetching Mint Info for dynamic parameter discovery" "$redemption_log" \
  || { docker exec nginx-lnd cat "$redemption_log" >&2 || true
       fail "dynamic parameter discovery never ran"; }
pass "dynamic parameter discovery ran"
docker exec nginx-lnd grep -E "(Fetching Mint Info|Found dynamic|NUT-05|NUT-08|Using default)" \
  "$redemption_log" | tail -10 || true

# ------------------------------------------------- multi-tenant LNURL -------

l402_log "== multi-tenant LNURL redemption"

docker compose stop nginx-lnd
docker compose up -d --build --no-deps nginx-lnurl
wait_for_container_running nginx-lnurl
export DIAG_CONTAINERS="nginx-lnurl cashu-mint redis"
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-lnurl"

flush_redis

# The two tenants map to different LNURL addresses in nginx.conf, so accepting
# both proves the proof-to-tenant mapping rather than a single global wallet.
tenant1_token="$(cashu_send 50)"
tenant2_token="$(cashu_send 30)"

cashu_redeem "$tenant1_token" "${BASE_URL}/tenant1"
assert_status 200 "tenant1 accepts its token"

cashu_redeem "$tenant2_token" "${BASE_URL}/tenant2"
assert_status 200 "tenant2 accepts its token"

wait_for 30 "proof-to-lnurl mappings in redis" \
  bash -c '[ "$(docker exec redis redis-cli keys "cashu:proof_lnurl:*" | grep -c .)" -ge 1 ]' \
  || fail "no cashu:proof_lnurl:* mappings were written"
pass "per-tenant proof mappings recorded"

l402_log "== Cashu/Redis slice passed"
