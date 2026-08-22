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
    lib/docker-in-job.sh          the only ngit-specific code here

## The two transforms sync.sh applies

**1. It adds a step that starts a Docker daemon.** A GitHub runner provides
one; an act job container does not, and this suite reaches its services over
published ports (`curl http://0.0.0.0:8000/`) and with `docker exec` — both of
which only behave like GitHub if the daemon shares the job's network namespace.
So `tests.yml` and `nginx-compat.yml` gain one step after their checkout:

    - name: Start a Docker daemon inside the job
      run: bash .ngit/act/lib/docker-in-job.sh

`docker-in-job.sh` handles what docker-in-docker then needs, each of which was
a failure first: a loopback ext4 data root (overlay2 refuses to stack on the
job's overlayfs and silently degrades to vfs), iptables (no NAT rules means no
published ports), buildx (the Dockerfile's cache mounts are rejected by the
legacy builder), and pinned checksummed tarballs rather than apt (installing
`docker.io` over the act image's CLI fails on a dpkg file conflict).

**2. It drops every job that declares a job-level `uses:`.** ngit-ci refuses a
workflow file containing one, because it cannot statically verify the container
declarations of a reusable workflow it has not fetched. In `tests.yml` that is
the tag-gated release fan-out — `publish`, `publish-image` and `publish-docs`,
which call `release-workflow.yml`, `ghcr-workflow.yml` and `docs.yml`. All
three are gated on `refs/tags/v*` and would never run on a proposal.

Everything else is copied verbatim: of `tests.yml`'s 2945 lines, 2917 are kept
unchanged.

`docs.yml`, `ghcr-workflow.yml` and `release-workflow.yml` are not mirrored at
all — they publish to GitHub Pages, GHCR and GitHub Releases.

## What the coordinator has to pass

    --act-container-options "--privileged"

Docker-in-docker needs it. Privileged is host root, which on a shared ngit-ci
runner would be unacceptable — it is acceptable here only because the job runs
on a rented sandbox that is destroyed when its lease expires.

The sandbox must also be able to nest containers. On paygress's LXD backend
that means the provider advertises the `nesting` capability; on the KVM backend
every sandbox is a full VM and it comes for free.

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

    act -W .github/workflows/tests.yml --container-options "--privileged"

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
