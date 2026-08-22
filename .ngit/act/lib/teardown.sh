#!/usr/bin/env bash
# Stop the stack and release the volumes.
#
# The lease is destroyed either way, so this is not about reclaiming the
# sandbox — it is about the daemon shutting down cleanly enough that a stuck
# container cannot hold the job open until the coordinator's timeout fires and
# turns a passing run into a failed one.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || exit 1

# shellcheck source=./env.sh
source .ngit/act/lib/env.sh

timeout 180 docker compose down -v --remove-orphans 2>&1 || {
  echo "compose down did not finish in 180s; killing what is left"
  docker ps -q | xargs -r timeout 60 docker kill 2>&1 || true
}

echo "teardown complete"
