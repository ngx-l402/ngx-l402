# ngx_l402 agent guide

Guidance for AI agents working in this repository. [CONTRIBUTING.md](CONTRIBUTING.md) is the full guide for humans; this file orients you faster and points at it.

## What this is

An nginx dynamic module written in Rust that puts L402 (Lightning HTTP 402, the `WWW-Authenticate` / 402-payment protocol) in front of nginx locations: unpaid requests get a payment challenge (Lightning invoice or Cashu), paid ones pass through. `ngx_l402_core` is the pure-Rust heart of the module; `src/` is the cdylib nginx loads.

## Layout

| Path | What it is |
|---|---|
| `ngx_l402_core/` | Pure Rust, no nginx symbols. **All unit tests live here**, plus crate-level integration tests in `ngx_l402_core/tests/` (real captured mint responses guarding cdk compat). |
| `src/` | The cdylib nginx loads. `test = false` — it cannot host tests (nginx symbols don't link). Put logic in core, test it there, re-export if needed. |
| `grpc-server/`, `stress-test/` | Standalone crates, deliberately outside the workspace; `stress-test/` carries its own `Cargo.lock`. |
| `docs/` | The mdBook pages (`book.toml` at the repo root). A new page must also be added to `docs/SUMMARY.md`, or it never renders. |
| `.ngit/act/` | Generated mirror of the GitHub workflows for the Nostr/act CI. Never hand-edit; see the workflow rule below. |
| `.github/`, `docker-compose.yml`, `Dockerfile` | CI, and the local regtest stack (bitcoind, LND, Redis, Cashu mint, nginx). |

## Setup and build

Full sequences: [docs/building.md](docs/building.md) (from source) and [docs/macos-setup.md](docs/macos-setup.md) (Docker stack on macOS). The short version, Linux:

```bash
# apt: build-essential clang libclang-dev libpcre2-dev zlib1g-dev pkg-config libssl-dev protobuf-compiler
# (configure needs PCRE2; building.md's apt line omits libpcre2-dev)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
curl -fsSL https://nginx.org/download/nginx-1.28.0.tar.gz | tar -xz -C /tmp
cd /tmp/nginx-1.28.0 && ./configure --with-compat && cd -
export NGINX_SOURCE_DIR=/tmp/nginx-1.28.0
cargo build --release --features export-modules   # → target/release/libngx_l402_lib.so
```

On macOS, also `export LIBCLANG_PATH=/opt/homebrew/opt/llvm/lib` (Homebrew LLVM) — or skip all of the above and run the stack in Docker: `cp .env.example .env && docker compose up -d bitcoind lndnode-receiver redis grpc-content-server nginx-lnd`.

Two things bite: without `--with-compat`, nginx may compile out fields the module reads (observed as an E0609 on `x_real_ip` against nginx 1.28.0) and the build fails; and macOS periodically wipes `/tmp`, so the nginx tree there may need re-downloading after a wipe.

## Before you call it done

CI runs these; run them first:

```bash
cargo fmt --check
# clippy needs NGINX_SOURCE_DIR set to a ./configure --with-compat nginx tree — see "Setup and build" above
cargo clippy --all-targets -- -D warnings
cargo test -p ngx_l402_core
# machete is a third-party subcommand — one-time install: cargo install cargo-machete --locked --version 0.9.2 (CI's pin)
cargo machete
```

When CI fails on a change of yours, first confirm the failing run belongs to the head commit and actually failed — not cancelled, not superseded by a newer push — then read the failing job's log directly. The check rollup lags and can show stale results from superseded runs.

## Rules that are easy to miss

- **Workflow edits:** `.github/workflows/` and `.ngit/act/workflows/` are kept in sync by `./.ngit/act/sync.sh`. Run it and stage the mirror in the same commit; a lint job checks the mirror. The mirror intentionally differs from the GitHub copy — trust sync.sh output.
- **Integration tests** are steps in `.github/workflows/tests.yml`, not a separate harness. Read CONTRIBUTING.md § Testing Guidelines before adding a case: poll with `.github/scripts/ci-wait.sh` instead of sleeping, dump container logs before any `exit 1`, flush Redis when a case replays a preimage, and stop the containers you started — every `nginx-*` service binds port 8000.
- **A new protected route in the integration stack** needs the `location` block in `nginx.conf` and a `COPY index.html` line in the `Dockerfile`, or it 404s before the module ever runs; add a case in `tests.yml` to cover it.
- **Panics:** no `.unwrap()`, `.expect()`, or unchecked indexing on any path reachable from an nginx callback — § Coding Standards states the rule ("No Panics"), § Common Pitfalls states the consequence in CONTRIBUTING.md's own words: "Panic in FFI code (Nginx will crash)". § Unsafe Code Guidelines governs pointers and `// SAFETY:` comments.
- **Commits:** conventional style (`docs:`, `ci:`, `fix:`, `feat:`), one concern per PR.
- **Secrets:** never commit `.env`, `ROOT_KEY` values, `CASHU_WALLET_MNEMONIC`, `CASHU_P2PK_PRIVATE_KEY`, NWC connection URIs, the LND `admin.macaroon` file, or preimages to any tracked file. See [SECURITY.md](SECURITY.md).

## Where things are documented

| Looking for | Go to |
|---|---|
| Every `l402_*` directive and env var | `docs/config-env-vars.md` |
| Payment backends (Lightning, Cashu) | `docs/lightning.md`, `docs/cashu.md` |
| Local regtest stack, end to end | `docs/macos-setup.md`, `docs/install-docker.md` |
| Architecture and design principles | CONTRIBUTING.md § Architecture Guidelines |
| Performance rules and the stress test | CONTRIBUTING.md § Performance Testing |
| PR process (GitHub and Nostr) | CONTRIBUTING.md § Submitting a Pull Request |
| Security policy and private disclosure | `SECURITY.md` |
