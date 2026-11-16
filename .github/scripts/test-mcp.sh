#!/usr/bin/env bash
# Checks `l402_mcp`: discovery passes free, each tool is priced on its own,
# the token names its tool, and requests whose headers can't be trusted are
# refused. Run against a started nginx container whose image has the module;
# a second nginx is served from it on port 8002, proxying to a stub on 8003.
#
#   test-mcp.sh <container>
set -euo pipefail

container=$1
failures=0

# The stub answers in the rewrite phase, but it sits behind proxy_pass, so the
# paywalled location still runs its access phase first.
conf='pid /tmp/mcp.pid;
error_log /tmp/mcp.log notice;
load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http {
    server {
        listen 8003;
        location / { return 200 "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"; }
    }
    server {
        listen 8002;
        location /mcp {
            l402 on;
            l402_mcp on;
            l402_mcp_upstream_validates on;
            l402_amount_msat_default 1000;
            l402_mcp_tool summarize 5000;
            proxy_pass http://127.0.0.1:8003;
        }
        location /mcp-resources {
            l402 on;
            l402_mcp on;
            l402_mcp_upstream_validates on;
            l402_mcp_charge tools/call resources/read;
            l402_amount_msat_default 1000;
            proxy_pass http://127.0.0.1:8003;
        }
    }
}'
printf '%s\n' "$conf" | docker exec -i "$container" sh -c 'cat > /tmp/mcp.conf'
# Cashu off: a second instance must not open the main instance's wallet.
docker exec -e CASHU_ECASH_SUPPORT=false "$container" nginx -c /tmp/mcp.conf
trap 'docker exec "$container" nginx -c /tmp/mcp.conf -s quit || true' EXIT

curl_in() {
    docker run --rm --network "container:$container" curlimages/curl:8.11.1 \
        -s --max-time 30 "$@" || true
}

# post <path> <header>... — prints "<status> <body>".
post() {
    local path=$1
    shift
    local args=()
    for h in "$@"; do args+=(-H "$h"); done
    curl_in -X POST -H 'Content-Type: application/json' "${args[@]}" \
        -d '{"jsonrpc":"2.0","id":1}' -w ' %{http_code}' "http://127.0.0.1:8002$path" |
        awk '{status=$NF; $NF=""; print status, $0}'
}

check() {
    if [ "$1" = "$2" ]; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3: got '$1', want '$2'"
        failures=$((failures + 1))
    fi
}

V='MCP-Protocol-Version: 2026-07-28'

echo "Free and charged methods:"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/list')
check "$status" 200 "tools/list passes free"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: resources/read' 'Mcp-Name: file:///a')
check "$status" 200 "resources/read passes free by default"
read -r status _ < <(post /mcp-resources "$V" 'Mcp-Method: resources/read' 'Mcp-Name: file:///a')
check "$status" 402 "resources/read is charged when opted in"

echo "Per-tool prices:"
read -r status body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$status" 402 "tools/call is charged"
check "$(jq -r '.error.code' <<<"$body")" 402 "the 402 body is a JSON-RPC payment error"
check "$(jq 'has("id")' <<<"$body")" false "the 402 body has no id"
check "$(jq -r '.error.data.amount_msat' <<<"$body")" 1000 "a tool without its own line costs the default"
check "$(jq -r '.error.data.tool' <<<"$body")" search "the 402 body names the tool"
check "$(jq -r '.error.data.invoice | startswith("lnbc") or startswith("lntb") or startswith("lnbcrt")' <<<"$body")" \
    true "the 402 body carries the invoice"
read -r status body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: summarize')
check "$(jq -r '.error.data.amount_msat' <<<"$body")" 5000 "l402_mcp_tool sets a tool's price"
read -r status body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: =?base64?c3VtbWFyaXpl?=')
check "$(jq -r '.error.data.amount_msat' <<<"$body")" 5000 "an encoded name is decoded before pricing"

echo "Token binding:"
read -r _ body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search')
caveats=$(jq -r '.error.data.macaroon' <<<"$body" | tr '_-' '/+' |
    awk '{ while (length($0) % 4) $0 = $0 "="; print }' | base64 -d 2>/dev/null | LC_ALL=C tr -c '[:print:]' '\n')
if grep -qE 'McpTool = search$' <<<"$caveats"; then
    echo "  ✓ the macaroon carries McpTool = search"
else
    echo "  ✗ the macaroon does not carry McpTool = search"
    failures=$((failures + 1))
fi

echo "Dynamic tool price:"
docker exec redis redis-cli set mcp-tool:/mcp:search 2500 >/dev/null
read -r _ body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$(jq -r '.error.data.amount_msat' <<<"$body")" 2500 "a tool's Redis price wins"
docker exec redis redis-cli del mcp-tool:/mcp:search >/dev/null

echo "Requests the headers can't vouch for:"
read -r status body < <(post /mcp 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$status" 400 "no MCP-Protocol-Version is refused"
check "$(jq -r '.error.code' <<<"$body")" -32020 "the 400 body is a HeaderMismatch error"
read -r status _ < <(post /mcp 'MCP-Protocol-Version: 2025-11-25' 'Mcp-Method: tools/list')
check "$status" 400 "an older protocol version is refused, discovery included"
read -r status _ < <(post /mcp "$V" 'Mcp-Name: search')
check "$status" 400 "no Mcp-Method is refused"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call')
check "$status" 400 "tools/call without Mcp-Name is refused"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search' 'Mcp-Name: summarize')
check "$status" 400 "a duplicate Mcp-Name is refused"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: =?base64?!!?=')
check "$status" 400 "an undecodable Mcp-Name is refused"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: =?base64?c2VhcmNoDQpYOiB5?=')
check "$status" 400 "an Mcp-Name that decodes to CR/LF is refused"
read -r status _ < <(post /mcp 'MCP-Protocol-Version: 9999-99-99' 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$status" 400 "a protocol version that is not a date is refused"

echo "Config checks:"
# expect_bad_conf <directives> <case>
expect_bad_conf() {
    local bad="load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http { server { listen 8004; location /x { $1 } } }"
    printf '%s\n' "$bad" | docker exec -i "$container" sh -c 'cat > /tmp/mcp-bad.conf'
    if docker exec -e CASHU_ECASH_SUPPORT=false "$container" nginx -t -c /tmp/mcp-bad.conf >/dev/null 2>&1; then
        echo "  ✗ $2: nginx -t accepted it"
        failures=$((failures + 1))
    else
        echo "  ✓ $2"
    fi
}
# Every case but the first declares the upstream, so it fails for its own reason.
U='l402_mcp_upstream_validates on;'
expect_bad_conf 'l402 on; l402_mcp on; l402_amount_msat_default 1000;' "l402_mcp on without l402_mcp_upstream_validates on is rejected"
expect_bad_conf "l402_mcp on; $U l402_amount_msat_default 1000;" "l402_mcp on without l402 on is rejected"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_charge tools/list;" "an unpriceable l402_mcp_charge method is rejected"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_charge completion/complete;" "a method without Mcp-Name can't be charged"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_tool a 10; l402_mcp_tool a 20;" "a tool priced twice is rejected"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_tool a 0;" "a non-positive tool price is rejected"

if [ "$failures" -ne 0 ]; then
    echo "$failures MCP check(s) failed"
    exit 1
fi
echo "✅ MCP checks passed"
