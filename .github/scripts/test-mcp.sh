#!/usr/bin/env bash
# Checks `l402_mcp`: discovery passes free, each tool is priced on its own,
# a tool listed `free` passes unpaid, the manifest lists the tools, an older
# client is classified from its body under `l402_mcp_legacy`,
# the token names its tool, and requests whose headers can't be trusted are
# refused. Run against a started nginx container whose image has the module;
# a second nginx is served from it on port 8002, proxying to a stub on 8003.
#
#   test-mcp.sh <container>
#
# With a second argument, it runs only the paid checks: that container must be
# an LND node (`lncli -n regtest`) that can pay the invoices <container> issues.
#
#   test-mcp.sh <container> <payer>
set -euo pipefail

container=$1
payer=${2:-}
failures=0

# The stub answers in the rewrite phase, but it sits behind proxy_pass, so the
# paywalled location still runs its access phase first.
# `user nginx` as in the image's own config: the workers of an instance started
# without it run as `nobody`, which can't read the LND cert and macaroon.
conf='user nginx;
pid /tmp/mcp.pid;
error_log /tmp/mcp.log notice;
load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http {
    log_format body escape=none "$request_body";
    server {
        listen 8003;
        location / { return 200 "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"; }
        # One more hop for the legacy location, so this server reads the body
        # the paywall forwarded and logs it for the checks below.
        location /mcp-legacy {
            access_log /tmp/mcp-upstream.log body;
            proxy_pass http://127.0.0.1:8005;
        }
    }
    server {
        listen 8005;
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
            l402_mcp_tool ping free;
            proxy_pass http://127.0.0.1:8003;
        }
        location /mcp-resources {
            l402 on;
            l402_mcp on;
            l402_mcp_upstream_validates on;
            l402_mcp_charge tools/call resources/read;
            l402_amount_msat_default 1000;
            l402_mcp_tool file:///free free;
            proxy_pass http://127.0.0.1:8003;
        }
        location /mcp-legacy {
            l402 on;
            l402_mcp on;
            l402_mcp_upstream_validates on;
            l402_mcp_legacy on;
            l402_amount_msat_default 1000;
            l402_mcp_tool summarize 5000;
            l402_mcp_tool ping free;
            proxy_pass http://127.0.0.1:8003;
        }
        location /mcp-legacy-small {
            l402 on;
            l402_mcp on;
            l402_mcp_upstream_validates on;
            l402_mcp_legacy on;
            l402_amount_msat_default 1000;
            l402_mcp_tool ping free;
            client_body_buffer_size 1k;
            proxy_pass http://127.0.0.1:8003;
        }
        location /plain {
            l402 on;
            l402_amount_msat_default 1000;
            proxy_pass http://127.0.0.1:8003;
        }
        location = /manifest { l402_manifest; }
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

# postb <path> <body> <header>... — `post` with a body of its own.
postb() {
    local path=$1 data=$2
    shift 2
    local args=()
    for h in "$@"; do args+=(-H "$h"); done
    # `${args[@]+...}`: bash before 4.4 treats an empty array as unset under `set -u`.
    curl_in -X POST -H 'Content-Type: application/json' ${args[@]+"${args[@]}"} \
        --data-binary "$data" -w ' %{http_code}' "http://127.0.0.1:8002$path" |
        awk '{status=$NF; $NF=""; print status, $0}'
}

# call <tool> — the body of a `tools/call` for it.
call() { printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":{}}}' "$1"; }

check() {
    if [ "$1" = "$2" ]; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3: got '$1', want '$2'"
        failures=$((failures + 1))
    fi
}

V='MCP-Protocol-Version: 2026-07-28'
O='MCP-Protocol-Version: 2025-11-25'

# pay <invoice> — pays it from the payer node and prints the preimage.
pay() {
    local hash preimage=""
    # A failure here leaves the preimage empty, and the checks that use it say so.
    hash=$(docker exec "$payer" lncli -n regtest decodepayreq "$1" | jq -r '.payment_hash') || true
    docker exec "$payer" lncli -n regtest payinvoice -f "$1" >/dev/null 2>&1 || true
    for _ in $(seq 1 30); do
        preimage=$(docker exec "$payer" lncli -n regtest listpayments |
            jq -r --arg h "$hash" '.payments[] | select(.payment_hash == $h and .status == "SUCCEEDED") | .payment_preimage') || true
        [ -n "$preimage" ] && break
        sleep 1
    done
    echo "$preimage"
}

# A token bought for one tool opens that tool once and no other, for a
# 2026-07-28 client (headers) and for an older one (body) alike.
if [ -n "$payer" ]; then
    echo "Paid calls, 2026-07-28 client:"
    H=("$V" 'Mcp-Method: tools/call')
    read -r status body < <(postb /mcp "$(call search)" "${H[@]}" 'Mcp-Name: search')
    check "$status" 402 "an unpaid call is challenged"
    token="L402 $(jq -r '.error.data.macaroon' <<<"$body"):$(pay "$(jq -r '.error.data.invoice' <<<"$body")")"
    read -r status _ < <(postb /mcp "$(call summarize)" "${H[@]}" 'Mcp-Name: summarize' "Authorization: $token")
    check "$status" 401 "the token does not open another tool"
    read -r status body < <(postb /mcp "$(call search)" "${H[@]}" 'Mcp-Name: search' "Authorization: $token")
    check "$status $(jq 'has("result")' <<<"$body")" "200 true" "the paid call reaches the upstream"
    read -r status _ < <(postb /mcp "$(call search)" "${H[@]}" 'Mcp-Name: search' "Authorization: $token")
    check "$status" 401 "the token works once"

    echo "Paid calls, older client:"
    read -r status body < <(postb /mcp-legacy "$(call search)" "$O")
    check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "an unpaid call is challenged"
    token="L402 $(jq -r '.error.data.macaroon' <<<"$body"):$(pay "$(jq -r '.error.data.invoice' <<<"$body")")"
    read -r status _ < <(postb /mcp-legacy "$(call summarize)" "$O" "Authorization: $token")
    check "$status" 401 "the token does not open another tool"
    read -r status body < <(postb /mcp-legacy "$(call search)" "$O" "Authorization: $token")
    check "$status $(jq 'has("result")' <<<"$body")" "200 true" "the paid call reaches the upstream"
    check "$(docker exec "$container" tail -n 1 /tmp/mcp-upstream.log)" "$(call search)" \
        "with its body unchanged"
    read -r status _ < <(postb /mcp-legacy "$(call search)" "$O" "Authorization: $token")
    check "$status" 401 "the token works once"

    if [ "$failures" -ne 0 ]; then
        echo "$failures paid MCP check(s) failed"
        docker exec "$container" cat /tmp/mcp.log || true
        exit 1
    fi
    echo "✅ Paid MCP checks passed"
    exit 0
fi

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

echo "Free tools:"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: ping')
check "$status" 200 "a tool listed free passes unpaid"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: =?base64?cGluZw==?=')
check "$status" 200 "an encoded name is decoded before the free check"
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: pings')
check "$status" 402 "a tool that only resembles a free one is charged"
read -r status _ < <(post /mcp-resources "$V" 'Mcp-Method: resources/read' 'Mcp-Name: file:///free')
check "$status" 200 "a free name passes under another charged method"
read -r status _ < <(post /mcp 'MCP-Protocol-Version: 2025-11-25' 'Mcp-Method: tools/call' 'Mcp-Name: ping')
check "$status" 400 "a free tool is still refused on an older protocol version"
docker exec redis redis-cli set mcp-tool:/mcp:ping 2500 >/dev/null
read -r status _ < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: ping')
check "$status" 200 "a Redis price does not charge for a free tool"
docker exec redis redis-cli del mcp-tool:/mcp:ping >/dev/null
docker exec redis redis-cli set mcp-tool:/mcp:search 0 >/dev/null
read -r status body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$status $(jq -r '.error.data.amount_msat' <<<"$body")" "402 1000" "a Redis price of 0 does not make a tool free"
docker exec redis redis-cli del mcp-tool:/mcp:search >/dev/null

echo "Manifest:"
manifest=$(curl_in http://127.0.0.1:8002/manifest)
route() { jq -c --arg p "$1" '.routes[] | select(.path == $p)' <<<"$manifest"; }
check "$(route /mcp | jq -c '.mcp.tools')" \
    '[{"name":"summarize","price":{"amount_msat":5000,"type":"static"}},{"name":"ping","price":{"type":"free"}}]' \
    "an MCP route lists its tools, a free one without an amount"
check "$(route /mcp | jq -r '.price.amount_msat')" 1000 "the route price is the default for unlisted tools"
check "$(route /mcp | jq -c '.mcp.charged_methods')" '["tools/call"]' "tools/call is the charged method by default"
check "$(route /mcp-resources | jq -c '.mcp.charged_methods')" '["tools/call","resources/read"]' \
    "l402_mcp_charge methods are listed"
check "$(route /mcp | jq -c '.caveats_required | map(select(startswith("Mcp")))')" \
    '["McpTool = <NAME>","McpMethod = <MCP_METHOD>"]' "an MCP route names the MCP caveats"
check "$(route /plain | jq -c '[has("mcp"), (.caveats_required | length)]')" '[false,2]' \
    "a route without l402_mcp has no mcp block and no MCP caveats"

echo "Token binding:"
read -r _ body < <(post /mcp "$V" 'Mcp-Method: tools/call' 'Mcp-Name: search')
caveats=$(jq -r '.error.data.macaroon' <<<"$body" | tr '_-' '/+' |
    awk '{ while (length($0) % 4) $0 = $0 "="; print }' | base64 -d 2>/dev/null | LC_ALL=C tr -c '[:print:]' '\n')
if grep -qE 'McpTool = search$' <<<"$caveats" && grep -qE 'McpMethod = tools/call$' <<<"$caveats"; then
    echo "  ✓ the macaroon carries McpTool = search and McpMethod = tools/call"
else
    echo "  ✗ the macaroon does not carry McpTool = search and McpMethod = tools/call"
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
read -r status body < <(post /mcp 'MCP-Protocol-Version: 2025-11-25' 'Mcp-Method: tools/list')
check "$status" 400 "an older protocol version is refused, discovery included"
check "$(jq -r '.error.code' <<<"$body")" -32022 "with UnsupportedProtocolVersion"
check "$(jq -c '.error.data' <<<"$body")" '{"supported":["2026-07-28"],"requested":"2025-11-25"}' "naming the supported and requested versions"
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
read -r status body < <(post /mcp 'MCP-Protocol-Version: 9999-99-99' 'Mcp-Method: tools/call' 'Mcp-Name: search')
check "$status" 400 "a protocol version that is not a date is refused"
check "$(jq -r '.error.code' <<<"$body")" -32022 "as an unsupported version"
check "$(jq -r '.error.data.requested' <<<"$body")" "" "without echoing the value"

echo "Only POST:"
for m in GET DELETE; do
    status=$(curl_in -X "$m" -H "$V" -o /dev/null -w '%{http_code}' http://127.0.0.1:8002/mcp)
    check "$status" 405 "$m is refused with 405"
done
allow=$(curl_in -X GET -H "$V" -o /dev/null -D - http://127.0.0.1:8002/mcp |
    tr -d '\r' | awk -F': ' 'tolower($1) == "allow" { print $2 }')
check "$allow" POST "the 405 names POST in Allow"

echo "Older clients (l402_mcp_legacy):"
read -r status _ < <(postb /mcp-legacy '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}')
check "$status" 200 "initialize with no version header passes free"
read -r status _ < <(postb /mcp-legacy '{"jsonrpc":"2.0","method":"notifications/initialized"}' "$O")
check "$status" 200 "a notification passes free"
read -r status _ < <(postb /mcp-legacy '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' "$O")
check "$status" 200 "tools/list passes free"
read -r status _ < <(postb /mcp-legacy '{"jsonrpc":"2.0","id":3,"result":{}}' "$O")
check "$status" 200 "a response to the server passes free"
# An older client reads a `402` as a broken transport, so its challenge is the
# JSON-RPC error response to its request, over `200`.
read -r status body < <(postb /mcp-legacy "$(call search)" "$O")
check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "tools/call is charged from the body"
check "$(jq -r '.id' <<<"$body")" 1 "the challenge answers the request by its id"
check "$(jq -r '.error.data.amount_msat' <<<"$body")" 1000 "at the default price"
check "$(jq -r '.error.data.tool' <<<"$body")" search "for the tool the body names"
caveats=$(jq -r '.error.data.macaroon' <<<"$body" | tr '_-' '/+' |
    awk '{ while (length($0) % 4) $0 = $0 "="; print }' | base64 -d 2>/dev/null | LC_ALL=C tr -c '[:print:]' '\n')
if grep -qE 'McpTool = search$' <<<"$caveats" && grep -qE 'McpMethod = tools/call$' <<<"$caveats"; then
    echo "  ✓ the macaroon carries McpTool = search and McpMethod = tools/call"
else
    echo "  ✗ the macaroon does not carry McpTool = search and McpMethod = tools/call"
    failures=$((failures + 1))
fi
read -r status body < <(postb /mcp-legacy "$(call summarize)" "$O")
check "$status $(jq -r '.error.data.amount_msat' <<<"$body")" "200 5000" "l402_mcp_tool sets the price"
read -r status body < <(postb /mcp-legacy "$(call search)")
check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "a call with no version header is charged too"
read -r status body < <(postb /mcp-legacy '{"jsonrpc":"2.0","method":"tools/call","params":{"name":"search"}}' "$O")
check "$status $(jq 'has("id")' <<<"$body")" "402 false" "a call without an id is challenged with 402"
read -r status body < <(postb /mcp-legacy "$(call search)" "$O" 'Transfer-Encoding: chunked')
check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "a chunked body is read"
# `Expect: 100-continue` holds the body back until nginx asks for it, so the
# handler has to wait for the read instead of finding the body already there.
read -r status body < <(postb /mcp-legacy "$(call search)" "$O" 'Expect: 100-continue')
check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "a body that arrives after the headers is waited for"
read -r status _ < <(postb /mcp-legacy "$(call ping)" "$O" 'Expect: 100-continue')
check "$status" 200 "and a free call sent that way reaches the upstream"
read -r status _ < <(postb /mcp-legacy "$(call ping)" "$O")
check "$status" 200 "a tool listed free passes unpaid"
check "$(docker exec "$container" tail -n 1 /tmp/mcp-upstream.log)" "$(call ping)" \
    "the upstream receives the body unchanged"
# Headers an older client has no reason to send don't decide anything.
read -r status body < <(postb /mcp-legacy "$(call search)" "$O" 'Mcp-Method: tools/call' 'Mcp-Name: ping')
check "$status $(jq -r '.error.code' <<<"$body")" "200 402" "an older client's Mcp-Name header can't make a call free"
# A 2026-07-28 client on the same location is still read from its headers.
read -r status body < <(postb /mcp-legacy "$(call ping)" "$V" 'Mcp-Method: tools/call' 'Mcp-Name: summarize')
check "$status $(jq -r '.error.data.amount_msat' <<<"$body")" "402 5000" "a 2026-07-28 client is still classified by its headers"

echo "Older clients, session traffic:"
for m in GET DELETE; do
    status=$(curl_in -X "$m" -H "$O" -o /dev/null -w '%{http_code}' http://127.0.0.1:8002/mcp-legacy)
    check "$status" 200 "$m from an older client passes"
    status=$(curl_in -X "$m" -H "$V" -o /dev/null -w '%{http_code}' http://127.0.0.1:8002/mcp-legacy)
    check "$status" 405 "$m from a 2026-07-28 client is still refused"
done
status=$(curl_in -X PUT -H "$O" -o /dev/null -w '%{http_code}' http://127.0.0.1:8002/mcp-legacy)
check "$status" 405 "PUT is refused"

echo "Older clients, bodies that can't be classified:"
# bad_body <body> <json-rpc code> <case>
bad_body() {
    local status body
    read -r status body < <(postb /mcp-legacy "$1" "$O")
    check "$status $(jq -r '.error.code' <<<"$body")" "400 $2" "$3"
}
bad_body '{"method":"tools/call","params":{"name":"ping","name":"summarize"}}' -32600 "a duplicate name is refused"
bad_body '{"method":"tools/list","method":"tools/call","params":{"name":"summarize"}}' -32600 "a duplicate method is refused"
bad_body '{"method":"tools/list","Method":"tools/call","params":{"name":"summarize"}}' -32600 "a key in another letter case is refused"
bad_body "[$(call ping),$(call summarize)]" -32600 "a batch is refused"
bad_body '{"method":"tools/call","params":{}}' -32602 "a call without a name is refused"
bad_body '{"method":"tools/call"' -32700 "a body that is not JSON is refused"
bad_body '' -32700 "an empty body is refused"
pad=$(printf 'x%.0s' $(seq 1 4000))
big='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ping","arguments":{"pad":"'$pad'"}}}'
read -r status _ < <(postb /mcp-legacy "$big" "$O")
check "$status" 200 "a body within client_body_buffer_size is read"
read -r status body < <(postb /mcp-legacy-small "$big" "$O")
check "$status $(jq -r '.error.code' <<<"$body")" "413 -32600" "a body beyond client_body_buffer_size is refused, not guessed at"
# Without the directive an older client is refused as before.
read -r status body < <(postb /mcp "$(call ping)" "$O")
check "$status $(jq -r '.error.code' <<<"$body")" "400 -32022" "without l402_mcp_legacy an older client is still refused"

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
# expect_good_conf <directives> <case> — the control for the cases above: a
# rejection only means something if the same location is accepted when valid.
expect_good_conf() {
    local good="load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http { server { listen 8004; location /x { $1 } } }"
    printf '%s\n' "$good" | docker exec -i "$container" sh -c 'cat > /tmp/mcp-good.conf'
    if docker exec -e CASHU_ECASH_SUPPORT=false "$container" nginx -t -c /tmp/mcp-good.conf >/dev/null 2>&1; then
        echo "  ✓ $2"
    else
        echo "  ✗ $2: nginx -t refused it"
        failures=$((failures + 1))
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
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_legacy maybe;" "an l402_mcp_legacy value that is not on or off is rejected"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_tool a FREE;" "a tool price that is neither an amount nor free is rejected"
expect_bad_conf "l402 on; l402_mcp on; $U l402_mcp_tool a free; l402_mcp_tool a 10;" "a tool both free and priced is rejected"
# The location default: a price nginx can't use used to be logged and dropped,
# leaving the location to answer 500 or take its parent's price.
expect_good_conf 'l402 on; l402_amount_msat_default 1000;' "a positive l402_amount_msat_default is accepted"
expect_bad_conf 'l402 on; l402_amount_msat_default 0;' "l402_amount_msat_default 0 is rejected"
expect_bad_conf 'l402 on; l402_amount_msat_default -5;' "a negative l402_amount_msat_default is rejected"
expect_bad_conf 'l402 on; l402_amount_msat_default free;' "an l402_amount_msat_default that is not a number is rejected"

if [ "$failures" -ne 0 ]; then
    echo "$failures MCP check(s) failed"
    exit 1
fi
echo "✅ MCP checks passed"
