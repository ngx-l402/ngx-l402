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

`.github/workflows/` is otherwise untouched, so the GitHub pipeline keeps its
own shape. The two adjustments a Nostr run needs are made by `sync.sh`.

Both pipelines therefore run the same workflow definitions. There is no second
copy of any test to keep in sync — that is the whole point of the arrangement,
and it is why a proposal cannot go green on Nostr and red on GitHub.

## Layout

    sync.sh                       regenerates workflows/ from .github/workflows/
    workflows/tests.yml           the integration suite
    workflows/nginx-compat.yml    the nginx version matrix
    workflows/lint.yml            fmt, clippy, and the sync check
    workflows/audit.yml           cargo audit

That is the whole directory. There is no CI infrastructure here — no shell, no
Docker bootstrap, no second copy of any test.

## The two transforms sync.sh applies

**1. It unfilters the `pull_request` trigger.** GitHub reads
`pull_request: branches: [main]` as "PRs *targeting* main". ngit-ci matches it
against the proposal's own `branch-name` tag — the *source* branch — so a
proposal from `fix/whatever` matches nothing and the workflow silently never
runs, with no error anywhere. Dropping the filter is the reading that preserves
the intent: on GitHub these run for every PR to main, and on Nostr `main` is
the only thing a proposal ever targets. `push: branches:` is left alone, since
there the ref really is the ref.

Worth fixing upstream — the filter should apply to the base branch — but until
it does, a filtered `pull_request` on a Nostr proposal is a workflow that never
fires and never says so.

**2. It drops every job that declares a job-level `uses:`.** ngit-ci refuses a
workflow file containing one, because it cannot statically verify the container
declarations of a reusable workflow it has not fetched. In `tests.yml` that is
the tag-gated release fan-out — `publish`, `publish-image` and `publish-docs`.
All three are gated on `refs/tags/v*` and would never run on a proposal.

Everything else is copied verbatim.

`docs.yml`, `ghcr-workflow.yml` and `release-workflow.yml` are not mirrored at
all — they publish to GitHub Pages, GHCR and GitHub Releases.

## Docker comes from the sandbox

A GitHub runner hands the job a Docker daemon. So does a paygress sandbox — the
coordinator passes act the sandbox's own socket:

    --act-container-daemon-socket unix:///var/run/docker.sock

`paygress-cli ci up` sets that, along with `--runner socket-adapter` and
`--adapter-socket`. Nothing here has to know about it.

This is why the directory carries no bootstrap. An earlier revision shipped a
259-line script that installed a daemon inside the job container, because
ngit-ci defaults that flag to `-` (mount nothing) and the sandbox's own daemon
could not start a container anyway. Both halves of that are fixed on the
paygress side: a provider advertising the `docker` capability launches the
workload so its daemon works, and `ci up` passes the socket through.

The provider must advertise `docker`. `ci up` requires it before spending
anything, so a provider that cannot serve CI costs a search rather than a lease.

## Running it

    paygress-cli ci up --repo naddr1... --provider <name> --mint https://...

That is the whole setup. No coordinator flags to remember and nothing
privileged: the job talks to the sandbox's daemon over a socket rather than
starting one of its own.

## Cost

One workflow file is one WorkflowPlan is one dispatch is **one leased sandbox**,
each starting from a clean box and recompiling ~400 crates. Four files is four
leases. `tests.yml` alone has a median duration of 35 minutes on GitHub with a
warm cache, and will be longer cold.

That is the number to watch when adding a workflow file: on GitHub a fifth file
is free parallelism, here it is a fifth sandbox.

## Running a workflow by hand

Any of these runs locally with [act](https://github.com/nektos/act), which is
what the coordinator does:

    act -W .github/workflows/tests.yml

The sandbox is x86. Two images are not portable to arm64: CLN's compose
entrypoint downloads an `x86_64-linux-gnu` nip47 plugin, and
`acinq/eclair:release-0.8.0` publishes no arm64 build. On a Mac, register qemu
and pin those two services:

    docker run --privileged --rm tonistiigi/binfmt --install amd64
    printf 'services:\n  cln:\n    platform: linux/amd64\n  eclair:\n    platform: linux/amd64\n' > /tmp/arm64.yml
    export COMPOSE_FILE=docker-compose.yml:/tmp/arm64.yml

binfmt alone is not enough: qemu will dispatch the x86 plugin, but an arm64 CLN
container has no x86 loader to run it with, and the plugin dies in a way that
surfaces as `lightningd: --nip47-relays: unknown option` and takes the node
down with it. The service has to be x86 end to end.
