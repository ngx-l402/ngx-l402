#!/usr/bin/env bash
# Slice 8 — nginx version compatibility.
#
# Ports .github/workflows/nginx-compat.yml, which is a parallel matrix on
# GitHub. Here every version shares one job on one lease, because a workflow
# file is a leased sandbox and five files would mean five sandboxes each paying
# provisioning, image pulls and a full dependency compile.
#
# Usage:
#   nginx-compat.sh              every supported version, in one job on one lease
#   nginx-compat.sh 1.29.8       one version
#
# Run together they share one daemon, so the Dockerfile's cargo cache mount is
# reused: the ~400 dependency crates compile once and each later version only
# rebuilds what actually depends on the nginx headers.
#
# The version list matches the matrix in .github/workflows/nginx-compat.yml.
# When that changes, change it here — the two gates disagreeing is worse than
# either being wrong.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh

VERSIONS=(1.28.0 1.28.3 1.29.8 1.30.3 1.31.2)

# No argument: recurse once per version, reporting a summary rather than
# stopping at the first failure — a lease is paid for, so finding two broken
# versions in one run beats finding one and buying another sandbox.
if [ "$#" -eq 0 ]; then
  passed=(); failed=()
  for version in "${VERSIONS[@]}"; do
    printf '\n\n════════════════════════════════════════════════════════════\n'
    printf '  nginx %s\n' "$version"
    printf '════════════════════════════════════════════════════════════\n\n'
    if bash "${BASH_SOURCE[0]}" "$version"; then passed+=("$version"); else
      failed+=("$version")
      printf '\n!! nginx %s failed — continuing to the next version\n' "$version"
    fi
  done
  printf '\npassed (%d): %s\nfailed (%d): %s\n' \
    "${#passed[@]}" "${passed[*]:-none}" "${#failed[@]}" "${failed[*]:-none}"
  [ "${#failed[@]}" -eq 0 ] || exit 1
  echo "the module builds and serves on every supported nginx version"
  exit 0
fi

set -e
NGX_VERSION="$1"
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
