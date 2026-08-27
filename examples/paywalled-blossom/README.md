# Paywalled Blossom server (ngx_l402 example)

A complete, deployable example of ngx_l402 in front of a real service: a
[Blossom](https://github.com/hzrd149/blossom) blob server where nginx terminates
TLS and returns `402` + a Lightning invoice on **both blob downloads and uploads**.
Anyone can pay to store a blob — there is no pubkey whitelist — and every blob
**auto-expires 90 days** after upload, so the server is a rolling, self-cleaning
"latest copy" store: publishers re-upload (and re-pay) to keep their copy fresh.
Useful for any pay-per-blob storage where the operator wants uploads and egress
to pay for themselves.

```
internet ──▶ nginx-l402 (TLS :443 + paywall) ──▶ blossom (:3000)
                   │
                 redis
```

- **nginx-l402** — the module; terminates TLS itself (the official nginx image
  ships `http_ssl_module`), returns `402` + invoice on blob **downloads and
  uploads** (`/upload` + `/mirror`); HEAD/list pass through untouched. Downloads
  are **realm-scoped**: one payment unlocks every blob for a 10-minute window.
- **blossom** — stores blobs on local disk; anyone may upload after paying, and
  blobs auto-expire after 90 days (`requireAuth: false` + an `expiration` rule).
- **redis** — module state (pricing, rate limits).

- **certbot** — issues the Let's Encrypt cert (standalone, port 80) and
  auto-renews it; nginx waits for the cert, then reloads every 6h to pick up
  renewals. Nothing to run or schedule by hand.

Payment mode is **LNURL** — a Lightning address, so there is **no Lightning node
to run on the host**. Sats land in whatever wallet backs the address.

## Prerequisites

- A host with **Docker + Docker Compose**, ports **80 and 443** reachable from
  the internet.
- A **domain** whose **A record points at the host** (certbot needs it to issue
  the cert).
- A **Lightning address** (sats from every paid upload/download land here).

## Configure

| File | Edit |
|---|---|
| `.env` | `ROOT_KEY`, `LNURL_ADDRESS`, `DOMAIN`, `CERTBOT_EMAIL` (copy from `.env.example`) |
| `blossom-config.yml` | `publicDomain` + retention window (`expiration`); base on the upstream example |
| `docker-compose.yml` | image names, if the defaults are unavailable (see comments) |
| `nginx.conf.template` | price / token lifetime (optional; the domain is filled from `.env`) |

```bash
cp .env.example .env
openssl rand -hex 32          # paste into ROOT_KEY in .env
# fill in LNURL_ADDRESS, DOMAIN, CERTBOT_EMAIL in .env
# set publicDomain (+ tune the 90-day `expiration` rule) in blossom-config.yml
```

## Run

```bash
docker compose up -d
```

That's it — certbot obtains the cert before nginx starts, nginx renders the
domain into its config and serves `:443`, and renewals run automatically. Certs
last 90 days and certbot renews them in the background; nothing to schedule.
Watch progress with `docker compose logs -f`.

## Verify

```bash
# A valid-looking but nonexistent blob path → should 402 (paywall hit):
curl -i https://blossom.YOURDOMAIN/0000000000000000000000000000000000000000000000000000000000000000
#   expect: HTTP/2 402  + WWW-Authenticate: L402 ...
```

Then pay for an upload, store a couple of blobs, and `GET` them: the first read
`402`s, and after payment the bytes return — **and the same token then fetches the
other blobs too**, no second payment, until the 10-minute realm window expires.

```bash
# Reuse the macaroon+preimage from the first paid read on a DIFFERENT blob:
curl -i -H "Authorization: L402 <macaroon>:<preimage>" \
  https://blossom.YOURDOMAIN/<another-sha256>
#   expect: HTTP/2 200 — the realm token covers every blob here
```

## Enable Cashu (optional)

To accept **Cashu ecash** alongside Lightning, fill the `CASHU_*` block in `.env`
(already in `.env.example` and wired into the `nginx-l402` service). The download
paywall then returns an `X-Cashu` NUT-24 payment request on its `402` **in addition
to** `WWW-Authenticate: L402`, and a client that speaks either can pay. No
`nginx.conf.template` change is needed: the existing `l402 on` location advertises
both once ecash is on.

The keys that matter:

- `CASHU_ECASH_SUPPORT=true` turns it on (empty keeps the server L402-only).
- `CASHU_WHITELISTED_MINTS` — comma-separated mints whose tokens you accept, and
  **required**: with no whitelist the server won't advertise a Cashu challenge,
  because accepting an arbitrary mint would let a forged mint pay. The example
  ships the Minibits (`https://mint.minibits.cash/Bitcoin`) and Coinos
  (`https://mint.coinos.io`) mints.
- `CASHU_WALLET_MNEMONIC` — the server's own ecash wallet seed (12 BIP39 words); it
  receives the tokens. Pin and back it up; it controls the funds.
- `CASHU_REDEEM_ON_LIGHTNING=true` melts received ecash to your configured
  Lightning backend (the LNURL address, in this example).

The wallet and replay state persist in the `cashu-data` volume. The compose
`command` chowns it to `nginx` on start (the image's own chown script only runs
under a plain `nginx` command, which a custom command like this one bypasses); if
you change the command, keep that chown or the wallet can't be written.

Verify the challenge before pointing a client at it:

```bash
curl -si https://blossom.YOURDOMAIN/<64-hex> \
  | grep -iE "HTTP/|x-cashu|www-authenticate"
#   expect a 402 with BOTH an `x-cashu: creqA…` and a `www-authenticate: L402 …` line
```

**Testing with free ecash.** Point the whitelist at a test mint and turn melting
off (fake ecash can't be melted to real Lightning), so no real sats move:

```
CASHU_WHITELISTED_MINTS=https://testnut.cashu.space
CASHU_REDEEM_ON_LIGHTNING=false
```

## Notes

- **blossom-server config** — base `blossom-config.yml` on the upstream example;
  it may need keys (database, dashboard) beyond the minimal file here. Confirm the
  `expiration` duration format and that `requireAuth: false` is supported by your
  blossom-server version — the schema evolves.
- **Macaroon scoping** — downloads use `l402_realm "blossom-read"`, so one payment
  covers **every** blob this location serves for the `l402_macaroon_timeout` window
  (10 min here) instead of one payment per blob. A client that needs N related
  blobs pays once. Note `l402_indefinite_access on` is **required**
  alongside a realm: it disables the single-use preimage check, without which the
  first request claims the preimage and the rest are rejected as replays.
- **Paid uploads** — `/upload` and `/mirror` are gated. A paying client
  (**payblob**) PUTs, gets `402`, and re-sends with payment, so a large blob
  crosses the wire **twice** unless it pre-pays via a `HEAD /upload` (BUD-06)
  preflight. Cashu is the clean path: `X-Cashu` rides its own header, so it
  coexists with a BUD-02 upload auth; L402 rides `Authorization` and only works
  when the upload carries no auth (this example's `requireAuth: false`). Storage is a flat
  90 days regardless of amount — pricing *by* payment would need ngx_l402 to pass
  a TTL to the storage layer, which it can't today.
- **Pricing is flat per blob** — a 1 KB blob and a 1 GB blob both cost 10 sats,
  and uploads are uncapped (`client_max_body_size 0`). Simple, and fine when your
  blobs are roughly one size or you trust who's paying. Set `client_max_body_size`
  if you'd rather bound what one payment can store.
- **Read and write realms don't cross** — a `blossom-read` token is rejected on the
  upload route: realms are matched exactly, and the method binding rules it out
  independently. Uploads stay per-blob on purpose (each one holds disk for 90 days);
  see `nginx.conf.template` for how to opt into a write realm instead.
- **Cashu** (optional) — see [Enable Cashu](#enable-cashu-optional) above; the
  `CASHU_*` env is already wired into `.env.example` and the `nginx-l402` service,
  defaulting to the Minibits and Coinos mints.

### Sizing the realm window

A realm token sells a **window of egress**, so the two knobs interact:

- **`l402_macaroon_timeout` must exceed your worst-case fetch.** At the ~20 MB/s
  ceiling below, 400 MB takes ~30s and 2.5 GB about 3 min — the 600s default covers
  both. Shortening the window to curb abuse backfires: the token expires mid-fetch
  and whatever is left starts `402`-ing.
- **Bound throughput instead.** `limit_conn blobconn 4` + `limit_rate 5m` caps a
  client at roughly 20 MB/s (`limit_rate` is per connection, and clients commonly
  fetch several blobs at once, so the connection cap is what actually binds).
- **Price the window, not the blob.** If 10 minutes at 20 MB/s is worth more than
  10 sats to you, raise `l402_amount_msat_default`.

## Cost

Disk holds the blobs; **egress bandwidth** is the variable cost, and is exactly
what the paywall recovers.
