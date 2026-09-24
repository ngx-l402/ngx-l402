# Lightning Network Payments

ngx_l402 implements the [L402 protocol](https://docs.lightning.engineering/the-lightning-network/l402), enabling API monetization via **Lightning Network payments**. When a client hits a protected endpoint without a valid token, the module responds with `402 Payment Required` and a Lightning invoice. The client pays the invoice, receives a preimage, and presents it alongside the macaroon to gain access.

---

## Supported Backends

Configure the backend via the `LN_CLIENT_TYPE` environment variable:

| `LN_CLIENT_TYPE` | Description |
|---|---|
| `LND` | Lightning Network Daemon — direct gRPC connection |
| `LND` + `LNC_PAIRING_PHRASE` | Lightning Node Connect — remote LND via mailbox (no open port needed) |
| `CLN` | Core Lightning |
| `ECLAIR` | Eclair node |
| `LNURL` | Lightning Network URL — delegate invoice generation to an LNURL server |
| `NWC` | Nostr Wallet Connect — receive into any NWC wallet, no node needed |
| `BOLT12` | Reusable Lightning Offers (BOLT12) |

See [Environment Variables](./config-env-vars.md) for the full list of per-backend settings.

Each worker connects to the backend on the first request that needs it, so an unreachable node does not stop nginx from starting. While it is down, requests that need a new invoice return `500` (counted in `l402_invoices_generation_errors_total`), and so do auto-detect retries whose preimage isn't cached in Redis. A full `macaroon:preimage` credential still verifies without the node.

---

## Payment Flow

1. Client requests a protected endpoint (no auth header).
2. Module generates a macaroon and requests an invoice from the configured Lightning backend.
3. Module responds `402 Payment Required` with:
   ```
   WWW-Authenticate: L402 macaroon="<macaroon>", invoice="<bolt11>"
   ```
4. Client pays the invoice and obtains the **preimage**.
5. Client retries with:
   ```
   Authorization: L402 <macaroon>:<preimage>
   ```
6. Module verifies the macaroon + preimage and returns `200 OK`.

---

## Authorization Header Format

Two formats are accepted:

| Format | Header value | When to use |
|---|---|---|
| **Classic** | `L402 <macaroon>:<preimage_hex>` | Client has the preimage (standard wallet flow) |
| **Auto-detect** | `L402 <macaroon>` | Server queries the node; no preimage needed from client |

> The preimage in the classic format must be the **32-byte (256-bit) hex-encoded payment preimage** corresponding to the invoice's `payment_hash`.

---

## Auto-Detect Payment (Server-Side Settlement Lookup)

With **auto-detect** enabled the client only needs to send the macaroon — no preimage required. The module queries your Lightning node directly to check whether the invoice is settled and retrieves the preimage from the node.

### Enabling auto-detect

Add `l402_auto_detect_payment on` to any `location {}` block:

```nginx
location /protected {
    l402                         on;
    l402_amount_msat_default     10000;
    l402_macaroon_timeout        0;
    l402_auto_detect_payment     on;   # ← enables server-side lookup
}
```

All boolean directives (`l402`, `l402_auto_detect_payment`) accept: `on` / `off` / `true` / `false` / `1` / `0` / `yes` / `no` (case-insensitive).

### Client flow with auto-detect enabled

1. Client requests a protected endpoint → receives `402 Payment Required` with an invoice.
2. Client pays the invoice (no preimage handling needed).
3. Client retries with just the macaroon:
   ```
   Authorization: L402 <macaroon>
   ```
4. Module extracts the `payment_hash` from the macaroon identifier, queries the node, and — if the invoice is settled — uses the returned preimage to verify the macaroon signature.
5. On success the module returns `200 OK`. If the invoice is not yet settled, it returns `402 Payment Required`. If the lookup fails or takes longer than 5 seconds, it returns `500`.

### Preimage caching (Redis)

When Redis is configured (`REDIS_URL`), settled preimages are cached under the key `l402:settled:<payment_hash_hex>`. Subsequent requests for the same payment hash are served from the cache, avoiding repeated node round-trips.

### Backend support matrix

| `LN_CLIENT_TYPE` | Auto-detect supported | Notes |
|---|---|---|
| `LND` | ✅ | Uses `LookupInvoice` gRPC |
| `CLN` | ✅ | Uses `listinvoices` JSON-RPC over unix socket |
| `BOLT12` | ✅ | Uses `listinvoices` like `CLN`, so `BOLT12_OFFER` must be an offer from your own node |
| `ECLAIR` | ✅ | Uses `POST /getreceivedinfo` REST API |
| `NWC` | ⚠️ | Uses NIP-47 `lookup_invoice`, which wallets may not implement |
| `LND` over LNC | ❌ | LNC mailbox does not expose `LookupInvoice` |
| `LNURL` | ❌ | Remote wallet — no server-side query API |

> [!NOTE]
> Even when `l402_auto_detect_payment on` is set, the classic `L402 <macaroon>:<preimage>` format is still accepted — auto-detect only activates when the client omits the preimage.

---

## Wallet Compatibility

> [!NOTE]
> **NWC works for _accepting_ payments, not just paying.** A common misconception is that Nostr Wallet Connect (NIP-47) is only a spending protocol. `LN_CLIENT_TYPE=NWC` uses the connection to **receive**: it creates each L402 invoice with `make_invoice` and verifies payment with `lookup_invoice`. The catch is that both are **optional** NIP-47 methods — not every wallet implements them, and a given connection secret may be permission-scoped (e.g. pay-only). Use a wallet and connection that grant `make_invoice` **and** `lookup_invoice` (a wallet advertises its supported commands in its NIP-47 info event); a pay-only connection can spend but cannot accept.

> [!NOTE]
> The payer needs the payment's preimage: 32 bytes, 64 hex characters. Most wallets show it after paying. Some don't, notably custodial wallets when payer and payee share the provider, since the payment settles inside their database and no preimage comes back. For those payers, turn on [auto-detect](#auto-detect-payment-server-side-settlement-lookup): the gateway checks the invoice itself.

---

## Also Supported: Cashu eCash

In addition to Lightning, the module accepts **Cashu eCash tokens** via the `X-Cashu` header as an alternative payment method. See [Cashu eCash Support](./cashu.md) for details.
