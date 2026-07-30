#!/usr/bin/env bash
# Every supported nginx version, in one job on one lease.
#
# Five workflow files meant five sandboxes and five full dependency compiles.
# Run together they share one daemon, so the Dockerfile's cargo cache mount is
# reused: the ~400 dependency crates compile once and each subsequent version
# only rebuilds what actually depends on the nginx headers.
#
# Versions match the matrix in .github/workflows/nginx-compat.yml. When that
# list changes, change it here — the two gates disagreeing is worse than either
# being wrong.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || exit 1

VERSIONS=(1.28.0 1.28.3 1.29.8 1.30.3 1.31.2)

passed=()
failed=()

for version in "${VERSIONS[@]}"; do
  printf '\n\n════════════════════════════════════════════════════════════\n'
  printf '  nginx %s\n' "$version"
  printf '════════════════════════════════════════════════════════════\n\n'

  if bash .ngit/act/slices/nginx-compat.sh "$version"; then
    passed+=("$version")
  else
    failed+=("$version")
    printf '\n!! nginx %s failed — continuing to the next version\n' "$version"
  fi
done

printf '\n\n════════════════════════════════════════════════════════════\n'
printf 'passed (%d): %s\n' "${#passed[@]}" "${passed[*]:-none}"
printf 'failed (%d): %s\n' "${#failed[@]}" "${failed[*]:-none}"

[ "${#failed[@]}" -eq 0 ] || exit 1
echo "the module builds and serves on every supported nginx version"
