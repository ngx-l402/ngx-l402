# Contributing to ngx_l402

Thank you for your interest in contributing to ngx_l402! This guide will help you understand how to contribute effectively to this high-performance Nginx module for L402 authentication.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Quick Start for Contributors](#quick-start-for-contributors)
- [How to Contribute](#how-to-contribute)
- [Development Workflow](#development-workflow)
- [Coding Standards](#coding-standards)
- [Testing Guidelines](#testing-guidelines)
- [Performance Testing](#performance-testing)
- [Submitting a Pull Request (GitHub + Nostr)](#submitting-a-pull-request-github--nostr)
- [Reporting Issues (GitHub + Nostr)](#reporting-issues-github--nostr)
- [Pull Request Process](#pull-request-process)
- [Architecture Guidelines](#architecture-guidelines)

## Code of Conduct

**Inclusivity Policy**: This project maintains a written policy requiring the equal treatment of all people, regardless of race, ethnicity, gender, sexual orientation, disability, age, religion, political opinion, or any other status. All contributors and participants must adhere to this standard.

- Be welcoming, respectful, and inclusive
- Accept constructive criticism gracefully
- Focus on what's best for the community
- Show empathy towards other contributors

## Quick Start for Contributors

1. **Fork and clone** the repository
2. **Review the [README.md](README.md)** for installation and configuration
3. **If you are on macOS, follow [docs/macos-setup.md](docs/macos-setup.md)**
4. **Create a feature branch**: `git checkout -b feature/your-feature-name`
5. **Make your changes** following our coding standards
6. **Test thoroughly** before submitting

---

## How to Contribute

We welcome various types of contributions:

| Type | Examples |
|------|----------|
| 🐛 **Bug Fixes** | Memory leaks, crashes, incorrect behavior |
| ✨ **Features** | New payment methods, protocol extensions |
| 📚 **Documentation** | Code comments, usage examples, guides |
| 🧪 **Testing** | Unit tests, integration tests, benchmarks |
| ⚡ **Performance** | Profiling, optimization, bottleneck fixes |
| 🔧 **Tooling** | CI/CD improvements, development scripts |

### Finding Issues to Work On

- Browse [open issues](https://github.com/ngx-l402/ngx-l402/issues)
- Look for `good-first-issue` or `help-wanted` labels if you are new.
- Check performance optimization issues (especially post-stress testing)
- Improve areas where documentation is unclear

### Reporting Bugs

**Before submitting**, search existing issues to avoid duplicates.

Include in your bug report:
- **Steps to reproduce** the issue
- **Expected vs actual behavior**
- **Environment**: Nginx version, OS, module version
- **Logs**: Relevant error messages (sanitize secrets!)
- **Configuration**: Nginx config (remove sensitive data)

Use the GitHub issue template when creating a new bug report. No GitHub account?
See [Reporting Issues (GitHub + Nostr)](#reporting-issues-github--nostr) — you can
file on the Nostr side instead.

### Suggesting Features

When proposing new features:
- **Check existing feature requests** first
- **Describe the use case** and problem it solves
- **Consider performance impact** (this is a high-performance module)
- **Discuss alternatives** and trade-offs

---

## Development Workflow

### Branching Strategy

```bash
# Create a feature branch
git checkout -b feature/add-new-payment-method

# Create a bugfix branch
git checkout -b fix/memory-leak-in-cashu

# Create a performance branch
git checkout -b perf/optimize-token-verification
```

### Development Cycle

1. **Make changes** in your feature branch
2. **Test locally**
3. **Commit with clear messages** (see [Commit Guidelines](#commit-guidelines))
4. **Push to your fork**
5. **Open a Pull Request**

### Keeping Your Fork Updated

```bash
# Fetch upstream changes
git fetch upstream

# Merge upstream main into your branch
git checkout main
git merge upstream/main

# Rebase your feature branch
git checkout feature/your-feature
git rebase main
```

---

## Coding Standards

### Rust Best Practices

**Required before submitting**:
```bash
cargo fmt              # Format code
```

**Code style**:
- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Write idiomatic Rust (iterators over loops, avoid unnecessary cloning)
- Use meaningful variable names
- Add doc comments (`///`) for public functions

**Code quality principles**:
- **Safety First**: Minimize `unsafe` code; document all safety invariants
- **No Panics**: Use `Result<T, E>` instead of `.unwrap()` or `.expect()`
- **Smart Logging**: Use appropriate log levels (`debug!`, `info!`, `warn!`, `error!`)
- **Self-Documenting**: Code should be clear; comments explain *why*, not *what*
- **Performance Aware**: Avoid allocations and blocking in hot paths

### Commit Guidelines

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types**: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `chore`

**Examples**:
```
feat: add P2PK mode for optimized verification

fix: correctly validate expired macaroons

perf: use connection pooling to reduce latency

docs: clarify unsafe code guidelines
```

### Unsafe Code Guidelines

This module uses FFI with Nginx C API. When writing `unsafe` code:

- **Nginx-guaranteed pointers** (e.g. `request`, `connection`, `loc_conf`): No null check needed, but add a `// SAFETY:` comment explaining why.
- **Optional/allocated pointers** (e.g. optional headers, `ngx_array_push` results): **Must** null-check before dereferencing.

```rust
// ✅ GOOD: Null-check where genuinely needed, SAFETY docs everywhere

// SAFETY: `request` and `connection->log` are guaranteed valid by Nginx
// for all access-phase handlers.
let r = &mut *request;
let log = &mut *(*r.connection).log;

// `authorization` CAN be null — not every request has the header.
// This check is genuinely needed.
let auth_header = if !r.headers_in.authorization.is_null() {
    // SAFETY: checked non-null above; Nginx guarantees `value.data`
    // is a valid C string for the header's lifetime.
    Some(CStr::from_ptr((*r.headers_in.authorization).value.data as *const c_char)
        .to_str()
        .unwrap_or("")
        .to_string())
} else {
    None
};

// `ngx_array_push` returns null if the allocation fails.
// This check is genuinely needed.
let h = ngx_array_push(&mut (*cmcf).phases[phase].handlers)
    as *mut ngx_http_handler_pt;
if h.is_null() {
    return NGX_ERROR as ngx_int_t;
}

// ❌ BAD: Missing check on optional pointer, no SAFETY comments
let val = (*r.headers_in.authorization).value;   // Segfault if no auth header!
let h = ngx_array_push(&mut handlers);           // Could be null!
*h = Some(my_handler);                           // Segfault if alloc failed!
```

**Rules**:
- Add a `// SAFETY:` comment above each `unsafe` dereference explaining why it is valid
- Null-check pointers that are **not** guaranteed by Nginx (optional headers, allocations, your own data)
- For Nginx-guaranteed pointers, document the guarantee instead of adding a redundant check
- Prefer safe abstractions when possible
- Test with Valgrind or AddressSanitizer

---

## Testing Guidelines

### Where tests live

| Kind | Location | Run with |
|------|----------|----------|
| **Unit** — pure logic, no nginx, no network | `#[cfg(test)] mod tests` in `ngx_l402_core/src/*.rs` | `cargo test -p ngx_l402_core` |
| **Integration** — the real module, in nginx, against a real backend | steps in `.github/workflows/tests.yml` | see [Running the CI suite locally](#running-the-ci-suite-locally) |

There is no separate test harness. The integration suite *is* `tests.yml`: one
`test` job that brings up regtest bitcoind, a Lightning node, Redis and a Cashu
mint via `docker-compose.yml`, then walks each backend in turn.

Prefer a unit test when the behaviour can be reached without nginx. They run in
seconds and gate the Docker steps, so a logic regression fails the build before
anything is built.

### Adding a unit test

Put it next to the code, in the same file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_macaroon_with_no_caveats() {
        assert!(parse_l402_header("L402 :deadbeef").is_err());
    }
}
```

Anything with no nginx dependency belongs in `ngx_l402_core` rather than `src/`,
where it can be tested this way.

### Adding an integration test case

Integration cases are steps in the single `test` job, named
`Run Integration Tests - <Backend>`. The job is sequential and stateful: each
backend section starts its containers, asserts, then stops them, because they
all bind port 8000.

**Add to an existing section** whenever the case fits a backend already covered.
Only add a new section for a new backend.

The conventions every case follows — a reviewer will ask for all of these:

**1. Poll for readiness. Never `sleep` and assert.** Source the helpers and use
them; a fixed sleep is a bet on a loaded runner that eventually loses and takes
the whole job down.

```bash
source .github/scripts/ci-wait.sh

wait_http http://0.0.0.0:8000/ 200 90 || { docker logs nginx-lnd; exit 1; }
wait_log  nginx-lnd "start worker processes" 90
wait_container lndnode 90
```

**2. Assert on the status code, and dump logs before exiting.** A bare
`exit 1` tells the next person nothing:

```bash
response=$(curl -s -w "\n%{http_code}" --max-time 30 http://0.0.0.0:8000/protected) || true
status_code=$(echo "$response" | tail -n1); status_code=${status_code:-000}
if [ "$status_code" -ne 402 ]; then
  echo "Error: /protected returned $status_code, expected 402"
  docker logs nginx-lnd
  exit 1
fi
```

Use `curl_until <status> <curl args...>` instead for a route that mints an
invoice from a third party — it retries and sets `response` and `status_code`
for you, so the assertion above is unchanged.

**3. Flush Redis when the case replays a preimage.** Replay protection is
persistent, so a preimage a previous section already spent returns 401 for
reasons that have nothing to do with your case:

```bash
docker exec redis redis-cli flushall
```

**4. Assert the negative cases, not just the happy path.** A case that only
proves a 200 does not prove the module rejects anything. Cover missing header,
malformed macaroon, wrong preimage, and expiry where the route sets a timeout.

**5. Leave the stack as you found it.** End a new backend section by stopping
what it started, so the next section gets port 8000:

```bash
docker-compose -f docker-compose.yml stop nginx-<backend>
```

**6. `assert_no_crash` catches what a status code cannot.** nginx restarts a
crashed worker, so a segfault can surface as a passing request. The final
`Check for worker crashes` step covers every container the job touched — add
yours to it.

### Wiring a new route or backend

A case that needs something not already in the stack touches more than
`tests.yml`:

| You need | Also edit |
|----------|-----------|
| A new protected route | `nginx.conf` (the `location` block) **and** `Dockerfile` — every protected location needs its own `COPY index.html /usr/share/nginx/html/<route>/index.html`, or it 404s before the module ever runs |
| A new Lightning backend | `docker-compose.yml` — a `nginx-<backend>` service on port 8000 with the right `LN_CLIENT_TYPE`, plus the node service itself |
| Log assertions | `RUST_LOG=info` on the nginx service; `info!` lines are filtered out by default and your `grep` will never match |

### Running the CI suite locally

The suite runs under [act](https://github.com/nektos/act), which is what the
Nostr coordinator does:

```bash
act -W .github/workflows/tests.yml
```

The suite is written for x86. Two images are not portable to arm64: CLN's
compose entrypoint downloads an `x86_64-linux-gnu` nip47 plugin, and
`acinq/eclair:release-0.8.0` publishes no arm64 build. On an Apple Silicon Mac,
register qemu and pin those two services:

```bash
docker run --privileged --rm tonistiigi/binfmt --install amd64
printf 'services:\n  cln:\n    platform: linux/amd64\n  eclair:\n    platform: linux/amd64\n' > /tmp/arm64.yml
export COMPOSE_FILE=docker-compose.yml:/tmp/arm64.yml
```

binfmt alone is not enough: qemu will dispatch the x86 plugin, but an arm64 CLN
container has no x86 loader to run it with, and the plugin dies in a way that
surfaces as `lightningd: --nip47-relays: unknown option` and takes the node down
with it. The service has to be x86 end to end.

For iterating on a single backend, skip act and drive compose directly — see
[docs/macos-setup.md](docs/macos-setup.md).

### After editing a workflow

CI runs on **both** GitHub Actions and [Nostr](https://gitworkshop.dev), from
the same workflow definitions. `.ngit/act/workflows/` is generated from
`.github/workflows/` and is never hand-edited. Regenerate it in the same commit:

```bash
./.ngit/act/sync.sh
git add -A .ngit/act/workflows
```

The `fmt` job fails on both pipelines if you forget. See
[.ngit/act/README.md](.ngit/act/README.md) for what the sync transforms and why.

One thing to know before adding a **new workflow file**: on GitHub a fifth file
is free parallelism, on Nostr it is a fifth leased sandbox recompiling ~400
crates from cold. Add a job or a step to an existing file unless the work
genuinely needs its own file.

---

## Performance Testing

> 📊 This module has been [stress tested](https://primal.net/e/nevent1qqs0x4x4g4jhugs44mdqwwe7c52lj0m63qsus7f68kvvhx4fs4jf8cqdvadjl) under high load. Performance-critical changes must be benchmarked.

### Performance-Critical Areas

| Component | Target | Notes |
|-----------|--------|-------|
| Request handler | < 1ms | Processes every request |
| L402 verification | < 5ms | Valid token path |
| Cashu P2PK mode | < 10ms | With local verification |
| Redis lookup | < 1ms | Dynamic pricing |

### Running the Stress Test

A dedicated Rust stress testing tool is available under `stress-test/` to benchmark the module:

```bash
# Build the stress test tool
cd stress-test && cargo build --release

# Single benchmark run
./target/release/stress-test --url http://localhost:8000/protected -c 50 -n 10000

# With authentication
./target/release/stress-test --url http://localhost:8000/protected \
    --auth "L402 macaroon:preimage" -c 50 -n 10000

# Save baseline results for later comparison
./target/release/stress-test --url http://localhost:8000/protected \
    -c 50 -n 10000 --save baseline.json

# Compare current run against a saved baseline
./target/release/stress-test --url http://localhost:8000/protected \
    -c 50 -n 10000 --compare baseline.json

# Run a concurrency sweep (1, 10, 25, 50, 100 concurrent connections)
./target/release/stress-test --url http://localhost:8000/protected \
    -n 10000 --sweep --save sweep_results.json
```

The tool reports latency percentiles (p50/p90/p95/p99/max), throughput (req/s), NGINX worker RSS memory usage, and error samples. When submitting performance-related PRs, include benchmark results from `--compare` in your PR description.

### Optimization Guidelines

**Hot Path Rules**:
- Avoid allocations (use static buffers, lazy initialization)
- No blocking I/O (use async or caching)
- Prefer stack over heap
- Use connection pooling for Redis/database

Example PR description:
```markdown
## Performance Improvement

**Before**: 5,420 req/s (p95: 25ms)
**After**: 8,730 req/s (p95: 12ms)
**Improvement**: +61% throughput, -52% latency
```

---

## Submitting a Pull Request (GitHub + Nostr)

This project is hosted on **both GitHub and [Nostr](https://gitworkshop.dev)**.

**First PR? Just pick whichever door you already know. Contributing regularly? Please post
to both.** Don't let the dual setup stop you from contributing — a PR on either side is
always welcome, and maintainers will bridge the gap when needed.

| | GitHub (classic) | Nostr (ngit) |
|---|---|---|
| Need an account | GitHub account | Nostr keypair (nsec) |
| Fork required | Yes | No |
| CI runs automatically | Yes | After a maintainer mirrors it |

### Option A — GitHub (easiest if you already have a GitHub account)

```bash
# Fork + clone (origin = your fork, upstream = this repo)
gh repo fork ngx-l402/ngx-l402 --clone
cd ngx-l402

git checkout -b fix/my-change
# ...make changes, commit...
git push -u origin fix/my-change

gh pr create --repo ngx-l402/ngx-l402 --base main \
  --title "fix: my change" --body "what and why"
```

### Option B — Nostr (no fork, no GitHub account needed)

```bash
# One-time: install ngit and log in with your Nostr identity
curl -Ls https://ngit.dev/install.sh | bash
ngit login

# Clone straight from Nostr (clone URL is on the gitworkshop.dev repo page)
git clone nostr://npub1qnw27f2jsqvn05wzpd56m7ykgmepk57p0yrzw8fzc7lfhjkmjmqqmd9r6h/ngx-l402
cd ngx-l402

# Branch MUST use the pr/ prefix
git checkout -b pr/my-change
# ...make changes, commit...

# This push IS the PR — it publishes a proposal to gitworkshop.dev
git push -o 'title=fix: my change' -o 'description=what and why' -u origin pr/my-change
```

### Option C — both (preferred for regular contributors)

Do the GitHub setup from Option A, then add the Nostr remote once:

```bash
git remote add nostr nostr://npub1qnw27f2jsqvn05wzpd56m7ykgmepk57p0yrzw8fzc7lfhjkmjmqqmd9r6h/ngx-l402
```

Then, from a single `pr/`-prefixed branch, push to both:

```bash
git checkout -b pr/my-change
# ...make changes, commit...

# 1) GitHub PR
git push -u origin pr/my-change
gh pr create --repo ngx-l402/ngx-l402 --base main \
  --title "fix: my change" --body "what and why"

# 2) Nostr PR — same branch, same title/description
git push -o 'title=fix: my change' -o 'description=what and why' nostr pr/my-change
```

Use identical title and description on both, and link each PR to the other in its
description. **To revise after review**, commit and push the branch again to both remotes —
both PRs update in place.

> **Maintainers:** merge locally with `git merge --no-ff` and push `main` through the Nostr
> remote — never use GitHub's squash/merge button. Preserving the original commit SHAs is
> what lets both the GitHub PR and the Nostr proposal close themselves automatically.

---

## Reporting Issues (GitHub + Nostr)

Issues work like PRs in that either side is welcome — but unlike PRs, **there is no
need to file on both**. Maintainers mirror issues across, which is why the same
reports show up on both trackers and gitworkshop issues carry their GitHub numbers.

| You have | File here | Notes |
|---|---|---|
| A GitHub account | [GitHub issues](https://github.com/ngx-l402/ngx-l402/issues) | The canonical tracker. Triage happens here and issue numbers come from here. |
| Only a Nostr identity (Option B) | The repo page on [gitworkshop.dev](https://gitworkshop.dev) | Fully supported — you never need a GitHub account just to report a bug. |

**Why the Nostr tracker is a real channel, not decoration:** the GitHub organisation
has twice been hidden by automated flagging. While that is in effect, nobody can file
or read issues on GitHub at all. The gitworkshop tracker stays reachable regardless.

---

## Pull Request Process

### Pre-Submission Checklist

Before opening a PR:

- [ ] Code formatted: `cargo fmt`
- [ ] Tests pass: `cargo test`
- [ ] Module builds: `cargo build --release --features export-modules`
- [ ] Manual testing with Nginx performed
- [ ] New behaviour covered by a test — a unit test in `ngx_l402_core`, or a case in `.github/workflows/tests.yml` (see [Testing Guidelines](#testing-guidelines))
- [ ] If you edited a workflow: `./.ngit/act/sync.sh` run and `.ngit/act/workflows` staged in the same commit
- [ ] Commit messages follow conventions
- [ ] No merge conflicts with `main`
- [ ] Documentation updated (if needed)

### PR Template

Use this structure for your PR description:

```markdown
## What
Brief description of changes (1-2 sentences).

## Why
Motivation / problem being solved.

## How
Implementation approach and key changes.

## Testing
- [ ] Unit tests added/updated
- [ ] Manually tested with Nginx
- [ ] Load tested (if performance-related)

## Checklist
- [ ] Code formatted and linted
- [ ] No breaking changes (or documented)
- [ ] Benchmark results included (if perf change)

Closes #issue_number
```

### Review Process

1. **CI checks run**: Automated formatting, linting, tests
2. **Maintainer review**: Code quality, design, safety
3. **Discussion**: Address feedback and questions
4. **Approval**: One maintainer approval required
5. **Merge**: Merged to `main` preserving commits (`git merge --no-ff`), so both the GitHub PR and Nostr proposal close automatically

### After Your PR is Merged

- You'll be credited in release notes
- Changes included in next release
- Consider helping with documentation or examples

---

## Architecture Guidelines

### Module Structure

```
Request → Access Handler → L402 Check → Verification → Response
                               ↓
                          Redis Pricing (dynamic)
                               ↓
                      Lightning/Cashu Payment
```

### Key Design Principles

1. **Minimal Request Latency**: Every request passes through this module
2. **Zero-Copy Where Possible**: Avoid unnecessary allocations
3. **Fail Securely**: Errors should deny access, not grant it
4. **Async-Aware**: Use non-blocking operations for I/O
5. **Memory Safety**: FFI boundary must be bulletproof

### Adding New Features

When adding features, consider:

- **Backward compatibility**: Will this break existing users?
- **Configuration**: Should this be optional/configurable?
- **Performance impact**: Benchmark critical paths
- **Error handling**: What happens when it fails?
- **Testing**: Can this be unit tested? Integration tested?

### Common Pitfalls

❌ **Don't**:
- Block in the request handler
- Panic in FFI code (Nginx will crash)
- Allocate on every request
- Use `.unwrap()` or `.expect()`
- Skip null checks on C pointers

✅ **Do**:
- Use lazy static for expensive initialization
- Return errors via `Result`
- Profile hot paths
- Add comprehensive safety comments
- Test edge cases

---

## Getting Help

- 📖 **Documentation**: See [README.md](README.md)
- 🐛 **Bug Reports**: [GitHub Issues](https://github.com/ngx-l402/ngx-l402/issues)
- 📚 **Learning Resources**:
  - [Rust Book](https://doc.rust-lang.org/book/)
  - [L402 Protocol](https://docs.lightning.engineering/the-lightning-network/l402)
  - [Nginx Dev Guide](http://nginx.org/en/docs/dev/development_guide.html)
  - [Cashu Protocol (NUT)](https://github.com/cashubtc/nuts)

---

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENCE).

---

## Quick Commands Reference

```bash
# Development
cargo fmt                                    # Format
cargo test                                   # Local test pass
cargo build --release --features export-modules  # Build

# Installation
sudo cp target/release/libngx_l402_lib.so /etc/nginx/modules/
sudo systemctl restart nginx && sudo systemctl reload nginx

# Debugging
sudo journalctl -u nginx -f                  # View logs
sudo tail -f /var/log/nginx/error.log        # Error logs
```

macOS contributors should use `docs/macos-setup.md` for Homebrew NGINX paths and commands.

---

**Thank you for contributing to ngx_l402!** 🚀⚡

Your contributions help make Lightning Network payments more accessible and performant for everyone.
