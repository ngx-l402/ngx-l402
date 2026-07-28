#!/usr/bin/env bash
# Slice 8 — nginx version compatibility.
#
# Ports .github/workflows/nginx-compat.yml, which is a matrix on GitHub. Here
# each version is its own workflow file instead: the coordinator runs one job
# at a time, and five full module builds in a single job would run past the
# 3600s job timeout and take the lease with it.
#
# Usage: nginx-compat.sh <nginx-version>
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh

NGX_VERSION="${1:?usage: nginx-compat.sh <nginx-version>}"
IMAGE="ngx_l402:nginx-${NGX_VERSION}"
CONTAINER="smoke-test-${NGX_VERSION}"

export DIAG_CONTAINERS="$CONTAINER"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

l402_log "== building the module against nginx ${NGX_VERSION}"
docker build --build-arg "NGX_VERSION=${NGX_VERSION}" -t "$IMAGE" .

l402_log "== the module loads"
# No --add-host: the published image has to start without the demo gRPC
# backend being resolvable, which is what the variable-form grpc_pass in
# nginx.conf exists for.
docker run --rm \
  -e LN_CLIENT_TYPE=LNURL \
  -e "LNURL_ADDRESS=${LNURL_ADDRESS}" \
  -e "ROOT_KEY=${ROOT_KEY}" \
  "$IMAGE" nginx -t

l402_log "== the .so is where nginx.conf expects it"
docker run --rm "$IMAGE" ls -lh /etc/nginx/modules/libngx_l402_lib.so

l402_log "== smoke test"
docker run -d --name "$CONTAINER" \
  -p 8000:8000 \
  -e LN_CLIENT_TYPE=LNURL \
  -e "LNURL_ADDRESS=${LNURL_ADDRESS}" \
  -e "ROOT_KEY=${ROOT_KEY}" \
  "$IMAGE" >/dev/null

wait_for_http_status "${BASE_URL}/" 200 120 "nginx ${NGX_VERSION}"

http_request "${BASE_URL}/"
assert_status 200 "free route on nginx ${NGX_VERSION}"

http_request "${BASE_URL}/protected"
assert_status 402 "protected route on nginx ${NGX_VERSION}"

l402_log "== nginx ${NGX_VERSION} compatibility passed"
