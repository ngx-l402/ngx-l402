# MCP tool pricing

Charge for each **tool** of an MCP server over Lightning, without adding
payment code to the server. ngx-l402 sits in front of the server, reads which
tool a request calls from the `Mcp-Name` header the 2026-07-28 Streamable HTTP
transport sends, and prices it:

```nginx
location /mcp {
    l402                        on;
    l402_mcp                    on;
    l402_mcp_upstream_validates on;
    l402_amount_msat_default    10000;   # 10 sats: any tool without its own line
    l402_mcp_tool  summarize    50000;   # 50 sats
    proxy_pass                  http://mcp-server:8000;
}
```

Everything runs locally: a regtest Bitcoin node, two Lightning nodes with a
channel between them, the gateway, an MCP server and an MCP client that pays.

```
client ──► gateway (nginx + ngx-l402) ──► mcp-server
  │             │
lnd-wallet ══ lnd-gateway
```

| File | What it is |
|---|---|
| [`nginx.conf`](nginx.conf) | The gateway config above. |
| [`server.py`](server.py) | A plain [MCP Python SDK](https://pypi.org/project/mcp/) server with two tools, `search` and `summarize`. No payment code. |
| [`client.py`](client.py) | The SDK's own client plus a short `httpx2.Auth` hook that pays a 402 and retries with the token. |
| [`setup.sh`](setup.sh) | Starts the stack, funds both nodes and opens the channel. |
| [`test.sh`](test.sh) | End-to-end checks: prices, payment, reuse, per-tool tokens, mismatched headers, auto-detect. |

## Run it

Needs Docker, `curl` and `jq`. The first run builds the module from this
checkout, which takes a few minutes.

```bash
cd examples/mcp-tool-pricing
./setup.sh
docker compose run --rm client
```

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

Then run the checks:

```bash
./test.sh
```

Clean up with `docker compose down -v`.

## What happens on a tool call

1. The client sends `tools/call` with `Mcp-Method: tools/call` and
   `Mcp-Name: search`. Listing tools, and every other method, passes free.
2. The gateway answers `402` with a Lightning invoice for that tool's price
   and a macaroon bound to it (`McpTool = search`). The body is a JSON-RPC
   error, so an MCP client can read it; `WWW-Authenticate` carries the same
   challenge for existing L402 clients.
3. The client pays and retries with `Authorization: L402 <macaroon>:<preimage>`.
4. The gateway checks the token and proxies the call. The MCP server checks
   that the headers match the body, and runs the tool.

A token works once, and only for its own tool: a `search` token is refused on
`summarize`. A client that sends `Mcp-Name: search` with a `summarize` body
gets past nginx, which never reads the body, and is refused by the MCP server
with `400` / `HeaderMismatch`. That server-side check is what
`l402_mcp_upstream_validates on` declares. Only set it for an upstream that
implements 2026-07-28 and enforces it, as the MCP Python SDK 2.3.0
does.

## Prices from Redis

The gateway reads a tool's price from Redis before its `l402_mcp_tool` line,
so you can change prices without reloading nginx:

```bash
docker compose exec redis redis-cli set mcp-tool:/mcp:search 20000   # 20 sats
```

## Using your own server and wallet

- **Server:** any MCP server on the 2026-07-28 transport that validates the
  mirrored headers. Point `proxy_pass` at it.
- **Gateway node:** set `LN_CLIENT_TYPE` and its variables for your backend
  (LND, CLN, LNURL, NWC, ...); see the [configuration docs](../../docs/config-env-vars.md).
  An invoice-only macaroon is enough for LND.
- **Client:** `client.py`'s `L402Auth` works with any MCP SDK client built on
  `httpx2`; swap `LndWallet` for your own wallet.

More on the directives, the trust model and the limits in
[docs/mcp.md](../../docs/mcp.md).
