#!/usr/bin/env bash
# Slice 1 — LNURL, dry-run/metrics and the gRPC route.
#
# Ports the LNURL, "Dry-Run (Shadow) Mode & Metrics" and gRPC steps of
# .github/workflows/tests.yml. It needs only redis, nginx-lnurl and the demo
# gRPC backend, which makes it the cheapest slice to run and the one that
# proves the docker-in-job machinery works before any Lightning node is
# involved.
#
# Runnable by hand against any docker daemon:
#   .ngit/act/slices/lnurl.sh
set -euo pipefail

# Resolve the repo root from this file rather than from git: the slice is also
# run by hand and inside containers where no .git is present.
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh

export DIAG_CONTAINERS="nginx-lnurl grpc-content-server"
write_env_file

# ------------------------------------------------------------------ setup --

release_serving_port
l402_log "== bringing up redis, nginx-lnurl and the gRPC backend"
docker compose up -d --build redis nginx-lnurl grpc-content-server

wait_for_container_running nginx-lnurl
wait_for_container_running grpc-content-server

# nginx opens 8000 before the L402 module has its backend wired up, so the free
# route serving 200 is the first honest sign the service is up.
wait_for_http_status "${BASE_URL}/" 200 120 "nginx-lnurl"

# ------------------------------------------------------------------ LNURL --

l402_log "== LNURL: free and protected routes"

http_request -L "${BASE_URL}/"
assert_status 200 "free route"

http_request -L "${BASE_URL}/protected"
assert_status 402 "protected route without credentials"
assert_l402_challenge "protected route returns an L402 challenge"

# Replay protection stores spent preimages, so start the credential checks from
# a known-empty Redis.
flush_redis

# The valid-credentials (-> 200) case is deliberately absent: nginx-lnurl bills
# to hello@getalby.com, which cannot be paid from a regtest node. The LND, NWC
# and Eclair slices cover the full pay-then-redeem flow.
assert_rejects_forged_credentials "${BASE_URL}/protected"

l402_log "== LNURL: multi-tenant invoice generation"

http_request -L "${BASE_URL}/tenant1"
assert_status 402 "tenant1 route"
assert_l402_challenge "tenant1 returns an L402 challenge"
tenant1_invoice="$(l402_invoice)"
l402_log "tenant1 invoice: ${tenant1_invoice:0:50}..."

# --------------------------------------------------- dry-run and metrics ---

l402_log "== dry-run (shadow) mode"

# nginx runs worker_processes auto and the counters are per worker, so spread
# the warm-up across workers before scraping.
for _ in 1 2 3 4 5; do
  curl -s -o /dev/null --max-time "$HTTP_TIMEOUT" -H "Connection: close" "${BASE_URL}/shadow" || true
done

http_request -H "Connection: close" "${BASE_URL}/shadow"
assert_status 200 "shadow route passes traffic through"
assert_response_matches '^X-L402-Dry-Run: 1' "X-L402-Dry-Run header set"
assert_response_matches '^X-L402-Dry-Run-Price-Msat: 10000' "dry-run price advertised"

# Challenge synthesis reaches the LN backend, which is an external LNURL
# address here; its absence is information, not a failure.
if response_matches '^X-L402-Dry-Run-Challenge:'; then
  l402_log "INFO: dry-run challenge header present (LN backend reachable)"
else
  l402_log "INFO: dry-run challenge header absent (LN backend unreachable — non-fatal)"
fi

l402_log "== metrics endpoint"

# Each scrape hits one worker, and only workers that served /shadow have a
# non-zero counter, so poll until one of them answers.
dry_run_total=""
for _ in $(seq 1 10); do
  http_request -H "Connection: close" "${BASE_URL}/metrics"
  if [ "$HTTP_STATUS" = "200" ]; then
    dry_run_total="$(awk '/^l402_dry_run_requests_total / {print $2}' <<< "$HTTP_RESPONSE")"
    [ -n "$dry_run_total" ] && [ "$dry_run_total" -ge 1 ] 2>/dev/null && break
  fi
  sleep 1
done

assert_status 200 "metrics endpoint"
assert_response_matches '^Content-Type: text/plain' "metrics served as text/plain"
for counter in l402_requests_total l402_dry_run_requests_total l402_dry_run_would_block_total; do
  assert_response_matches "^${counter} " "counter ${counter} exposed"
done

if [ -z "$dry_run_total" ] || [ "$dry_run_total" -lt 1 ]; then
  fail "l402_dry_run_requests_total=${dry_run_total:-unset}, expected at least 1"
fi
pass "l402_dry_run_requests_total=${dry_run_total}"

# ------------------------------------------------------------------- gRPC --

l402_log "== gRPC backend"

install_grpcurl() {
  command -v grpcurl >/dev/null 2>&1 && return 0
  local version="1.8.9" arch
  case "$(uname -m)" in
    x86_64)  arch="x86_64" ;;
    aarch64) arch="arm64" ;;
    *) fail "no grpcurl build for $(uname -m)" ;;
  esac
  curl -fsSL --retry 3 \
    "https://github.com/fullstorydev/grpcurl/releases/download/v${version}/grpcurl_${version}_linux_${arch}.tar.gz" \
    | tar -xzf - -C /usr/local/bin grpcurl
}
install_grpcurl

# The backend is published on loopback only, matching the compose file.
wait_for 60 "gRPC reflection" grpcurl -plaintext localhost:50051 list \
  || fail "gRPC server never became reachable on localhost:50051"

grpc_expect() {
  local method="$1" path="$2" expected="$3" response
  response="$(grpcurl -plaintext -d "{\"path\": \"${path}\"}" localhost:50051 "content.ContentService/${method}" 2>&1)"
  grep -q -- "$expected" <<< "$response" \
    || { HTTP_RESPONSE="$response"; fail "${method}(${path}) did not return /${expected}/"; }
  pass "${method}(${path})"
}

grpc_expect GetContent /test.txt "Content for path"
grpc_expect GetProtectedContent /protected "This is Protected Content"
grpc_expect GetFreeContent /free "Free Content"

l402_log "== gRPC through the L402-protected nginx route"

http_request -H "Content-Type: application/grpc" -H "grpc-timeout: 60S" \
  -X POST "${BASE_URL}/content.ContentService/GetProtectedContent"
assert_status 402 "gRPC route without credentials"
assert_l402_challenge "gRPC route returns an L402 challenge"

# As with /protected above, the paid path is not exercised here: the LNURL
# address cannot be paid from regtest.

l402_log "== LNURL slice passed"
