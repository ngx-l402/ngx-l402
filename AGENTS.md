# ngx_l402 agent guide

Guidance for AI agents working in this repository. [CONTRIBUTING.md](CONTRIBUTING.md) is the full guide for humans; this file orients you faster and points at it.

## What this is

An nginx dynamic module written in Rust that puts L402 (Lightning HTTP 402, the `WWW-Authenticate` / 402-payment protocol) in front of nginx locations: unpaid requests get a payment challenge (Lightning invoice or Cashu), paid ones pass through. `ngx_l402_core` is the pure-Rust heart of the module; `src/` is the cdylib nginx loads.

## Layout

| Path | What it is |
|---|---|
| `ngx_l402_core/` | Pure Rust, no nginx symbols. **All unit tests live here**, plus crate-level integration tests in `ngx_l402_core/tests/` (real captured mint responses guarding cdk compat). |
| `src/` | The cdylib nginx loads. `test = false` — it cannot host tests (nginx symbols don't link). Put logic in core, test it there, re-export if needed. |
| `grpc-server/`, `stress-test/` | Standalone crates, deliberately outside the workspace; `stress-test/` carries its own `Cargo.lock`. `grpc-server/` is deliberately lockless — its `Cargo.lock` isn't gitignored, so a local build leaves one that's easy to commit by accident. |
| `docs/` | The mdBook pages (`book.toml` at the repo root). A new page must also be added to `docs/SUMMARY.md`, or it never renders. |
| `.ngit/act/workflows/` | Generated mirror of the GitHub workflows for the Nostr/act CI — never hand-edit. Only the `workflows/` subtree is generated; the `sync.sh` and `README.md` beside it are edited by hand, and leaving a workflow out of the mirror means editing `sync.sh`. See the workflow rule below. |
| `.github/`, `docker-compose.yml`, `Dockerfile` | CI, and the local regtest stack (bitcoind, LND, Redis, Cashu mint, nginx). |

## Setup and build

Full sequences: [docs/building.md](docs/building.md) (from source) and [docs/macos-setup.md](docs/macos-setup.md) (Docker stack on macOS). The short version, Linux:

```bash
# apt: build-essential clang libclang-dev libpcre2-dev zlib1g-dev pkg-config libssl-dev protobuf-compiler
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"   # rustup can't update the current shell's PATH
# the module needs nginx 1.30.7+ or 1.31.9+ (v1.4.0); 1.30.7 matches the default image —
# build against the source of the nginx you target (docs/building.md)
curl -fsSL https://nginx.org/download/nginx-1.30.7.tar.gz | tar -xz -C /tmp
cd /tmp/nginx-1.30.7 && ./configure --with-compat && cd -
export NGINX_SOURCE_DIR=/tmp/nginx-1.30.7
cargo build --release --features export-modules   # → target/release/libngx_l402_lib.so
```

On macOS, also `export LIBCLANG_PATH=/opt/homebrew/opt/llvm/lib` (Homebrew LLVM) — or skip all of the above and run the stack in Docker, following [docs/macos-setup.md](docs/macos-setup.md) § 3 start to finish: the compose command, then the block-mining step the section covers — without it, LND can't create invoices and `/protected` never returns its 402.

Two things bite: without `--with-compat`, nginx may compile out fields the module reads (observed as an E0609 on `x_real_ip` against nginx 1.28.0) and the build fails; and macOS periodically wipes `/tmp`, so the nginx tree there may need re-downloading after a wipe.

## Before you call it done

CI runs these; run them first:

```bash
cargo fmt --check
# clippy needs NGINX_SOURCE_DIR set to a ./configure --with-compat nginx tree — see "Setup and build" above
cargo clippy --all-targets -- -D warnings
cargo test -p ngx_l402_core
# machete is a third-party subcommand — install the version pinned in .github/workflows/machete.yml
cargo machete
# audit is a CI gate too — one-time install: cargo install cargo-audit --locked (as audit.yml does)
cargo audit
```

When CI fails on a change of yours, first confirm the failing run belongs to the head commit and actually failed — not cancelled, not superseded by a newer push — then read the failing job's log directly. The check rollup lags and can show stale results from superseded runs.

## Rules that are easy to miss

- **Workflow edits:** `.github/workflows/` and `.ngit/act/workflows/` are kept in sync by `./.ngit/act/sync.sh`. Run it and stage the mirror in the same commit; a lint job checks the mirror. The mirror intentionally differs from the GitHub copy — trust sync.sh output.
- **Integration tests** are steps in `.github/workflows/tests.yml`, not a separate harness. Read CONTRIBUTING.md § Testing Guidelines before adding a case: poll with `.github/scripts/ci-wait.sh` instead of sleeping, dump container logs before any `exit 1`, flush Redis when a case replays a preimage, and stop the containers you started — every `nginx-*` service binds port 8000. Several cases live in `.github/scripts/` — `test-tenant-config.sh`, `test-nested-off.sh`, `test-mcp.sh` — which `tests.yml` calls.
- **A new protected route in the integration stack:** without a matching `location` block the route usually isn't challenged (watch `/protected`-prefixed names: nginx's longest-prefix match sends them to the existing `/protected` block, hiding the gap), and without a file behind it the *paid* request 404s — CONTRIBUTING.md § Testing Guidelines covers the `nginx.conf`/`Dockerfile` requirements; add a case in `tests.yml` to cover it.
- **Panics:** no `.unwrap()`, `.expect()`, or unchecked indexing on request paths reachable from an nginx callback, except `.expect()` on invariants established before workers fork — the accepted pattern (see `get_cached_seed` in `src/cashu.rs`). § Coding Standards states the rule ("No Panics"), § Common Pitfalls states the consequence in CONTRIBUTING.md's own words: "Panic in FFI code (Nginx will crash)". § Unsafe Code Guidelines governs pointers and `// SAFETY:` comments.
- **Commits:** conventional style (`docs:`, `ci:`, `fix:`, `feat:`), one concern per PR.
- **Bitfield accessors:** don't use nginx-sys's generated accessors on `ngx_http_request_t` (e.g. `header_only()`) — nginx 1.30 adds an `in_port_t port` right before the bitfields and nginx-sys 0.5 places most of them two bits early, so `header_only()` actually reads `pipeline`. Check the field's underlying semantics instead; `is_head` in `src/lib.rs` reads the method directly.
- **Secrets:** never commit *real* credentials — a live `ROOT_KEY`, a real `CASHU_WALLET_MNEMONIC` or `CASHU_P2PK_PRIVATE_KEY`, NWC connection URIs, the LND `admin.macaroon` file, preimages, or `.env`. `tests.yml` deliberately commits fixed test values (`ROOT_KEY`, the `abandon … about` mnemonic, the `00…01` P2PK key) — reuse those in new cases. See [SECURITY.md](SECURITY.md).

## Where things are documented

| Looking for | Go to |
|---|---|
| `l402_*` directives and env vars | `docs/config-env-vars.md`; the `l402_mcp*` directives live in `docs/mcp.md` |
| Payment backends (Lightning, Cashu) | `docs/lightning.md`, `docs/cashu.md` |
| Local regtest stack, end to end | `docs/macos-setup.md` (LND only; `tests.yml` runs every backend) |
| Running the published image against real nodes | `docs/install-docker.md` |
| Architecture and design principles | CONTRIBUTING.md § Architecture Guidelines |
| Performance rules and the stress test | CONTRIBUTING.md § Performance Testing |
| PR process (GitHub and Nostr) | CONTRIBUTING.md § Submitting a Pull Request |
| Security policy and private disclosure | `SECURITY.md` |
