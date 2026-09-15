# Redis & Dynamic Configuration

The module supports real-time configuration updates via Redis **without requiring an Nginx reload**.

## Setup

```bash
Environment=REDIS_URL=redis://127.0.0.1:6379
```

---

## Dynamic Pricing

Set the price for a specific path in Redis. Changes are picked up immediately by the next request.

```bash
# Set price to 1000 msats for /api/resource
SET /api/resource 1000

# Set price to 5000 msats for /api/premium
SET /api/premium 5000
```

> **Note**: If no Redis key exists for a path, the module falls back to `l402_amount_msat_default` in `nginx.conf`.

---

## Dynamic LNURL (Per-Tenant Routing)

Override the LNURL address for a specific request path. This takes precedence over `l402_lnurl_addr` in `nginx.conf`.

**Key format**: `lnurl:<request_path>`

```bash
# Route /api/tenant1 payments to alice
SET lnurl:/api/tenant1 alice@getalby.com

# Route /api/tenant2 payments to bob
SET lnurl:/api/tenant2 bob@getalby.com
```

> [!NOTE]
> These keys set the payout destination for a route — for Lightning invoices and
> for Cashu redemption, where a token accepted on a route is melted to the LNURL
> that route maps to. Redis is a trusted component here: bind it to a private
> interface, set `requirepass`, and limit write access to tenant configuration.

---

## Replay Attack Prevention

Redis is used to enforce single-use of L402 preimages and Cashu tokens, preventing replay attacks across distributed deployments.

```bash
Environment=REDIS_URL=redis://127.0.0.1:6379
Environment=L402_PREIMAGE_TTL_SECONDS=86400      # Default: 24 hours
Environment=L402_CASHU_TOKEN_TTL_SECONDS=86400   # Default: 24 hours
```

**How it works**: After successful verification, a SHA-256 hash of the preimage or token is stored in Redis, and reusing the credential is rejected with `401`. Protection persists across Nginx restarts and works with multiple Nginx instances.

A preimage's marker lives for `L402_PREIMAGE_TTL_SECONDS` or the macaroon's own timeout, whichever is longer. On routes with `l402_macaroon_timeout 0` it never expires, because neither does the macaroon. A Cashu token's marker lives for `L402_CASHU_TOKEN_TTL_SECONDS`.

---

## Invoice Rate Limiting

Limits how many invoices (402 responses) a single IP can request per route within a time window. This protects your Lightning node from invoice-spam without affecting clients that hold a valid token.

```nginx
location /api/resource {
    l402 on;
    l402_amount_msat_default 1000;
    l402_invoice_rate_limit 5r/m;   # 5 invoices per minute per IP
}
```

**Supported formats**:

| Value | Limit |
|---|---|
| `5r/m` | 5 per minute |
| `10r/h` | 10 per hour |
| `2r/s` | 2 per second |
| `5` | 5 per minute (shorthand) |

Requests that exceed the limit receive `429 Too Many Requests` with a `Retry-After` header set to the window duration.

The rate limit only applies to unauthenticated requests (those that would result in a 402). Requests presenting a valid L402 token bypass it entirely.

**How it works**: Uses a fixed-window Redis counter (`INCR` + `EXPIRE` on first hit) keyed by IP and path. Fails open — if Redis is unavailable, rate limiting is disabled and traffic passes through normally.

---

## Keys the Module Writes

| Key | Holds | Expires |
|---|---|---|
| `<path>` | Dynamic price you set | Never — you manage it |
| `lnurl:<path>` | Per-route LNURL override you set | Never — you manage it |
| `l402:preimage:<sha256>` | Spent Lightning preimage | See [Replay Attack Prevention](#replay-attack-prevention) |
| `l402:cashu_token:<sha256>` | Spent Cashu token | `L402_CASHU_TOKEN_TTL_SECONDS` |
| `l402:settled:<payment hash>` | Settled preimage cached by auto-detect | `L402_PREIMAGE_TTL_SECONDS` |
| `l402:invoice_rate:<sha256>` | Invoice rate-limit counter for a client and route | The rate-limit window |
| `cashu:proof_lnurl:<sha256>` | Which tenant a Cashu proof belongs to | 20 redemption intervals — see [Multi-Tenant](./config-multi-tenant.md) |

Run Redis with `maxmemory-policy noeviction`: an evicted replay marker makes its credential usable again.
