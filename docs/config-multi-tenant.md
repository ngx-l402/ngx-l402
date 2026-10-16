# Multi-Tenant Configuration

The module supports **multi-tenant mode**, allowing different API routes to use different Lightning/LNURL backends. This is useful for platforms hosting multiple merchants or services, where each tenant receives payments to their own wallet.

> **Current Support**: Multi-tenant is currently supported for **Cashu eCash payments only** when using `LN_CLIENT_TYPE=LNURL`.

---

## How It Works

1. **Per-location LNURL addresses**: Name each tenant once with `l402_tenant` and point locations at it with `l402_tenant_ref`, or give a location its address directly with `l402_lnurl_addr`.
2. **Proof tracking**: When a Cashu token is received, the proofs are mapped to the tenant's LNURL address in Redis, in both standard and P2PK mode.
3. **Grouped redemption**: The automatic redemption task groups proofs by tenant and redeems each group to the correct LNURL address.

If Redis is missing or down:

- **No `REDIS_URL`**: nothing can be mapped, so every tenant's proofs are redeemed to `LNURL_ADDRESS`.
- **Redis configured but unreachable**: redemption pauses rather than guess, and the proofs stay unspent until Redis is back.

Mappings don't expire. A mapping is deleted once its proofs are swapped or melted. If that delete fails, for example because Redis blipped, the worker running redemption removes it on its next pass, along with the spent proofs in the wallet database.

> **Note**: That cleanup pass runs after each redemption cycle, so it needs `CASHU_REDEEM_ON_LIGHTNING=true`. With redemption off, mappings are still deleted when their proofs are spent, but nothing retries a failed delete.

---

## Nginx Configuration

### Named tenants

Define each tenant once at `http` level, then reference it by name:

```nginx
http {
    l402_tenant alice lnurl=alice@getalby.com;
    l402_tenant bob   lnurl=bob@getalby.com;

    server {
        location /api/alice/ { l402 on; l402_amount_msat_default 10000; l402_tenant_ref alice; }
        location /api/bob/   { l402 on; l402_amount_msat_default 15000; l402_tenant_ref bob; }
    }
}
```

A tenant name is letters, digits, `-` and `_`. `lnurl=` is the only key so far and is required.

A location that sets neither `l402_tenant_ref` nor `l402_lnurl_addr` inherits its parent's destination. Setting either one replaces it.

nginx refuses to start when:

- `l402_tenant_ref` names a tenant that no `l402_tenant` defines. The error lists the tenants that are defined.
- two `l402_tenant` lines use the same name.
- one location sets both `l402_tenant_ref` and `l402_lnurl_addr`.
- `l402_tenant_ref` is used with `CASHU_ECASH_SUPPORT=true` and an `LN_CLIENT_TYPE` other than `LNURL`. Only LNURL mode redeems Cashu per tenant, so on any other backend the tenant would be paid for Lightning and the operator would keep the Cashu.

The same backend mismatch with `l402_lnurl_addr` only logs a warning, so configs that start today keep starting.

### Addresses per location

`l402_lnurl_addr` sets an address on a single location. It is the shorter form when a tenant has only one route:

```nginx
# Tenant 1 — payments go to alice@getalby.com
location /api/tenant1 {
    l402 on;
    l402_amount_msat_default 10000;
    l402_macaroon_timeout 0;
    l402_lnurl_addr "alice@getalby.com";
}

# Tenant 2 — payments go to bob@getalby.com
location /api/tenant2 {
    l402 on;
    l402_amount_msat_default 15000;
    l402_macaroon_timeout 0;
    l402_lnurl_addr "bob@getalby.com";
}

# Tenant 3 — self-hosted LNURL server
location /api/tenant3 {
    l402 on;
    l402_amount_msat_default 5000;
    l402_macaroon_timeout 0;
    l402_lnurl_addr "user@your-lnurl-server.com";
}
```

---

## Required Environment Variables

```bash
# Use LNURL client type
Environment=LN_CLIENT_TYPE=LNURL

# Default LNURL address (fallback when l402_lnurl_addr is not set)
Environment=LNURL_ADDRESS=default@your-domain.com

# Redis is required for proof-to-tenant mapping
Environment=REDIS_URL=redis://127.0.0.1:6379

# Enable Cashu eCash support
Environment=CASHU_ECASH_SUPPORT=true
Environment=CASHU_WALLET_MNEMONIC="word1 word2 ... word12"
Environment=CASHU_WHITELISTED_MINTS=https://mint.example.com

# Enable automatic redemption to Lightning
Environment=CASHU_REDEEM_ON_LIGHTNING=true
Environment=CASHU_REDEMPTION_INTERVAL_SECS=60
```

---

## Dynamic LNURL Override via Redis

You can also override the LNURL address per path dynamically without reloading Nginx. The override takes precedence over both `l402_tenant_ref` and `l402_lnurl_addr`:

```bash
SET lnurl:/api/tenant1 alice@getalby.com
SET lnurl:/api/tenant2 bob@getalby.com
```

See [Redis & Dynamic Config](./config-redis.md) for more details.
