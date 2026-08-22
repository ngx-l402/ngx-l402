#!/usr/bin/env sh
# Regenerate .ngit/act/workflows/ from .github/workflows/.
# Run after editing a GitHub workflow; lint.yml fails if you forget.
#
# .github/workflows/ is deliberately left alone, so the two transforms a Nostr
# run needs live here instead:
#
#   1. act job containers get no docker daemon, and this suite reaches its
#      services over published ports, so docker-using workflows gain a step
#      that starts one inside the job.
#   2. ngit-ci refuses a workflow file containing a job-level `uses:` -- it
#      cannot verify the container declarations of a reusable workflow it has
#      not fetched -- so those jobs are dropped. They are all tag-gated and
#      would never run on a proposal.

set -eu
cd "$(dirname "$0")/../.."

src=.github/workflows
out=.ngit/act/workflows
mkdir -p "$out"

# Insert the daemon bootstrap after the checkout step.
add_docker_step() {
  awk '
    { print }
    !done && /uses: actions\/checkout@/ {
      print ""
      print "      - name: Start a Docker daemon inside the job"
      print "        run: bash .ngit/act/lib/docker-in-job.sh"
      done = 1
    }
  '
}

# Drop every job whose body declares a job-level `uses:`, with the comments
# and blank lines that introduce it.
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

cp "$src/lint.yml"  "$out/lint.yml"
cp "$src/audit.yml" "$out/audit.yml"
add_docker_step < "$src/nginx-compat.yml" > "$out/nginx-compat.yml"
add_docker_step < "$src/tests.yml" | drop_reusable_jobs > "$out/tests.yml"
