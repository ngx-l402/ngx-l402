#!/usr/bin/env bash
# Assertions and waits shared by the integration slices.
#
# The GitHub suite repeats the same twelve-line curl-and-check block dozens of
# times, which is why a wrong expectation there tends to be copied rather than
# fixed. Here each check exists once and every failure prints the response and
# the logs of whichever containers the slice named in DIAG_CONTAINERS.
#
# The other difference from the GitHub original is that nothing sleeps a fixed
# number of seconds waiting for a service. `sleep 15` is either slower than it
# needs to be or too short on a loaded box, and on a paid lease it is both. The
# waits below poll for the condition they actually care about.

# shellcheck shell=bash

# 60s, not the 30 most of the GitHub suite uses: its Cashu calls are the slow
# ones and it gives those --max-time 60 individually. A redemption that has to
# fetch mint info first goes past 30s, and curl then reports status 000, which
# reads as "the module returned nothing" rather than "we did not wait".
HTTP_TIMEOUT="${HTTP_TIMEOUT:-60}"
DIAG_CONTAINERS="${DIAG_CONTAINERS:-}"

# Set by http_request, read by the assertions.
HTTP_RESPONSE=""
HTTP_STATUS=""

l402_log() { printf '%s\n' "$*"; }

# Everything a failing assertion knows: the request that failed, what came
# back, and what the services thought they were doing at the time.
dump_diagnostics() {
  local container
  if [ -n "$HTTP_RESPONSE" ]; then
    printf -- '--- response ---\n%s\n--- end response ---\n' "$HTTP_RESPONSE" >&2
  fi
  for container in $DIAG_CONTAINERS; do
    printf -- '--- docker logs %s (last 80 lines) ---\n' "$container" >&2
    docker logs --tail 80 "$container" 2>&1 >&2 || true
  done
}

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  dump_diagnostics
  exit 1
}

pass() { printf 'PASS: %s\n' "$*"; }

# curl reports 000 when it got no answer at all — connection refused, or the
# timeout expired. No assertion in this suite ever wants that: every check is
# about *which* status came back. So a 000 is retried rather than reported,
# which matters most on the LNURL routes, where synthesising the challenge
# means an outbound call to a third-party lightning address that occasionally
# takes longer than the timeout.
_http_attempt() {
  local attempt
  for attempt in 1 2 3; do
    HTTP_RESPONSE="$(curl "$@" || true)"
    HTTP_STATUS="$(tail -n1 <<< "$HTTP_RESPONSE")"
    [ "$HTTP_STATUS" != "000" ] && return 0
    [ "$attempt" -lt 3 ] || break
    l402_log "no response (attempt ${attempt}/3); retrying"
    sleep 5
  done
}

# Issue a request, keeping headers so the L402 challenge can be inspected.
# Usage: http_request <curl args...>
http_request() {
  _http_attempt -s -i -w '\n%{http_code}' --max-time "$HTTP_TIMEOUT" "$@"
}

# curl -I, for the cross-method replay checks. `-X HEAD` makes curl wait for a
# body that never arrives and hang until the timeout instead.
http_head() {
  _http_attempt -s -i -I -w '\n%{http_code}' --max-time "$HTTP_TIMEOUT" "$@"
}

assert_status() {
  local expected="$1" what="$2"
  [ -n "$HTTP_STATUS" ] || fail "${what}: no status captured (request never completed)"
  [ "$HTTP_STATUS" = "$expected" ] || fail "${what}: got HTTP ${HTTP_STATUS}, expected ${expected}"
  pass "${what} -> ${expected}"
}

# Matching is done with a here-string rather than `printf ... | grep -q`.
# Under `set -o pipefail` that pipeline reports failure exactly when it
# succeeds: grep exits the moment it finds the pattern, printf then dies of
# SIGPIPE, and its 141 becomes the pipeline's status. With a 76 KB challenge
# page the race is real and the assertion fails only sometimes — which is the
# worst way for it to be wrong.
response_matches() {
  grep -qi -- "$1" <<< "$HTTP_RESPONSE"
}

assert_response_matches() {
  local pattern="$1" what="$2"
  response_matches "$pattern" || fail "${what}: response does not match /${pattern}/"
  pass "$what"
}

assert_response_not_matches() {
  local pattern="$1" what="$2"
  ! response_matches "$pattern" || fail "${what}: response unexpectedly matches /${pattern}/"
  pass "$what"
}

assert_l402_challenge() {
  local what="${1:-L402 challenge}"
  assert_response_matches 'WWW-Authenticate: L402 macaroon=' "$what"
}

# Both values come out of the same WWW-Authenticate header; an empty result
# means the challenge was malformed, which is a failure rather than a value.
#
# `|| true` on each stage because a missing field must reach the caller as an
# empty string, not as a `set -e` abort three lines before the error message
# that would have explained it.
l402_field() {
  local field="$1" header
  header="$(grep -i 'WWW-Authenticate: L402' <<< "$HTTP_RESPONSE" || true)"
  grep -o "${field}=\"[^\"]*\"" <<< "$header" | cut -d'"' -f2 || true
}

l402_invoice() {
  local invoice
  invoice="$(l402_field invoice || true)"
  [ -n "$invoice" ] || fail "no invoice in the L402 challenge"
  printf '%s' "$invoice"
}

l402_macaroon() {
  local macaroon
  macaroon="$(l402_field macaroon || true)"
  [ -n "$macaroon" ] || fail "no macaroon in the L402 challenge"
  printf '%s' "$macaroon"
}

# Credentials that must always be rejected, whatever the backend. Every slice
# runs these, so they live here rather than being pasted per backend.
readonly L402_WRONG_KEY_TOKEN='MDAxMmxvY2F0aW9uIEw0MDIKMDAzMGlkZW50aWZpZXIgM460twjJAuVrQN-u5JPUZ0aKNWevybkbveRc2DeF2ZAKMDAyMWNpZCBSZXF1ZXN0UGF0aCA9IC9wcm90ZWN0ZWQKMDAyZnNpZ25hdHVyZSCwR6G2lDj1thda81BPwQuo73_shURzPf1XOwuejNLwVwo=:fbe9ac25c04e14b10177514e2d57b0e39224e70277ac1a2cd23c28e58cd4ea35'
readonly L402_NO_CAVEATS_TOKEN='AgEETFNBVALmAUr/gQMBARJNYWNhcm9vbklkZW50aWZpZXIB/4IAAQMBB1ZlcnNpb24BBgABC1BheW1lbnRIYXNoAf+EAAEHVG9rZW5JZAH/hgAAABT/gwEBAQRIYXNoAf+EAAEGAUAAABn/hQEBAQlbMzJddWludDgB/4YAAQYBQAAAa/+CAiD/pv/jOjY1/9oC/4z/tHb/qf/2Jf+d/4H/u/+YGHj/+/+O/8D/v/+P/8X/qRL/5v/x/4r/tkIBIA1Y/8j/pR3/0P+b/7cwWP+W/87/sD18GP//Hf/f/9Aj//NcBFs2/9VhNEUF/70AAAAGIDlR1jVm5IfEJgvuSQoJLqLg4FcW4Ib1vW8sbkRHdUWX:651505fae9ea341c770c6ebef207d8560d546eb3aee26985e584c15d1c987875'

# A backend that accepted either of these would be accepting a forged macaroon,
# so both belong in every slice regardless of which node issues invoices.
assert_rejects_forged_credentials() {
  local url="$1"

  http_request -H "Authorization: L402 ${L402_WRONG_KEY_TOKEN}" "$url"
  assert_status 401 "wrong-key macaroon rejected"

  http_request -H "Authorization: L402 ${L402_NO_CAVEATS_TOKEN}" "$url"
  assert_status 401 "caveat-less macaroon rejected"
}

# Redeem a Cashu token where success is expected, retrying a few times.
#
# The first redemption after a container starts also warms the wallet's
# connection to the mint, and until that settles the module answers 401 or the
# request simply takes too long. Neither is a verdict on the token, so the
# caller asserts on the status *after* this returns.
#
# Cases that expect a rejection use http_request directly — retrying those
# would only hide a token that was accepted on the second try.
cashu_redeem() {
  local token="$1" url="$2" attempt
  for attempt in 1 2 3; do
    http_request -L -H "Authorization: Cashu ${token}" "$url"
    [ "$HTTP_STATUS" = "200" ] && return 0
    l402_log "Cashu redemption attempt ${attempt} returned ${HTTP_STATUS}; retrying"
    sleep 5
  done
  return 0
}

# ---------------------------------------------------------------- waits ----

# Poll until `cmd` succeeds. Returns 1 on timeout so callers can decide whether
# that is fatal — some of the ported steps treat a missing optional service as
# a skip rather than a failure.
wait_for() {
  local timeout="$1" what="$2"; shift 2
  local deadline=$((SECONDS + timeout))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if "$@" >/dev/null 2>&1; then
      pass "${what} ready after $((timeout - (deadline - SECONDS)))s"
      return 0
    fi
    sleep 2
  done
  printf 'TIMEOUT: %s not ready after %ss\n' "$what" "$timeout" >&2
  return 1
}

# nginx binds its port before the module has finished wiring up its backend, so
# "the port is open" is not the same as "the service works". Wait for a status
# code the route can only produce once it is actually serving.
wait_for_http_status() {
  local url="$1" expected="$2" timeout="${3:-120}"
  local what="${4:-$url}"
  local deadline=$((SECONDS + timeout)) code=""
  while [ "$SECONDS" -lt "$deadline" ]; do
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$url" || true)"
    if [ "$code" = "$expected" ]; then
      pass "${what} serving ${expected}"
      return 0
    fi
    sleep 2
  done
  HTTP_RESPONSE="last status: ${code:-none}"
  fail "${what} never returned ${expected} within ${timeout}s"
}

wait_for_container_running() {
  local name="$1" timeout="${2:-60}"
  wait_for "$timeout" "container ${name}" \
    docker inspect -f '{{.State.Running}}' "$name" \
    || fail "container ${name} is not running"
}

# Redis carries replay state between checks, so slices flush it at the points
# where the GitHub suite does. Wrapped so a slice that has no redis (nginx
# compatibility matrix) can call it harmlessly.
flush_redis() {
  docker exec redis redis-cli flushall >/dev/null 2>&1 || true
}

# Exactly one nginx flavour may hold port 8000, which is why the GitHub suite
# stops the previous one before starting the next. A slice run on its own has
# nothing to stop; a slice run after another in the same lease does, and
# without this the second one dies on "port is already allocated".
#
# Named for what it guarantees rather than what it stops, so adding an nginx
# service later means adding it to one list.
release_serving_port() {
  docker compose stop \
    nginx-lnurl nginx-lnd nginx-cln nginx-bolt12 nginx-nwc nginx-lnc nginx-eclair \
    >/dev/null 2>&1 || true
}
