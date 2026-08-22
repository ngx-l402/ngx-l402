#!/usr/bin/env bash
# Slice 2 — LND backend: pricing, the full pay-then-redeem flow, method
# binding, autodetect and Cashu eCash acceptance.
#
# Ports the "Start nginx containers for LND" and "Run Integration Tests - LND"
# steps of .github/workflows/tests.yml, including the Cashu eCash block that
# lives inside them. Redis replay, rate limiting and redemption are slice 5.
#
# This is the first slice that pays a real invoice, so it needs the whole
# regtest backbone from .ngit/act/lib/lightning.sh.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-lnd lndnode lndnode-receiver cashu-mint redis"
write_env_file

setup_lightning_backbone

# ------------------------------------------------------------------ setup --

l402_log "== starting nginx-lnd"

# nginx runs as a non-root user inside the image but reads the receiver's
# macaroon and cert through a read-only volume, so the bits have to be readable
# from outside the node container.
docker exec lndnode-receiver chmod a+rx \
  /root /root/.lnd /root/.lnd/data /root/.lnd/data/chain \
  /root/.lnd/data/chain/bitcoin /root/.lnd/data/chain/bitcoin/regtest
docker exec lndnode-receiver chmod a+r \
  /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon /root/.lnd/tls.cert

release_serving_port
docker compose up -d --build --no-deps nginx-lnd redis
wait_for_container_running nginx-lnd
docker exec nginx-lnd chmod 755 /root

wait_for_http_status "${BASE_URL}/" 200 180 "nginx-lnd"
wait_for_l402_ready "${BASE_URL}/protected"

# --------------------------------------------------------------- routing ---

l402_log "== free and protected routes"

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route without credentials"
assert_l402_challenge "protected route returns an L402 challenge"

invoice="$(l402_invoice)"
amount="$(invoice_amount_msat "$invoice")"
[ "$amount" = "10000" ] || fail "default price is ${amount} msat, expected 10000"
pass "default price is 10000 msat"

# --------------------------------------------------------- dynamic price ---

l402_log "== dynamic pricing through redis"

docker exec redis redis-cli set /protected 15000 >/dev/null
http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route with a dynamic price set"
amount="$(invoice_amount_msat "$(l402_invoice)")"
[ "$amount" = "15000" ] || fail "dynamic price is ${amount} msat, expected 15000"
pass "redis-set price of 15000 msat honoured"

docker exec redis redis-cli del /protected >/dev/null
http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route after clearing the dynamic price"
amount="$(invoice_amount_msat "$(l402_invoice)")"
[ "$amount" = "10000" ] || fail "price after reset is ${amount} msat, expected 10000"
pass "price falls back to 10000 msat once the key is deleted"

# ---------------------------------------------------- valid credentials ----

flush_redis

l402_log "== paying an invoice and redeeming the token"

http_request "${BASE_URL}/protected"
assert_status 402 "fresh challenge for the redemption test"
cred_invoice="$(l402_invoice)"
cred_macaroon="$(l402_macaroon)"
cred_preimage="$(pay_invoice_get_preimage "$cred_invoice")"

http_request -H "Authorization: L402 ${cred_macaroon}:${cred_preimage}" "${BASE_URL}/protected"
assert_status 200 "paid credentials are accepted"

# The same preimage a second time is a replay, and this route is not
# l402_indefinite_access, so it must be refused.
http_request -H "Authorization: L402 ${cred_macaroon}:${cred_preimage}" "${BASE_URL}/protected"
assert_status 401 "replayed preimage rejected"

http_request -H "Authorization: L402 ${cred_macaroon}:fbe9ac25c04e14b10177514e2d57b0e39224e70277ac1a2cd23c28e58cd4ea35" \
  "${BASE_URL}/protected"
assert_status 401 "valid macaroon with the wrong preimage rejected"

assert_rejects_forged_credentials "${BASE_URL}/protected"

# --------------------------------------------------------- method binding --

l402_log "== method binding (RequestMethod caveat)"

flush_redis

http_request -X GET "${BASE_URL}/protected"
assert_status 402 "challenge for the method-binding test"
method_macaroon="$(l402_macaroon)"
method_preimage="$(pay_invoice_get_preimage "$(l402_invoice)")"

# HEAD rather than POST: this location uses try_files with static content, so
# nginx answers POST with 405 before the L402 handler runs, while HEAD reaches
# the access phase exactly as GET does.
#
# Run the cross-method case first, while the preimage is still unspent, so a
# rejection can only come from the method caveat and not from replay
# protection.
http_head -H "Authorization: L402 ${method_macaroon}:${method_preimage}" "${BASE_URL}/protected"
assert_status 401 "GET-bound token refused on HEAD"

http_request -X GET -H "Authorization: L402 ${method_macaroon}:${method_preimage}" "${BASE_URL}/protected"
assert_status 200 "GET-bound token accepted on GET"

# ------------------------------------------------------------- autodetect --

l402_log "== autodetect (macaroon only, no preimage)"

http_request -L "${BASE_URL}/protected-auto"
assert_status 402 "autodetect route challenge"
auto_macaroon="$(l402_macaroon)"
pay_invoice_get_preimage "$(l402_invoice)" >/dev/null

http_request -H "Authorization: L402 ${auto_macaroon}" "${BASE_URL}/protected-auto"
assert_status 200 "autodetect accepts a macaroon whose invoice was paid"

# ------------------------------------------------------------------ Cashu --

l402_log "== Cashu eCash acceptance"

# The wallet database is created by the module on first use; the tests below
# delete it deliberately, so make sure it is writable first.
docker exec nginx-lnd chmod 770 /app/data
docker exec nginx-lnd sh -c 'chmod 660 /app/data/cashu_tokens.db 2>/dev/null || true'

cashu_token="$(cashu_send 10)"

cashu_redeem "$cashu_token" "${BASE_URL}/protected"
assert_status 200 "valid Cashu token accepted"

l402_log "== Cashu tokens that must be refused"

insufficient_token="$(cashu_send 1)"
http_request -L -H "Authorization: Cashu ${insufficient_token}" "${BASE_URL}/protected"
assert_status 401 "token below the route price rejected"

malformed_token="$(printf '%s' '{"token":[{"mint":"http://cashu-mint:3338","proofs":[]}]}' | base64 -w 0)"
http_request -L -H "Authorization: Cashu ${malformed_token}" "${BASE_URL}/protected"
assert_status 401 "token with no proofs rejected"

non_whitelisted_token="$(printf '%s' '{"token":[{"mint":"https://nofees.testnut.cashu.space","proofs":[{"id":"test-proof","amount":10,"secret":"test-secret","C":"test-commitment"}]}]}' | base64 -w 0)"
http_request -L -H "Authorization: Cashu ${non_whitelisted_token}" "${BASE_URL}/protected"
assert_status 401 "token from a mint outside the whitelist rejected"

l402_log "== wallet state survives losing the database"

docker exec nginx-lnd rm -f \
  /app/data/cashu_tokens.db /app/data/cashu_tokens.db-shm /app/data/cashu_tokens.db-wal
docker compose restart nginx-lnd
wait_for_container_running nginx-lnd
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-lnd after restart"
docker exec nginx-lnd sh -c 'chmod 660 /app/data/cashu_tokens.db 2>/dev/null || true'

regenerated_token="$(cashu_send 15)"
cashu_redeem "$regenerated_token" "${BASE_URL}/protected"
assert_status 200 "tokens are accepted again after the database is rebuilt"

l402_log "== LND slice passed"
