# MCP tool pricing

Charge for each **tool** of an [MCP](https://modelcontextprotocol.io) server
over Lightning, without adding payment code to the server. ngx-l402 sits in
front of the server as an nginx module, sees which tool a request calls, and
asks for payment before passing it on:

```nginx
location /mcp {
    l402                        on;
    l402_mcp                    on;
    l402_mcp_upstream_validates on;      # see "Why the server matters" below
    l402_amount_msat_default    10000;   # 10 sats: any tool without its own line
    l402_mcp_tool  summarize    50000;   # 50 sats

    proxy_pass         http://mcp-server:8000;
    proxy_http_version 1.1;
    proxy_buffering    off;              # tool results can stream back
}
```

Everything runs locally on a throwaway test network. No real money is
involved.

A few terms used below:

- **Lightning** is a payment network on top of Bitcoin. Two nodes with a
  **channel** between them can pay each other instantly. **regtest** is a
  private Bitcoin test chain, so the coins here are worthless.
- **sats** (satoshis) are the unit prices are quoted in. nginx takes prices in
  **msat**, millisatoshis: 1 sat = 1000 msat.
- **L402** is a way for an HTTP server to say "pay first". It answers
  `402 Payment Required` with a Lightning **invoice** and a **macaroon**, a
  signed token. Paying the invoice reveals a secret, the **preimage**. The
  client then sends `Authorization: L402 <macaroon>:<preimage>`, which proves
  it paid.

```
client ──► gateway (nginx + ngx-l402) ──► mcp-server
  │             │
lnd-wallet ══ lnd-gateway        (a Lightning channel; the client pays through it)
```

## Before you start

- **Docker** with Compose v2 (`docker compose`), plus `curl`, `jq` and
  `openssl`.
- **Port 8402** free on your machine. The gateway listens there, on
  `127.0.0.1` only.
- **A few minutes for the first run.** It builds the ngx-l402 module from
  this checkout.
- **An MCP client that speaks protocol `2026-07-28`.** ngx-l402 reads the tool
  name from the `Mcp-Name` header, which that revision added. Clients that
  announce an older revision (such as `2025-11-25`) are refused with `400` and
  code `-32022`, which lists the version they need. The client in this
  example uses the MCP Python SDK 2.3.0, which speaks 2026-07-28.

## Run it

From a clone of this repository:

```bash
cd examples/mcp-tool-pricing
./setup.sh
docker compose run --rm client
```

`setup.sh` starts everything, funds the client's Lightning wallet, opens a
channel to the gateway's node, and makes a 1-sat test payment so you know
payments go through. Then the client lists the tools for free and pays for
one call of each. It runs inside Docker, so it reaches the gateway as
`gateway:8000`; from your own machine the same gateway is
`http://localhost:8402/mcp`.

```
Connected to http://gateway:8000/mcp (protocol 2026-07-28)
Tools (listing is free): search, summarize
Calling search
  402 for search: paying 10 sats
  -> 3 results for 'lightning': alpha, beta, gamma
Calling summarize
  402 for summarize: paying 50 sats
  -> MCP servers can charge per tool call when ...
Spent 60 sats
```

To check everything the example claims, with real requests and payments:

```bash
./test.sh
```

It spends a little regtest money, sets a Redis price for `search` while it
runs (putting back any price you set yourself), and starts a second nginx on
port 8001 inside the gateway container for the auto-detect checks.

`setup.sh` is safe to run again, including after `docker compose down` and
`up`. Remove everything with `docker compose down -v`.

## What's in the folder

| File | What it is |
|---|---|
| [`nginx.conf`](nginx.conf) | The gateway config shown above. |
| [`server.py`](server.py) | A plain [MCP Python SDK](https://pypi.org/project/mcp/) server with two tools, `search` and `summarize`. It has no payment code. |
| [`client.py`](client.py) | The SDK's own client plus a short `httpx2.Auth` hook that pays a `402` and retries with the token. |
| [`docker-compose.yml`](docker-compose.yml) | Bitcoin (regtest), two LND nodes, Redis, the MCP server, the gateway and the client. |
| [`setup.sh`](setup.sh) | Starts the stack, funds the wallet, opens the channel. |
| [`test.sh`](test.sh) | 20 end-to-end checks: prices, paying, reuse, one tool per token, headers that lie about the body, auto-detect payment, and the client's spending cap. |

## What happens on a tool call

1. The client sends `tools/call` with the headers `Mcp-Method: tools/call` and
   `Mcp-Name: search`. Listing tools, and every other method, passes free.
2. The gateway answers `402` with an invoice for that tool's price and a
   macaroon bound to it: it only works for `tools/call` on `search`, at this
   path, once. The body is a JSON-RPC error that an MCP client can read, and
   the `WWW-Authenticate` header carries the same challenge for existing L402
   clients.
3. The client pays the invoice and sends the same request again with the
   token.
4. The gateway checks the token and passes the call to the MCP server, which
   runs the tool.

## Why the server matters

nginx never reads the request body. It prices the call by the `Mcp-Name`
header, but the server runs whatever tool the body names. So a client could
send `Mcp-Name: search` (10 sats) with a body that calls `summarize`
(50 sats).

The 2026-07-28 spec closes that gap on the server side: a server must refuse
a request whose headers don't match its body. `test.sh` shows it happening:
nginx lets the request through, and the MCP server answers
`400` / `HeaderMismatch`.

`l402_mcp_upstream_validates on` is you telling ngx-l402 that your server does
this check. nginx won't start `l402_mcp` without it. Only set it for a server
that implements 2026-07-28 and enforces the check, as the MCP Python SDK
2.3.0 does.

## Keep these if you copy it

- **Your own `ROOT_KEY`.** The gateway signs tokens with it, and anyone who
  knows it can create tokens that pass without paying. `setup.sh` writes a
  random one to `.env`, which is gitignored, and compose won't start the
  gateway without one.
- **Local only.** The gateway is published on `127.0.0.1:8402`, not on every
  network interface. Nothing else in the stack is published.
- **Redis that keeps its data.** The gateway records every spent token in
  Redis; that record is what makes a token work only once. Here Redis has a
  volume, append-only persistence, and never evicts keys, so a spent token
  stays spent across restarts and under memory pressure.
- **Something to build from.** `docker-compose.yml` builds the gateway from
  this checkout (`context: ../..`). A copy of the folder on its own needs
  `context` pointed at an ngx-l402 checkout, or a published image with
  `l402_mcp`.
- **A spending cap.** `client.py` refuses an invoice above `MAX_SATS` (default
  100 per call) instead of paying whatever the server asks. Try it with
  `docker compose run --rm -e MAX_SATS=20 client`: it pays for `search` and
  stops at `summarize`.

## Changing prices

A tool's price can also come from Redis. It wins over the tool's
`l402_mcp_tool` line, or over the default for a tool without one, and takes
effect without reloading nginx:

```bash
docker compose exec redis redis-cli set mcp-tool:/mcp:search 20000   # 20 sats
docker compose exec redis redis-cli del mcp-tool:/mcp:search         # back to 10
```

Redis prices are in msat, like the ones in `nginx.conf`.

## Using your own server and wallet

- **Server:** any MCP server on the 2026-07-28 transport that checks the
  headers against the body. Point `proxy_pass` at it. If you keep the SDK's
  DNS-rebinding protection, the server's `allowed_hosts` must include the
  host nginx proxies to (`mcp-server:8000` here), or it refuses every request.
- **Gateway node:** set `LN_CLIENT_TYPE` and its variables for your backend
  (LND, CLN, LNURL, NWC and others); see the
  [configuration docs](../../docs/config-env-vars.md). For LND, an
  invoice-only macaroon is enough: the gateway issues and checks invoices,
  it never spends.
- **Client:** `L402Auth` in `client.py` works with any client built on
  `httpx2`, which the MCP Python SDK uses. Swap `LndWallet` for your own
  wallet, and give it a macaroon or key that can only pay. To run
  `client.py` outside Docker, set `MCP_URL`, `LND_REST_URL` and `LND_DIR` for
  your own LND node: this example's wallet node isn't reachable from your
  machine, and its files live in a Docker volume.

## If something goes wrong

| What you see | What to do |
|---|---|
| `required variable ROOT_KEY is missing` | Run `./setup.sh` first; it creates `.env`. |
| `port is already allocated` on 8402 | Free the port, or change `127.0.0.1:8402` in `docker-compose.yml`, `GW` in `test.sh`, and the URL `setup.sh` prints. |
| `timed out waiting for ...` from `setup.sh` | Run it again. If it keeps happening, look at `docker compose logs lnd-wallet lnd-gateway`. |
| `400` with code `-32022` | The client speaks an MCP protocol older than 2026-07-28. Upgrade it. |
| `400` with code `-32020` | A header is missing or malformed, or (from the MCP server) doesn't match the body. |
| `Not paying: ... more than MAX_SATS` | The tool costs more than the client's cap. Raise `MAX_SATS` if that's expected. |
| Client changes don't show up | `setup.sh` builds the client image; after editing `client.py` without it, run `docker compose --profile client build client`. |

More on the directives, the trust model and the limits in
[docs/mcp.md](../../docs/mcp.md).
