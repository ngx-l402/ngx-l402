# Nostr-native CI

Workflows run by an ngit-ci coordinator on paid paygress sandboxes, published
back to Nostr as kind 9841/9842 events. GitHub Actions syntax, executed by
`act`, so nothing here may assume a GitHub-hosted runner: no preinstalled
toolchain, no `GITHUB_TOKEN`, no docker daemon handed to the job.

`.github/workflows/` is deliberately left alone — ngit-ci detects but never
runs it, so the GitHub mirror keeps its own pipeline. This directory is a port
of `.github/workflows/tests.yml`, split so that one failure names one area.

## Layout

    workflows/   one file per slice; four steps each, no logic
    slices/      the actual tests, one script per slice
    lib/         everything shared between slices

Keeping the tests in shell rather than in YAML is what makes them debuggable:
any slice runs by hand against any docker daemon, without act, a coordinator,
or a lease.

    .ngit/act/slices/lnurl.sh

## Slices

| workflow | needs | notes |
| -------- | ----- | ----- |
| `build` | nothing | fmt, clippy, unit tests, release build, `.so` check |
| `security-audit` | nothing | `cargo audit`; no docker at all |
| `integration-lnurl` | redis, nginx-lnurl, gRPC backend | cheapest; run this first |
| `integration-lnd` | full regtest backbone | pricing, pay-and-redeem, method binding, Cashu |
| `integration-cln` | backbone + CLN | CLN, BOLT12, autodetect |
| `integration-nwc` | backbone + CLN | reaches a public Nostr relay |
| `integration-cashu-redis` | backbone | replay, rate limiting, redemption, multi-tenant |
| `integration-cashu-p2pk` | backbone | NUT-11/NUT-24 pay-to-public-key |
| `integration-lnc` | backbone + litd | reaches Lightning Labs' mailbox server |
| `integration-eclair` | backbone + Eclair | channel confirmation is order-sensitive |
| `nginx-compat-*` | nothing | one file per nginx version |

Between them these cover every gating step of `.github/workflows/tests.yml`,
`nginx-compat.yml` and `audit.yml`. Two things are deliberately not here:

- **The Tor / SOCKS5 steps.** On the mirror they end in `|| echo`, so they
  cannot fail a build — they print whether an onion address appeared and move
  on. Porting them would buy log output and a Tor container per run. Worth
  adding only alongside a decision to make them actually assert something.
- **`docs.yml` and `ghcr-workflow.yml`**, plus the release upload at the end of
  `tests.yml`. Those publish to GitHub Pages, GHCR and GitHub Releases; they
  are deploy steps aimed at GitHub-specific targets, not gates on a proposal.

Three slices depend on a third party being reachable — getalby.com for LNURL,
a Nostr relay for NWC, the mailbox server for LNC. When one of those fails on
its own, suspect the third party before the module.

## What the coordinator has to pass

    --act-container-options "--privileged"

The suite reaches its services over published ports (`curl
http://0.0.0.0:8000/`) and with `docker exec`. Both only behave like a GitHub
runner if the docker daemon shares the job container's network namespace, so
the job starts its own daemon — see `lib/docker-in-job.sh`, which also explains
the storage-driver, iptables and buildx traps that come with that.

Privileged is host root, which on a shared ngit-ci runner would be
unacceptable. It is acceptable here only because the job runs on a rented
sandbox that is destroyed when the lease expires.

## Running a slice on an arm64 machine

The sandbox is x86. Two of the images are not portable to it: CLN's compose
entrypoint downloads an `x86_64-linux-gnu` nip47 plugin, and
`acinq/eclair:release-0.8.0` publishes no arm64 build. To run the CLN, NWC or
Eclair slices on a Mac, register qemu and pin those two services:

    docker run --privileged --rm tonistiigi/binfmt --install amd64
    printf 'services:\n  cln:\n    platform: linux/amd64\n  eclair:\n    platform: linux/amd64\n' > /tmp/arm64.yml
    COMPOSE_FILE=docker-compose.yml:/tmp/arm64.yml .ngit/act/slices/cln.sh

binfmt on its own is not enough. qemu will dispatch the x86 plugin, but an
arm64 CLN container has no x86 loader to run it with, and the plugin then dies
in a way that surfaces as `lightningd: --nip47-relays: unknown option` and
takes the node down with it. The service has to be x86 end to end.

## Adding a slice

1. Write `slices/<name>.sh`. Source `lib/env.sh`, `lib/l402.sh`, and
   `lib/lightning.sh` if it needs to pay anything. Set `DIAG_CONTAINERS` so a
   failure dumps the right logs.
2. Copy a workflow file and point it at the script.
3. Run it by hand first. A full CI cycle is 15–45 minutes and costs sats; a
   typo does not need to cost either.

Two rules the existing slices follow, both learned the hard way:

- **Never `printf ... | grep -q`.** Under `set -o pipefail` that reports
  failure precisely when it matches — grep exits early, printf takes SIGPIPE,
  and 141 becomes the pipeline's status. Use `grep <<< "$var"`.
- **Never sleep a fixed number of seconds** waiting for a service. Use the
  waits in `lib/l402.sh`; they poll for the condition they care about.
