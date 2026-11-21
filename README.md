# ngx_l402 — L402 Nginx Module

[![Join us on Discord](https://img.shields.io/badge/Discord-Join%20the%20community-5865F2?logo=discord&logoColor=white)](https://discord.gg/QYwcj2wuQ)

An [L402](https://docs.lightning.engineering/the-lightning-network/l402) authentication module for Nginx that enables Lightning Network-based monetization for your REST APIs (HTTP/1 and HTTP/2).

Supports **LND**, **LNC**, **CLN**, **Eclair**, **LNURL**, **NWC**, and **BOLT12** backends.

For local contributor setup on macOS (Docker nginx recommended), see `docs/macos-setup.md`.

![L402 module demo](https://github.com/user-attachments/assets/3db23ab0-6025-426e-86f8-3505fa0840b9)

### ✨ Key Features

- **Server-side auto-detect** — Enable `l402_auto_detect_payment on` in `nginx.conf` and clients no longer need to include the preimage in the `Authorization` header. The module queries your Lightning node directly (LND, CLN, BOLT12, Eclair, or an NWC wallet that supports lookup) to confirm payment settlement.
- **Classic preimage flow** — Standard `L402 <macaroon>:<preimage>` header is always supported.
- **Cashu eCash support** — Accept Cashu tokens as an alternative payment method via the `X-Cashu` header.
- **Redis caching** — Settled preimages are cached in Redis to avoid repeated node lookups.
- **Multi-tenant LNURL** — Per-location LNURL addresses for multi-tenant deployments.

---

## 📖 Documentation

**Full documentation is available at: https://ngx-l402.org/docs/**

- [Installation](https://ngx-l402.org/docs/install-manual.html)
- [Docker Setup](https://ngx-l402.org/docs/install-docker.html)
- [Configuration & Environment Variables](https://ngx-l402.org/docs/config-env-vars.html)
- [Cashu eCash Support](https://ngx-l402.org/docs/cashu.html)
- [Multi-Tenant](https://ngx-l402.org/docs/config-multi-tenant.html)
- [Dry-Run (Shadow) Mode](https://ngx-l402.org/docs/dry-run.html)
- [Building from Source](https://ngx-l402.org/docs/building.html)

---

## ⚡ Quick Start

> Requires **NGINX 1.30.7+** (stable) or **1.31.9+** (mainline). Older releases,
> 1.28 and 1.29 included, lack the fixes for the HTTP/2 bugs CVE-2026-123007 and
> CVE-2026-123633, and the module logs a warning at startup when loaded into one.
>
> Pre-built binaries are published for several NGINX versions — see the assets on
> the [latest release](https://github.com/ngx-l402/ngx-l402/releases/latest). For
> anything else, build from source:
> ```
> docker build --build-arg NGX_VERSION=<your-version> -t ngx_l402 .
> ```

```bash
docker run -d \
  --name l402-nginx \
  -p 8000:8000 \
  -e LN_CLIENT_TYPE=LNURL \
  -e LNURL_ADDRESS=username@your-lnurl-server.com \
  -e ROOT_KEY=$(openssl rand -hex 32) \
  ghcr.io/ngx-l402/ngx-l402:latest
```

Test it:

```bash
curl http://localhost:8000/           # 200 OK
curl -i http://localhost:8000/protected  # 402 Payment Required
```

---

## 💬 Community

Building something paid with Lightning, or thinking about it? Come hang out with the people building ngx_l402 and the folks running it in front of their APIs and MCP servers.

- **Get unstuck fast.** Ask about setup, your Lightning backend, or an nginx config that won't behave.
- **Shape what comes next.** New features like MCP tool pricing get discussed there first, so your use case can steer them.
- **Show what you built.** Share your paid API, your pricing ideas, or a demo.
- **Find something to work on.** A good place to start if you want to contribute.

<p>
  <a href="https://discord.gg/QYwcj2wuQ"><img alt="Join ngx_l402 on Discord" src="https://img.shields.io/badge/Join%20us%20on%20Discord-5865F2?style=for-the-badge&logo=discord&logoColor=white"></a>
</p>

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENCE)
