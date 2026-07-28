#!/usr/bin/env bash
# Runs only when a slice failed. The lease is destroyed when the job ends, so
# anything not printed here is gone: this is the last chance to see what the
# stack looked like.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || exit 1

section() { printf '\n========== %s ==========\n' "$*"; }

section "containers"
docker ps -a 2>&1 || true

section "compose services"
docker compose ps 2>&1 || true

section "disk"
df -h /var/lib/docker / 2>&1 || true

section "dockerd log (last 40 lines)"
tail -40 /var/log/dockerd.log 2>&1 || true

# Every container that ran, not just the ones the slice named: a failure in
# nginx is often caused by the node behind it.
for container in $(docker ps -aq 2>/dev/null); do
  name="$(docker inspect -f '{{.Name}}' "$container" 2>/dev/null | tr -d '/')"
  section "logs: ${name:-$container}"
  docker logs --tail 120 "$container" 2>&1 || true
done
