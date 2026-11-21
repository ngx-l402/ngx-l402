#!/usr/bin/env bash
# End-to-end checks against the running example (run ./setup.sh first).
# Every request is a real MCP JSON-RPC call sent through the gateway to the
# MCP server; every payment is a real Lightning payment over the regtest
# channel. Needs curl and jq.
set -uo pipefail
cd "$(dirname "$0")" || exit 1

if [ ! -f .env ] || ! curl -s -o /dev/null http://localhost:8402/mcp; then
    echo "The example isn't running. Start it with ./setup.sh first." >&2
    exit 1
fi

# Undo what the checks change, even if they're interrupted: the Redis price
# override and the second nginx used for auto-detect.
cleanup() {
    docker compose exec -T redis redis-cli del mcp-tool:/mcp:search >/dev/null 2>&1
    docker compose exec -T gateway sh -c '[ -f /tmp/auto.pid ] && nginx -c /tmp/auto.conf -s quit' >/dev/null 2>&1
}
trap cleanup EXIT

GW=http://localhost:8402/mcp
V=2026-07-28
failures=0

check() {
    if [ "$1" = "$2" ]; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3: got '$1', want '$2'"
        failures=$((failures + 1))
    fi
}

# body <method> <params-json> — a JSON-RPC request with the per-request
# protocol fields 2026-07-28 requires in params._meta.
body() {
    jq -cn --arg m "$1" --argjson p "$2" --arg v "$V" \
        '{jsonrpc:"2.0", id:1, method:$m,
          params:($p + {_meta:{"io.modelcontextprotocol/protocolVersion":$v,
                               "io.modelcontextprotocol/clientCapabilities":{}}})}'
}

# mcp <url> <Mcp-Method> <Mcp-Name|""> <body> [curl args...] — sets $status
# and $out (the response body).
mcp() {
    local url=$1 method=$2 name=$3 data=$4
    shift 4
    local args=(-H "Content-Type: application/json" -H "Accept: application/json, text/event-stream"
        -H "MCP-Protocol-Version: $V" -H "Mcp-Method: $method")
    [ -n "$name" ] && args+=(-H "Mcp-Name: $name")
    out=$(curl -s -X POST "${args[@]}" "$@" -d "$data" -w '\n%{http_code}' "$url")
    status=${out##*$'\n'}
    out=${out%$'\n'*}
}

call() { body tools/call "$(jq -cn --arg n "$1" --argjson a "$2" '{name:$n, arguments:$a}')"; }
SEARCH=$(call search '{"query":"lightning"}')
SUMMARIZE=$(call summarize '{"text":"one two three"}')

# pay <invoice> — pays from the client's wallet, prints the preimage.
pay() {
    docker compose exec -T lnd-wallet lncli -n regtest payinvoice --force --json "$1" |
        jq -rs 'map(select(.status == "SUCCEEDED")) | .[0].payment_preimage'
}

# buy <tool> <body> — gets a 402 for the tool, pays it, prints the token.
buy() {
    mcp "$GW" tools/call "$1" "$2"
    local mac inv
    mac=$(jq -r '.error.data.macaroon' <<<"$out")
    inv=$(jq -r '.error.data.invoice' <<<"$out")
    echo "L402 $mac:$(pay "$inv")"
}

echo "Discovery:"
mcp "$GW" tools/list "" "$(body tools/list '{}')"
check "$status" 200 "tools/list passes free"
check "$(jq -r '[.result.tools[].name] | sort | join(",")' <<<"$out")" "search,summarize" "it lists both tools"

echo "Prices:"
mcp "$GW" tools/call search "$SEARCH"
check "$status" 402 "an unpaid tools/call gets 402"
check "$(jq -r '.error.data.amount_msat' <<<"$out")" 10000 "search costs the default, 10 sats"
mcp "$GW" tools/call summarize "$SUMMARIZE"
check "$(jq -r '.error.data.amount_msat' <<<"$out")" 50000 "summarize costs its l402_mcp_tool price, 50 sats"
docker compose exec -T redis redis-cli set mcp-tool:/mcp:search 20000 >/dev/null
mcp "$GW" tools/call search "$SEARCH"
check "$(jq -r '.error.data.amount_msat' <<<"$out")" 20000 "a Redis price overrides it"
docker compose exec -T redis redis-cli del mcp-tool:/mcp:search >/dev/null

echo "Paying:"
token=$(buy search "$SEARCH")
mcp "$GW" tools/call search "$SEARCH" -H "Authorization: $token"
check "$status" 200 "a paid token runs the tool"
check "$(jq -r '.result.content[0].text' <<<"$out")" "3 results for 'lightning': alpha, beta, gamma" "the MCP server's answer comes back"
mcp "$GW" tools/call search "$SEARCH" -H "Authorization: $token"
check "$status" 401 "the same token can't be used twice"

echo "A token buys one tool:"
token=$(buy search "$SEARCH")
mcp "$GW" tools/call summarize "$SUMMARIZE" -H "Authorization: $token"
check "$status" 401 "a search token is refused on summarize"

echo "Headers that lie about the body:"
# Paid for search, but the body asks for summarize. nginx can only see the
# header and lets it through; the MCP server compares header with body and
# refuses. This is what l402_mcp_upstream_validates on vouches for.
mcp "$GW" tools/call search "$SUMMARIZE" -H "Authorization: $token"
check "$status" 400 "the MCP server refuses a mismatched Mcp-Name"
check "$(jq -r '.error.code' <<<"$out")" -32020 "with HeaderMismatch"

echo "Requests nginx refuses itself:"
mcp "$GW" tools/call "" "$SEARCH"
check "$status" 400 "tools/call without Mcp-Name"
status=$(curl -s -o /dev/null -w '%{http_code}' "$GW")
check "$status" 405 "GET (the removed SSE stream)"

echo "Auto-detect retry (l402_auto_detect_payment):"
# A second nginx in the gateway container, on port 8001, with auto-detect on:
# a client that paid may send just the macaroon and the gateway asks its node.
docker compose exec -T gateway sh -c 'sed "s/listen 8000;/listen 8001;/; s|pid .*|pid /tmp/auto.pid;|; s|l402_mcp_upstream_validates on;|& l402_auto_detect_payment on;|" /etc/nginx/nginx.conf > /tmp/auto.conf && nginx -c /tmp/auto.conf 2>/dev/null'
# Port 8001 isn't published, so these requests run from inside the container.
auto() { docker compose exec -T gateway curl -s "$@"; }
amcp() {
    local name=$1 data=$2
    shift 2
    out=$(auto -X POST -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
        -H "MCP-Protocol-Version: $V" -H "Mcp-Method: tools/call" -H "Mcp-Name: $name" "$@" \
        -d "$data" -w '\n%{http_code}' http://localhost:8001/mcp)
    status=${out##*$'\n'}
    out=${out%$'\n'*}
}
docker compose exec -T redis redis-cli set mcp-tool:/mcp:search 30000 >/dev/null
amcp search "$SEARCH"
mac=$(jq -r '.error.data.macaroon' <<<"$out")
inv=$(jq -r '.error.data.invoice' <<<"$out")
check "$(jq -r '.error.data.amount_msat' <<<"$out")" 30000 "the challenge quotes the Redis price"
amcp search "$SEARCH" -H "Authorization: L402 $mac"
check "$status" 402 "the macaroon alone, before paying, gets a new challenge"
check "$(jq -r '.error.data.amount_msat' <<<"$out")" 30000 "which also quotes the Redis price"
pay "$inv" >/dev/null
amcp search "$SEARCH" -H "Authorization: L402 $mac"
check "$status" 200 "after paying, the macaroon alone runs the tool"

echo
if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "✅ All checks passed"
