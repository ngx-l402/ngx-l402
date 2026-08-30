#!/usr/bin/env sh
# Regenerate .ngit/act/workflows/ from .github/workflows/.
# Run after editing a GitHub workflow; lint.yml fails if you forget.
#
# .github/workflows/ is deliberately left alone, so the two transforms a Nostr
# run needs are made here.

set -eu
cd "$(dirname "$0")/../.."

src=.github/workflows
out=.ngit/act/workflows
mkdir -p "$out"

# 1. Unfilter the pull_request trigger.
#
# GitHub reads `pull_request: branches: [main]` as "PRs *targeting* main".
# ngit-ci matches it against the proposal's own `branch-name` tag -- the source
# branch -- so a proposal from `fix/whatever` matches nothing and the workflow
# silently never runs, with no error anywhere. Dropping the filter is the
# reading that preserves the intent: on GitHub these run for every PR to main,
# and on Nostr `main` is the only thing a proposal ever targets.
#
# `push: branches:` is left alone -- there the ref really is the ref.
unfilter_pull_request() {
  awk '
    /^  pull_request:[[:space:]]*$/ { print; skipping = 1; next }
    skipping && /^    / { next }
    { skipping = 0; print }
  '
}

# 2. Serialise matrix jobs.
#
# On GitHub each matrix job gets its own runner and its own Docker daemon, so
# five nginx versions build in parallel and share nothing. Here they share the
# sandbox's one daemon -- and therefore one BuildKit cache mount. Run together
# they corrupt each other's cargo registry unpack (`File exists (os error 17)`)
# and publish to the same host ports. Measured: two of five survived.
#
# max-parallel is the smallest change that restores the isolation the workflow
# was written against. It costs wall-clock, not correctness.
serialise_matrix() {
  awk '
    /^    strategy:[[:space:]]*$/ { print; print "      max-parallel: 1"; next }
    { print }
  '
}

# 3. Drop every job whose body declares a job-level `uses:`, with the comments
# and blank lines that introduce it.
#
# ngit-ci refuses a workflow file containing one, because it cannot verify the
# container declarations of a reusable workflow it has not fetched. In
# tests.yml they are the tag-gated release fan-out, which never runs on a
# proposal.
drop_reusable_jobs() {
  awk '
    !injobs { print; if ($0 ~ /^jobs:[[:space:]]*$/) injobs = 1; next }
    /^  [A-Za-z0-9_.-]+:[[:space:]]*$/ {
      flush(); buf = pre $0 "\n"; pre = ""; uses = 0; next
    }
    /^[[:space:]]*(#|$)/ { pre = pre $0 "\n"; next }
    { buf = buf pre $0 "\n"; pre = ""; if ($0 ~ /^    uses:/) uses = 1 }
    END { flush() }
    function flush() { if (buf != "" && !uses) printf "%s", buf; buf = "" }
  '
}

for f in nginx-compat.yml lint.yml audit.yml; do
  unfilter_pull_request < "$src/$f" | serialise_matrix > "$out/$f"
done
unfilter_pull_request < "$src/tests.yml" | serialise_matrix | drop_reusable_jobs \
  > "$out/tests.yml"
