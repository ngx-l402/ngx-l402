# Nostr-native CI

This directory is how `ngx_l402` gets CI without GitHub. An
[ngit-ci](https://ngit.dev) coordinator watches the repo's Nostr events and, for
each proposal, runs the workflows below on a Linux sandbox rented by the second
from a [paygress](https://paygress.net) provider and paid for with Cashu ecash.
Results are published back to Nostr as kind 9841/9842 events.

## The one rule

**Nothing in `workflows/` is edited by hand.** It is generated from
`.github/workflows/` by:

    ./.ngit/act/sync.sh

Edit the GitHub workflow, run that, commit both. `lint.yml` regenerates and
fails on both pipelines if you forget, and prints this command.

Both pipelines therefore run the same workflow definitions. There is no second
copy of any test to keep in sync — that is the whole point of the arrangement,
and it is why a proposal cannot go green on Nostr and red on GitHub.

## Layout

    sync.sh                       regenerates workflows/ from .github/workflows/
    workflows/tests.yml           the integration suite
    workflows/nginx-compat.yml    the nginx version matrix
    workflows/lint.yml            fmt, clippy, and the sync check
    workflows/audit.yml           cargo audit

That is the whole directory. No shell, no Docker bootstrap, no ported test
logic.

Every `.github/workflows/*.yml` is mirrored except `docs.yml`,
`ghcr-workflow.yml` and `release-workflow.yml`, which publish to GitHub Pages,
GHCR and GitHub Releases. Add a workflow and it is mirrored by default; leaving
one out is an edit to the exclusion list at the top of `sync.sh`.

## The three transforms sync.sh applies

**1. It unfilters the `pull_request` trigger.** ngit-ci matches
`pull_request: branches: [main]` against the proposal's own `branch-name` tag —
the *source* branch — where GitHub reads it as the *target*. So a proposal from
`fix/whatever` matches nothing and the workflow silently never runs, with no
error anywhere. On Nostr `main` is the only thing a proposal targets, so
dropping the filter preserves the intent. Worth fixing upstream.

**2. It inserts `max-parallel: 1` under every `strategy:`.** On GitHub each
matrix job gets its own runner and its own Docker daemon. Here they share the
sandbox's one daemon, and therefore one BuildKit cache mount: run together they
corrupt each other's cargo registry unpack and publish to the same host ports.
Measured, two of `nginx-compat.yml`'s five survived. This is why that workflow
takes roughly 5× longer on Nostr than on GitHub — it costs wall-clock, not
correctness.

**3. It drops every job that declares a job-level `uses:`.** ngit-ci refuses a
workflow file containing one, because it cannot statically verify the container
declarations of a reusable workflow it has not fetched. In `tests.yml` that is
the tag-gated release fan-out — `publish`, `publish-image` and `publish-docs` —
all gated on `refs/tags/v*` and never run on a proposal.

Apart from those three, the mirrors are byte-for-byte copies.

## Docker comes from the sandbox

A GitHub runner hands the job a Docker daemon. So does a paygress sandbox — the
coordinator passes act the sandbox's own socket:

    --act-container-daemon-socket unix:///var/run/docker.sock

`paygress-cli ci up` sets that, along with `--runner socket-adapter` and
`--adapter-socket`. Nothing here has to know about it, which is why this
directory carries no bootstrap and nothing runs privileged.

The provider must advertise the `docker` capability. `ci up` requires it before
spending anything, so a provider that cannot serve CI costs a search rather than
a lease.

## Running it

    paygress-cli ci up --repo naddr1... --provider <name> --mint https://...

Any workflow also runs locally under [act](https://github.com/nektos/act), which
is what the coordinator does:

    act -W .github/workflows/tests.yml

The sandbox is x86, and two images are not portable to arm64: CLN's compose
entrypoint downloads an `x86_64-linux-gnu` nip47 plugin, and
`acinq/eclair:release-0.8.0` publishes no arm64 build. On an Apple Silicon Mac,
register qemu and pin those two services — binfmt alone is not enough, since an
arm64 CLN container has no x86 loader for the plugin and takes the node down
with it:

    docker run --privileged --rm tonistiigi/binfmt --install amd64
    printf 'services:\n  cln:\n    platform: linux/amd64\n  eclair:\n    platform: linux/amd64\n' > /tmp/arm64.yml
    export COMPOSE_FILE=docker-compose.yml:/tmp/arm64.yml

## Cost

One workflow file is one WorkflowPlan is one dispatch is **one leased sandbox**,
each starting from a clean box and recompiling ~400 crates. Four files is four
leases. `tests.yml` alone has a median duration of 35 minutes on GitHub with a
warm cache, and will be longer cold.

That is the number to watch when adding a workflow file: on GitHub a fifth file
is free parallelism, here it is a fifth sandbox. Prefer a new job or step in an
existing file.
