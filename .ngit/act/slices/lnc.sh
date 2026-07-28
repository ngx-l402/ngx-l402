#!/usr/bin/env bash
# Slice 6 — Lightning Node Connect: pairing with a Lightning Terminal session
# and generating an invoice through the LNC tunnel.
#
# Ports "Stop nginx-lnurl and start litd", "Generate LNC Pairing Phrase",
# "Start nginx-lnc" and "Run Integration Tests - LNC" from
# .github/workflows/tests.yml.
#
# External dependency: LNC pairing runs through a public mailbox server
# (LNC_MAILBOX_SERVER, mailbox.terminal.lightning.today by default). A failure
# to pair is as likely to be that server as it is to be the module.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-lnc litd lndnode redis"
write_env_file

setup_lightning_backbone

# ------------------------------------------------------------------ litd ---

l402_log "== starting Lightning Terminal"
docker compose up -d --no-deps litd
wait_for_container_running litd

# Readiness is checked over litcli, not HTTP. Despite
# `--insecure-httplisten=0.0.0.0:8080` in docker-compose.yml, this image binds
# its web interface on 127.0.0.1:8443 with TLS, so the published port maps to
# nothing and no HTTP probe from outside the container can ever succeed. The
# GitHub original polls that port from *inside* the container, where nothing
# listens either, and warns instead of failing — so it never noticed.
#
# litcli is also the honest check: it is what the next step uses.
_litd_ready() { docker exec litd litcli --network=regtest status >/dev/null 2>&1; }
wait_for 300 "litd RPC" _litd_ready || fail "litd never answered litcli"

l402_log "== creating an LNC session"

pairing_phrase=""
for attempt in $(seq 1 10); do
  session_output="$(docker exec litd litcli --network=regtest sessions add \
    --label="ci_test_${attempt}" --type=admin 2>&1 || true)"

  if grep -q "pairing_secret_mnemonic" <<< "$session_output"; then
    pairing_phrase="$(grep "pairing_secret_mnemonic" <<< "$session_output" \
      | sed 's/.*"pairing_secret_mnemonic"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' \
      | tr -d '\n' || true)"

    # The mnemonic is exactly ten words; a shorter capture means the sed above
    # matched a truncated line and the phrase would fail to pair.
    word_count="$(wc -w <<< "$pairing_phrase" | tr -d ' ')"
    if [ "$word_count" = "10" ]; then
      pass "pairing phrase generated (10 words)"
      break
    fi
    l402_log "got ${word_count} words instead of 10; retrying"
    pairing_phrase=""
  fi
  sleep 3
done

if [ -z "$pairing_phrase" ]; then
  docker logs --tail 100 litd >&2 || true
  fail "litd never produced a usable pairing phrase"
fi

LNC_PAIRING_PHRASE="$pairing_phrase"
export LNC_PAIRING_PHRASE
write_env_file

# ------------------------------------------------------------- nginx-lnc ---

l402_log "== starting nginx-lnc"
docker compose up -d --build --no-deps nginx-lnc redis
wait_for_container_running nginx-lnc

# Pairing goes out to the mailbox server before the module can serve anything,
# so this waits longer than the other slices.
wait_for_http_status "${BASE_URL}/" 200 300 "nginx-lnc"

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

l402_log "== invoice generation over LNC"

http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route without credentials"
assert_l402_challenge "LNC issues an L402 challenge"

invoice="$(l402_invoice)"
case "$invoice" in
  lnbcrt*|lnbc*|lntb*) pass "challenge carries a BOLT11 invoice (${invoice:0:12}...)" ;;
  *) HTTP_RESPONSE="invoice: ${invoice}"; fail "challenge value is not a BOLT11 invoice" ;;
esac

# Decoding is a sanity check on the invoice the tunnel produced, not a gate:
# it is the same node behind litd, so a decode failure here would be an lncli
# problem rather than a module one.
if decoded="$(lnd_cli decodepayreq "$invoice" 2>&1)" && grep -q num_msat <<< "$decoded"; then
  l402_log "invoice decodes to $(printf '%s' "$decoded" | jq -r '.num_msat') msat"
else
  l402_log "INFO: could not decode the invoice locally (non-fatal): ${decoded}"
fi

l402_log "== LNC slice passed"
