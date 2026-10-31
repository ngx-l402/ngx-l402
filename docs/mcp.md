# MCP Tool Pricing

A remote [MCP](https://modelcontextprotocol.io) server takes every message as a
`POST` to one URL, with the tool to run in the body. With plain `l402 on;` every
message costs the same and one token opens every tool. `l402_mcp on;` prices and
binds each **tool call** instead, without adding payment code to the server:

```nginx
location /mcp {
    l402                      on;
    l402_mcp                  on;
    l402_amount_msat_default  1000;          # price of a tool without its own line
    l402_mcp_tool  summarize  5000;          # a tool with its own price
    proxy_pass                http://mcp-server;
}
```

- **Discovery is free.** `tools/list` and every other method pass through, so a
  client can see what is for sale before paying.
- **Each tool has its own price.** A tool without an `l402_mcp_tool` line costs
  the default, so a new tool is never free by accident.
- **A token buys one tool.** The macaroon carries `McpTool = <name>` next to the
  path and method, so a token bought for a cheap tool is rejected on an
  expensive one.

---

## Trust model — read this first

The module never reads the request body. It reads the tool from the headers the
2026-07-28 Streamable HTTP transport mirrors from the body:

```http
POST /mcp
MCP-Protocol-Version: 2026-07-28
Mcp-Method: tools/call
Mcp-Name: summarize
```

The same spec requires the **MCP server** to reject a request whose headers
disagree with its body (`400`, `HeaderMismatch`). That check is what stops a
client from sending `Mcp-Name: search` and calling `summarize` in the body.

So header-based pricing is only as strong as the upstream's validation:

- **The upstream must implement 2026-07-28 or later** and enforce the
  header/body check. Against a server that ignores the headers, a client can pay
  for the cheapest tool and run any other.
- The module trusts the headers only when `MCP-Protocol-Version` is
  `2026-07-28` or later. Anything older is refused (see below).

---

## How requests are handled

| Request | Result |
|---|---|
| A method not in `l402_mcp_charge` (e.g. `tools/list`) | Passes through free |
| `tools/call` with `Mcp-Name` | Charged at that tool's price; `402` until paid |
| No `MCP-Protocol-Version`, or older than `2026-07-28` | `400` |
| No `Mcp-Method` | `400` |
| `tools/call`, `resources/read` or `prompts/get` without `Mcp-Name` | `400` |
| A duplicated `Mcp-Method`, `Mcp-Name` or `MCP-Protocol-Version` | `400` |
| An `Mcp-Name` that is malformed or doesn't decode | `400` |

A `400` carries a JSON-RPC error with code `-32020` (`HeaderMismatch`) and
`"id": null`, the shape the spec uses for transport-level rejections.

A request the headers can't vouch for is **refused rather than charged at the
default price**: a default-priced token for a request that can't be tied to a
tool would let a cheap payment open an expensive tool.

`Mcp-Name` values outside plain ASCII arrive base64-encoded
(`=?base64?c3VtbWFyaXpl?=`). They are decoded before the price lookup, so an
encoded tool name still gets its own price.

`OPTIONS` preflight requests can pass through with `l402_exempt_methods OPTIONS;`,
which is checked first.

---

## Directives

| Directive | Meaning |
|---|---|
| `l402_mcp on\|off;` | Price per tool. Needs `l402 on;` in the same location — nginx refuses to start otherwise. |
| `l402_mcp_tool <name> <msat>;` | One tool's price. Repeat per tool; the name is compared exactly with the decoded `Mcp-Name`. |
| `l402_mcp_charge <method>...;` | Methods that cost money. Default `tools/call`. Also accepts `resources/read` (priced by URI) and `prompts/get`. |

All three inherit into nested locations like the other `l402_*` directives.
Everything else — Cashu, auto-detect, rate limits, dry run, tenants, realms,
`l402_indefinite_access` — works on an MCP location as on any other.

---

## Pricing and Redis

The price of a call is the first of:

1. the tool's Redis key, `mcp-tool:<path>:<tool>`
2. the tool's `l402_mcp_tool` line
3. the location's Redis key (`<path>`, as in [Redis & Dynamic Config](./config-redis.md))
4. `l402_amount_msat_default`

In the Redis key the tool name is percent-encoded except for letters, digits
and `-_.~/`:

```
SET mcp-tool:/mcp:summarize 8000
SET "mcp-tool:/mcp:get%20weather" 2000     # tool "get weather"
```

---

## The 402 response

An unpaid tool call gets `402` with the usual `WWW-Authenticate: L402 …` header
(and `X-Cashu` when Cashu is on), so existing L402 clients work unchanged. The
body is a JSON-RPC error an MCP client can read:

```json
{
  "jsonrpc": "2.0",
  "id": null,
  "error": {
    "code": -32002,
    "message": "Payment required to call summarize",
    "data": {
      "tool": "summarize",
      "amount_msat": 5000,
      "www_authenticate": "L402 macaroon=\"…\", invoice=\"lnbc…\"",
      "macaroon": "…",
      "invoice": "lnbc…"
    }
  }
}
```

`-32002` is an implementation-defined code: MCP has no standard payment error
yet. `l402_payment_html` does not apply to MCP calls; they always get JSON.

---

## Limitations

- **Older clients** (protocol versions before 2026-07-28) don't send the
  headers and are refused, discovery included. Keep a separate plain
  `l402 on;` location for them if needed. Reading the body to support them is
  planned.
- **Retries are new calls.** A client that re-sends a `tools/call` — after an
  input-required result, or a dropped response stream — needs a new payment
  under the default replay check. MRTR retries carry a new JSON-RPC `id`, so the
  module can't tell them from a fresh call.
- **stdio MCP servers** have no HTTP to sit in front of.
