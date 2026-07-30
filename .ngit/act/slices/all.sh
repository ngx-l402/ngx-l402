#!/usr/bin/env bash
# Every integration slice, in one job, on one lease.
#
# This exists because of how ngit-ci leases compute: a WorkflowPlan is per
# *workflow file*, so one file is one dispatch is one sandbox. Giving each
# coverage area its own file — which is what this directory did first — made
# every area buy its own sandbox and re-pay provisioning, image pulls and a
# full ~400-crate compile. On a 4 vCPU box that is hours.
#
# Run together, the expensive parts happen once: one daemon, one image build,
# one funded regtest backbone. Each slice then costs only its own services and
# assertions. The slice scripts are unchanged and still run individually.
#
# The reason this is a single job rather than several jobs in one workflow: act
# gives each job its own container, so a per-job docker daemon would take the
# image cache with it when the job ends. Sharing the cache means sharing the
# job. When ngit-ci publishes per-job results (kind 9841 — the plan already
# carries `job_ids` for it), splitting this back out becomes worthwhile.
#
# Order is not arbitrary: cashu-p2pk reconfigures nginx-lnd into P2PK mode and
# does not put it back, so it runs last.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || exit 1

SLICES=(lnurl lnd cashu-redis cln nwc lnc eclair cashu-p2pk)

passed=()
failed=()

for slice in "${SLICES[@]}"; do
  printf '\n\n════════════════════════════════════════════════════════════\n'
  printf '  slice: %s\n' "$slice"
  printf '════════════════════════════════════════════════════════════\n\n'

  if bash ".ngit/act/slices/${slice}.sh"; then
    passed+=("$slice")
  else
    failed+=("$slice")
    # Deliberately not fail-fast. A lease is paid for and the feedback loop is
    # ~20 minutes, so finding four problems in one run beats finding one and
    # buying another sandbox to find the next. The cost is that a slice which
    # leaves the stack broken can take later ones down with it, so when several
    # fail, trust the first.
    printf '\n!! slice %s failed — continuing so the rest of the run still reports\n' "$slice"
  fi
done

printf '\n\n════════════════════════════════════════════════════════════\n'
printf '  summary\n'
printf '════════════════════════════════════════════════════════════\n'
printf 'passed (%d): %s\n' "${#passed[@]}" "${passed[*]:-none}"
printf 'failed (%d): %s\n' "${#failed[@]}" "${failed[*]:-none}"

[ "${#failed[@]}" -eq 0 ] || exit 1
echo "all integration slices passed"
