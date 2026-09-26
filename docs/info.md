# Runtime Introspection

The `l402_info_endpoint` directive turns a location into a read-only
operator endpoint that reports what the module actually loaded at
runtime: the active Lightning backend, whether Redis is configured, and
the post-merge value of every per-location knob. It answers "what is this
deployment *doing*" without digging through logs.

```nginx
location = /l402/info {
    l402_info_endpoint;

    # The module does no auth here — lock it down yourself.
    allow 127.0.0.1;
    allow 10.0.0.0/8;
    deny  all;

    # Or additionally:
    # auth_basic "L402 admin";
    # auth_basic_user_file /etc/nginx/.l402_admin.htpasswd;
}
```

Default is off: no location serves the document unless the directive is
present. For stricter setups, put the location in a `server` block bound
to localhost so it is never reachable from a public listener.

Where the [capability manifest](./manifest.md) describes the instance to
*clients*, this endpoint describes it to *operators*. The manifest is
public discovery; introspection is debugging.

---

## Example response

```json
{
  "backend": "LND",
  "cashu_enabled": false,
  "locations": [
    {
      "auto_detect_payment": false,
      "default_amount_msat": 10000,
      "dry_run": false,
      "indefinite_access": false,
      "macaroon_timeout_secs": 0,
      "path": "/protected"
    },
    {
      "auto_detect_payment": false,
      "default_amount_msat": 10000,
      "dry_run": true,
      "indefinite_access": false,
      "macaroon_timeout_secs": 0,
      "path": "/shadow"
    }
  ],
  "redis_configured": true,
  "version": "1"
}
```

Only `GET` and `HEAD` are accepted; anything else returns `405`.

---

## What the document describes

| Field | Source | Meaning |
|---|---|---|
| `version` | constant `"1"` | Schema version. Bumped on breaking changes. |
| `backend` | env var `LN_CLIENT_TYPE` | Active Lightning backend: `LNURL`, `LND`, `CLN`, `NWC`, `BOLT12`, `ECLAIR`. |
| `redis_configured` | env var `REDIS_URL` | Redis URL was set **and parsed successfully** at startup. **Not** a liveness check — see below. |
| `cashu_enabled` | env var `CASHU_ECASH_SUPPORT` | Cashu ecash support is enabled. |
| `locations[].path` | `location` directive | URL path of each l402-protected route. |
| `locations[].dry_run` | `l402_dry_run` | Shadow mode: challenges are evaluated, payment is not enforced. |
| `locations[].indefinite_access` | `l402_indefinite_access` | Preimage replay check is skipped; one payment lasts the macaroon lifetime. |
| `locations[].auto_detect_payment` | `l402_auto_detect_payment` | Server settles invoices via node lookup; clients may omit the preimage. |
| `locations[].default_amount_msat` | `l402_amount_msat_default` | Static price after `merge_loc_conf`. |
| `locations[].macaroon_timeout_secs` | `l402_macaroon_timeout` | Macaroon lifetime; `0` means no expiry. |
| `locations[].manifest_hidden` | `l402_manifest_hide` | Present only when the route is hidden from the public manifest. |

Values are post-merge: an inner location that inherits or overrides an
outer scope shows the value that actually applies to it.

The schema is intentionally minimal — the knobs an operator most often
needs to confirm after a deploy. Other directives (`l402_realm`,
`l402_invoice_rate_limit`, `l402_payment_html`, …) can be added in a
future schema version without breaking this one.

Dynamic (Redis-backed) prices are **not** reflected in
`default_amount_msat` — it is the static default, same caveat as the
manifest.

---

## Security model

The endpoint maps your entire protected surface, so treat it like
`/metrics`:

- **No module-level auth.** The module deliberately does not
  authenticate; use `allow`/`deny`, `auth_basic`, mTLS, or a
  localhost-only server block. The `nginx.conf` shipped in the Docker
  image ACLs `/l402/info` to localhost.
- **No secrets, by construction.** The document never contains
  `MACAROON_FILE_PATH`, `NWC_URI`, Redis URLs or credentials, wallet
  state, balances, or preimages — only runtime status and feature flags.
  Responses carry `Cache-Control: no-store` so shared caches cannot
  retain the map of your protected surface.
- **`redis_configured` is config, not health.** It reports that
  `REDIS_URL` was set and parsed successfully at startup. Pinging Redis
  per request would block the nginx event loop, so actual liveness
  belongs in your monitoring, not here.

---

## Caveats and limitations

- **Only explicit `l402 on;` locations are listed.** Registration happens
  in the `l402` directive handler, so a location that is protected purely
  by inheritance (an `l402 on;` parent, no directive of its own) is
  paywalled but absent from `locations[]`. The capability manifest has
  the same blind spot.
- **Keep the endpoint location unpaywalled.** `l402 on;` is inherited by
  nested locations, and the access phase runs before the content handler —
  so an `/l402/info` location sitting under an `l402 on;` parent (or one
  where you add `l402 on;` directly) answers with 402 challenges instead of
  JSON. That's a valid way to sell access to your own introspection, but it
  is almost never what you want for a debugging endpoint.
- **Per-worker registry.** Like the manifest, the route registry is
  rebuilt per worker from the shared config, so every worker renders the
  same document.
- **Reload behaviour.** On `nginx -s reload`, new workers build a fresh
  registry from the new config; old workers serve in-flight requests
  with their existing registry until they exit.
