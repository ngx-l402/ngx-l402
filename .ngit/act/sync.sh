#!/usr/bin/env sh
# Regenerate .ngit/act/workflows/ from .github/workflows/.
# Run after editing a GitHub workflow; lint.yml fails if you forget.
#
# .github/workflows/ is deliberately left alone. The one transform a Nostr run
# needs is made here: ngit-ci refuses a workflow file containing a job-level
# `uses:`, because it cannot verify the container declarations of a reusable
# workflow it has not fetched. Those jobs are dropped. They are all tag-gated
# and would never run on a proposal.

set -eu
cd "$(dirname "$0")/../.."

src=.github/workflows
out=.ngit/act/workflows
mkdir -p "$out"

# Drop every job whose body declares a job-level `uses:`, with the comments and
# blank lines that introduce it.
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
  cp "$src/$f" "$out/$f"
done
drop_reusable_jobs < "$src/tests.yml" > "$out/tests.yml"
