# ngx_l402 — L402 Nginx Module

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

> Requires **NGINX 1.28.0** or later.
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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENCE)
