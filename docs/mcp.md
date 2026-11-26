# MCP Tool Pricing

A remote [MCP](https://modelcontextprotocol.io) server takes every message as a
`POST` to one URL, with the tool to run in the body. With plain `l402 on;` every
message costs the same and one token opens every tool. `l402_mcp on;` prices and
binds each **tool call** instead, without adding payment code to the server:

```nginx
location /mcp {
    l402                      on;
    l402_mcp                  on;
    l402_mcp_upstream_validates on;          # see "Trust model" below
    l402_amount_msat_default  1000;          # price of a tool without its own line
    l402_mcp_tool  summarize  5000;          # a tool with its own price
    l402_mcp_tool  ping       free;          # a tool that costs nothing
    proxy_pass                http://mcp-server;
}
```

- **Discovery is free.** `tools/list` and every other method pass through, so a
  client can see what is for sale before paying.
- **Each tool has its own price.** A tool without an `l402_mcp_tool` line costs
  the default, so a new tool is never free by accident.
- **A tool is free only when you say so.** `l402_mcp_tool <name> free;` lets its
  calls pass unpaid; see [Free tools](#free-tools).
- **A token buys one tool.** The macaroon carries `McpTool = <name>` and
  `McpMethod = <method>` next to the path and HTTP method, so a token bought
  for a cheap tool is rejected on an expensive one, and one bought for
  `tools/call` doesn't open `prompts/get` of the same name. With
  [`l402_realm`](./realm.md) the token is bound to the realm instead of the
  path, so it also works for the same tool and method on any other path in
  that realm.

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
  for the cheapest tool and run any other — or, when a tool is listed `free`,
  name that one and run any other without paying.
- **You declare that it does** with `l402_mcp_upstream_validates on;`. The
  module can't see whether the upstream checks, and the client's
  `MCP-Protocol-Version` can't vouch for it either, because the client chooses
  it. nginx refuses to start with `l402_mcp on` and no declaration.
- The client's version is checked as well: the headers are trusted only when
  `MCP-Protocol-Version` is a real date, `2026-07-28` or later. Anything older is
  refused (see below), unless [`l402_mcp_legacy`](#older-clients) is on: then
  it is read from the request body instead, which needs no such declaration.

---

## How requests are handled

| Request | Result |
|---|---|
| Anything but `POST` (e.g. an older client's `GET` stream or session `DELETE`) | `405`, `Allow: POST` |
| A method not in `l402_mcp_charge` (e.g. `tools/list`) | Passes through free |
| `tools/call` with `Mcp-Name` | Charged at that tool's price; `402` until paid |
| `tools/call` with the `Mcp-Name` of a tool listed `free` | Passes through free |
| No `MCP-Protocol-Version` | `400`, `-32020` |
| `MCP-Protocol-Version` older than `2026-07-28`, or not a revision date | `400`, `-32022` |
| No `Mcp-Method` | `400` |
| `tools/call`, `resources/read` or `prompts/get` without `Mcp-Name` | `400` |
| A duplicated `Mcp-Method`, `Mcp-Name` or `MCP-Protocol-Version` | `400` |
| An `Mcp-Name` that is malformed, doesn't decode, or decodes to control characters | `400` |

With `l402_mcp_legacy on;`, a request with no `MCP-Protocol-Version`, or one
older than `2026-07-28`, is handled by its body instead of the rows above; see
[Older clients](#older-clients).

Every other `400` carries a JSON-RPC error with code `-32020`
(`HeaderMismatch`). An unsupported version gets `-32022`
(`UnsupportedProtocolVersion`) with `data: {"supported": ["2026-07-28"],
"requested": "…"}`, so a client that also speaks 2026-07-28 can retry with it.
`requested` echoes the client's value only when it is a real date, and is `""`
otherwise. Neither carries an `id`, the shape the spec uses for
transport-level rejections.

A request the headers can't vouch for is **refused rather than charged at the
default price**: a default-priced token for a request that can't be tied to a
tool would let a cheap payment open an expensive tool.

`Mcp-Name` values outside plain ASCII arrive base64-encoded
(`=?base64?c3VtbWFyaXpl?=`). They are decoded before the price lookup, so an
encoded tool name still gets its own price. Only padded, standard-alphabet
base64 is accepted, the form the spec's examples use.

`OPTIONS` preflight requests can pass through with `l402_exempt_methods OPTIONS;`,
which is checked before the `405`.

---

## Directives

| Directive | Meaning |
|---|---|
| `l402_mcp on\|off;` | Price per tool. Needs `l402 on;` for the location, set there or inherited — nginx refuses to start otherwise. |
| `l402_mcp_tool <name> <msat>\|free;` | One tool's price, or `free` to let it pass unpaid. Repeat per tool; the name is compared exactly with the decoded `Mcp-Name`. |
| `l402_mcp_charge <method>...;` | Methods that cost money. Default `tools/call`. Also accepts `resources/read` (priced by URI) and `prompts/get`, the other methods that carry an `Mcp-Name`. |
| `l402_mcp_upstream_validates on\|off;` | Declares that the upstream rejects a request whose `Mcp-Method` / `Mcp-Name` disagree with its body. Required by `l402_mcp on`. |
| `l402_mcp_legacy on\|off;` | Also serve clients on protocol versions before 2026-07-28, by reading the method and the tool from the request body. Default `off`. See [Older clients](#older-clients). |

All five inherit into nested locations like the other `l402_*` directives.
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

## Free tools

Some servers mix paid tools with free ones. List the free ones by name:

```nginx
location /mcp {
    l402                      on;
    l402_mcp                  on;
    l402_mcp_upstream_validates on;
    l402_amount_msat_default  21000;         # every tool not listed below
    l402_mcp_tool  lookup     free;
    l402_mcp_tool  status     free;
    proxy_pass                http://mcp-server;
}
```

- **A free call is not a call priced at zero.** It passes like `tools/list`:
  no token, no Redis lookup, no payment backend. It still needs the same valid
  headers as a charged call, and gets the same `400` without them.
- **`free` is the only way.** A price of `0` is refused by `nginx -t`, and a
  Redis price of `0` means "no Redis price". Redis can't make a tool free, and a
  Redis price for a tool listed `free` is ignored, so changing which tools are
  free takes a reload.
- **Everything else stays paid.** There is no "free by default": a tool the
  upstream adds later costs the default until you list it.
- **The name covers every charged method.** With
  `l402_mcp_charge tools/call prompts/get;`, `l402_mcp_tool status free;` frees
  both the tool and the prompt named `status`.
- **A nested location that has its own `l402_mcp_tool` lines** replaces the
  parent's list, free tools included.

Free tools are listed in the [capability manifest](./manifest.md#mcp-routes)
with `"price": {"type": "free"}`, next to the priced ones.

A free tool raises the stakes of the [trust model](#trust-model--read-this-first):
against an upstream that doesn't reject a header/body mismatch, naming a free
tool in `Mcp-Name` runs any tool for nothing.

---

## The 402 response

An unpaid tool call gets `402` with the usual `WWW-Authenticate: L402 …` header
(and `X-Cashu` when Cashu is on), so existing L402 clients work unchanged. The
body is a JSON-RPC error an MCP client can read:

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": 402,
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

MCP defines no payment error, and the 2026-07-28 spec asks for new codes
outside the JSON-RPC server range (`-32768` to `-32000`), so the code is `402`.
`-32002` would read as "resource not found" to MCP clients. `l402_payment_html` does not apply to MCP calls; they always get JSON.

---

## Older clients

Most MCP clients in use today speak a protocol version before 2026-07-28. They
send no `Mcp-Method` or `Mcp-Name`, so on a plain `l402_mcp` location they are
refused, discovery included. `l402_mcp_legacy on;` serves them too:

```nginx
location /mcp {
    l402                        on;
    l402_mcp                    on;
    l402_mcp_upstream_validates on;
    l402_mcp_legacy             on;
    l402_amount_msat_default    21000;
    l402_mcp_tool  lookup       free;
    proxy_pass                  http://mcp-server;
}
```

**Which requests.** One with no `MCP-Protocol-Version` header (the first
`initialize` of a session has none), or with a version before `2026-07-28`. A
2026-07-28 client on the same location is read from its headers as before.

**How they are read.** nginx reads the request body, and the module takes the
JSON-RPC `method` and `params.name` (`params.uri` for `resources/read`) from
it. Prices, `free` tools, Redis and the token's `McpTool` / `McpMethod` caveats
then work exactly as for a newer client. The body goes on to the upstream
unchanged.

| Request from an older client | Result |
|---|---|
| `GET` (the session's event stream) or `DELETE` (ending the session) | Passes through free |
| A method not in `l402_mcp_charge` (`initialize`, `tools/list`, a notification) | Passes through free |
| A response to the server (no `method`) | Passes through free |
| `tools/call` for a tool listed `free` | Passes through free |
| `tools/call` for any other tool | Charged at that tool's price |
| A body that can't be read one way only (below) | `400` |
| A body larger than `client_body_buffer_size` | `413` |

**The challenge is a JSON-RPC error response, not a `402`.** An older client
reads an HTTP `402` as a broken connection, and some end the whole session on
it. So an unpaid call is answered with HTTP `200` and the JSON-RPC error for
that request: the body shown under [The 402 response](#the-402-response), plus
the request's `id`. The `WWW-Authenticate` header is still sent. A client that
can pay retries the same call with `Authorization: L402 <macaroon>:<preimage>`;
one that can't shows its user "Payment required to call …" and carries on. A
call sent without an `id` can't be answered that way and gets the `402`.

**What is refused.** The upstream parses the same body with its own JSON
parser. If the two could disagree about the method or the name, a call could be
priced as one tool and run as another, so such a body gets a `400` rather than
a guess:

| Body | JSON-RPC code |
|---|---|
| Not UTF-8 JSON, or anything after the JSON value (a byte-order mark and UTF-16 included) | `-32700` |
| A batch array, or any value that isn't one object | `-32600` |
| A key that appears twice in the request object or in `params` | `-32600` |
| `method`, `params`, `name` or `uri` in another letter case, or a non-ASCII key next to them | `-32600` |
| A charged call whose `params` isn't an object, or whose name isn't a non-empty string | `-32602` |

These limits apply to the request object and to `params` of a charged call.
What a tool receives in `arguments` is not inspected.

**Body size.** The module classifies a body only from memory. One larger than
[`client_body_buffer_size`](https://nginx.org/en/docs/http/ngx_http_core_module.html#client_body_buffer_size)
(16k by default) is spooled to disk by nginx and refused with `413`. Raise it on
the location if your tools take large arguments:

```nginx
location /mcp {
    # ...
    client_body_buffer_size 1m;
}
```

**Trust.** For these requests the module prices exactly what the upstream will
run, so nothing depends on the upstream checking headers. It depends instead on
the upstream routing by `MCP-Protocol-Version` the same way: a request without
a 2026-07-28 version must be run from its body, never from an `Mcp-Name` header.
`l402_mcp_upstream_validates on;` is still required, for the 2026-07-28 clients
the same location serves.

---

## Limitations

- **Older clients** (protocol versions before 2026-07-28) are refused, discovery
  included, unless the location sets [`l402_mcp_legacy on;`](#older-clients).
  With it, batch requests (2025-03-26 only) are still refused, and the separate
  `GET /sse` + `POST /messages` endpoints of the 2024-11-05 transport are not
  covered by one location.
- **Retries are new calls.** A client that re-sends a `tools/call` — after an
  input-required result, or a dropped response stream — needs a new payment
  under the default replay check. MRTR retries carry a new JSON-RPC `id`, and
  the state tying them to the first call (`requestState`) is in the body, so
  the module can't tell them from a fresh call. For tools that ask for input,
  let one payment cover a short window instead; the token still opens only
  its own tool and MCP method:

  ```nginx
  location /mcp {
      # ...
      l402_indefinite_access on;   # a token works until it expires
      l402_macaroon_timeout  300;  # 5 minutes: room for the retries
  }
  ```
- **stdio MCP servers** have no HTTP to sit in front of.
