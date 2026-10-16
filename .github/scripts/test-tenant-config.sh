#!/usr/bin/env bash
# Checks for `l402_tenant` / `l402_tenant_ref`, run against a started nginx
# container whose image has the module.
#
# The fixtures live here rather than in nginx.conf because that file is shared
# with nginx-lnd, which runs LND with Cashu on, and a tenant ref is refused
# there by design.
#
#   test-tenant-config.sh <container>
set -euo pipefail

container=$1
failures=0

# A whole nginx.conf around the given `http {}` contents.
wrap() {
    printf 'load_module /etc/nginx/modules/libngx_l402_lib.so;\nevents {}\nhttp {\n%s\n}\n' "$1"
}

# expect <ok|fail> <case> <pattern> <http-body> [VAR=value...]
#
# Runs `nginx -t` on the config with the given environment overrides and checks
# the outcome. <pattern> must appear in the output either way; for ok cases
# that is how a warning is asserted, and "successful" does for a clean load.
expect() {
    local want=$1 name=$2 pattern=$3 body=$4
    shift 4
    local env_args=()
    for kv in "$@"; do env_args+=(-e "$kv"); done

    wrap "$body" | docker exec -i "$container" sh -c 'cat > /tmp/tenant-test.conf'
    local out status=0
    out=$(docker exec ${env_args[@]+"${env_args[@]}"} "$container" \
        nginx -t -c /tmp/tenant-test.conf 2>&1) || status=$?

    local got=ok
    [ "$status" -ne 0 ] && got=fail
    if [ "$got" = "$want" ] && grep -qF -- "$pattern" <<<"$out"; then
        echo "  ✓ $name"
    else
        echo "  ✗ $name: expected $want with '$pattern', got $got:"
        sed 's/^/      /' <<<"$out"
        failures=$((failures + 1))
    fi
}

echo "Config-load checks:"

expect ok "a location may reference a tenant defined after it" "successful" '
    server { listen 8001; location /a { l402 on; l402_tenant_ref alice; } }
    l402_tenant alice lnurl=alice@getalby.com;'

expect ok "a nested location inherits its parent tenant" "successful" '
    l402_tenant alice lnurl=alice@getalby.com;
    server { listen 8001; location /a { l402_tenant_ref alice; location /a/b { l402 on; } } }'

expect ok "a child l402_lnurl_addr replaces the parent tenant" "successful" '
    l402_tenant alice lnurl=alice@getalby.com;
    server { listen 8001; location /a { l402_tenant_ref alice;
        location /a/b { l402 on; l402_lnurl_addr bob@example.org; } } }'

expect fail "an unknown tenant names the ones that exist" \
    'tenant "carol" is not defined; defined tenants: alice, bob' '
    l402_tenant alice lnurl=alice@getalby.com;
    l402_tenant bob lnurl=bob@example.org;
    server { listen 8001; location /c { l402 on; l402_tenant_ref carol; } }'

expect fail "a tenant defined twice is refused" 'tenant "alice" is already defined' '
    l402_tenant alice lnurl=alice@getalby.com;
    l402_tenant alice lnurl=mallory@example.org;'

expect fail "a misspelt key is refused" 'unknown key "lnrul"' '
    l402_tenant alice lnrul=alice@getalby.com;'

expect fail "l402_tenant is http-level only" '"l402_tenant" directive is not allowed here' '
    server { listen 8001; location /a { l402_tenant alice lnurl=alice@getalby.com; } }'

expect fail "a tenant and an address in one location are refused" "are both set in one location" '
    l402_tenant alice lnurl=alice@getalby.com;
    server { listen 8001; location /a { l402 on; l402_tenant_ref alice;
        l402_lnurl_addr bob@example.org; } }'

expect fail "a tenant on a node backend with Cashu on is refused" "needs LN_CLIENT_TYPE=LNURL" '
    l402_tenant alice lnurl=alice@getalby.com;
    server { listen 8001; location /a { l402 on; l402_tenant_ref alice; } }' \
    LN_CLIENT_TYPE=LND CASHU_ECASH_SUPPORT=true

expect ok "a tenant on a node backend without Cashu loads" "successful" '
    l402_tenant alice lnurl=alice@getalby.com;
    server { listen 8001; location /a { l402 on; l402_tenant_ref alice; } }' \
    LN_CLIENT_TYPE=LND CASHU_ECASH_SUPPORT=false

expect ok "l402_lnurl_addr on a node backend with Cashu on only warns" \
    "Cashu paid on this route is redeemed to the operator" '
    server { listen 8001; location /a { l402 on; l402_lnurl_addr bob@example.org; } }' \
    LN_CLIENT_TYPE=LND CASHU_ECASH_SUPPORT=true

# A ref must pay the tenant's own address, not fall back to LNURL_ADDRESS.
# `ghost` points at a domain that cannot resolve: if it fell back, it would
# get a working invoice from the global address like `alby` does.
echo "Routing checks:"
routing_conf='pid /tmp/tenant-routing.pid;
error_log /tmp/tenant-routing.log notice;
load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http {
    l402_tenant alby  lnurl=hello@getalby.com;
    l402_tenant ghost lnurl=nobody@ngx-l402.invalid;
    server {
        listen 8001;
        location /alby  { l402 on; l402_amount_msat_default 10000; l402_tenant_ref alby; }
        location /ghost { l402 on; l402_amount_msat_default 10000; l402_tenant_ref ghost; }
        location /legacy {
            l402 on; l402_amount_msat_default 10000; l402_lnurl_addr hello@getalby.com;
            location /legacy/child    { l402 on; }
            location /legacy/override { l402 on; l402_lnurl_addr other@getalby.com; }
        }
        location = /.well-known/l402-services { l402_manifest; }
    }
}'
printf '%s\n' "$routing_conf" | docker exec -i "$container" sh -c 'cat > /tmp/tenant-routing.conf'
# Cashu off: a second instance must not open the main instance's wallet.
docker exec -e CASHU_ECASH_SUPPORT=false "$container" nginx -c /tmp/tenant-routing.conf

fetch() {
    docker run --rm --network "container:$container" curlimages/curl:8.11.1 \
        -s -i --max-time 30 "http://127.0.0.1:8001$1" || true
}

alby=$(fetch /alby)
if head -n1 <<<"$alby" | grep -q ' 402' && grep -qi '^WWW-Authenticate: L402 .*invoice="ln' <<<"$alby"; then
    echo "  ✓ a tenant ref gets an invoice from its tenant"
else
    echo "  ✗ /alby: expected 402 with an L402 invoice, got:"
    sed 's/^/      /' <<<"$alby"
    failures=$((failures + 1))
fi

# The manifest shows each route's post-merge address. `l402_lnurl_addr`
# inheritance shares its merge branch with tenant refs, so pin it here.
routes=$(fetch /.well-known/l402-services | sed -n '/^{/,$p' |
    jq -r '.routes[] | "\(.path)=\(.lnurl_addr)"' | sort | tr '\n' ' ')
want="/alby=hello@getalby.com /ghost=nobody@ngx-l402.invalid /legacy/child=hello@getalby.com /legacy/override=other@getalby.com /legacy=hello@getalby.com "
if [ "$routes" = "$want" ]; then
    echo "  ✓ every route reports the address it inherits or sets"
else
    echo "  ✗ manifest addresses: got '$routes', want '$want'"
    failures=$((failures + 1))
fi

ghost=$(fetch /ghost)
if grep -qi '^WWW-Authenticate: L402' <<<"$ghost"; then
    echo "  ✗ /ghost was issued an invoice, so it did not use its tenant's address:"
    sed 's/^/      /' <<<"$ghost"
    failures=$((failures + 1))
else
    echo "  ✓ a tenant ref uses its own address, not LNURL_ADDRESS"
fi

# Each parse re-runs every `l402_tenant`, so a reload only succeeds if the
# previous cycle's tenants were cleared first. A failed reload keeps the old
# config serving, so the log is the only place it shows.
docker exec "$container" nginx -c /tmp/tenant-routing.conf -s reload
for _ in $(seq 1 10); do
    # Old workers only shut down once the new config has loaded.
    docker exec "$container" grep -q 'gracefully shutting down\|\[emerg\]' /tmp/tenant-routing.log && break
    sleep 1
done
if docker exec "$container" grep -q '\[emerg\]' /tmp/tenant-routing.log; then
    echo "  ✗ reloading the same tenants failed:"
    docker exec "$container" grep '\[emerg\]' /tmp/tenant-routing.log | sed 's/^/      /'
    failures=$((failures + 1))
else
    echo "  ✓ a reload redefines the same tenants"
fi

docker exec "$container" nginx -c /tmp/tenant-routing.conf -s quit

if [ "$failures" -ne 0 ]; then
    echo "$failures tenant check(s) failed"
    exit 1
fi
echo "✅ Named tenant checks passed"
