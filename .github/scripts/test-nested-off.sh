#!/usr/bin/env bash
# Checks that `l402 off;` and `l402_auto_detect_payment off;` in a nested
# location override `on` from the enclosing one, while a nested location with
# no directive of its own still inherits. Run against a started nginx container
# whose image has the module; a second nginx is served from it on port 8001.
#
#   test-nested-off.sh <container>
set -euo pipefail

container=$1
failures=0

# The free locations serve a file rather than `return`: `return` answers in the
# rewrite phase, before the access-phase paywall runs, so it would pass even
# with the paywall still on.
conf='pid /tmp/nested-off.pid;
error_log /tmp/nested-off.log notice;
load_module /etc/nginx/modules/libngx_l402_lib.so;
events {}
http {
    server {
        listen 8001;
        root /usr/share/nginx/html;
        location /paid {
            l402 on; l402_amount_msat_default 10000;
            l402_auto_detect_payment on;
            try_files /index.html =404;
            location /paid/inherit { try_files /index.html =404; }
            location /paid/free {
                l402 off;
                try_files /index.html =404;
                location /paid/free/paid-again { l402 on; try_files /index.html =404; }
            }
            location /paid/no-detect { l402 on; l402_auto_detect_payment off; }
        }
        location = /l402/info { l402_info_endpoint; }
    }
}'
printf '%s\n' "$conf" | docker exec -i "$container" sh -c 'cat > /tmp/nested-off.conf'
# Cashu off: a second instance must not open the main instance's wallet.
docker exec -e CASHU_ECASH_SUPPORT=false "$container" nginx -c /tmp/nested-off.conf
trap 'docker exec "$container" nginx -c /tmp/nested-off.conf -s quit || true' EXIT

status() {
    docker run --rm --network "container:$container" curlimages/curl:8.11.1 \
        -s -o /dev/null -w '%{http_code}' --max-time 30 "http://127.0.0.1:8001$1" || true
}

# expect_status <path> <code> <case>
expect_status() {
    local got
    got=$(status "$1")
    if [ "$got" = "$2" ]; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3: GET $1 returned $got, want $2"
        failures=$((failures + 1))
    fi
}

echo "Paywall checks:"
expect_status /paid 402 "the parent location charges"
expect_status /paid/inherit 402 "a nested location with no directive inherits l402 on"
expect_status /paid/free 200 "a nested l402 off frees the location"
expect_status /paid/free/paid-again 402 "l402 on below an l402 off charges again"

# /l402/info lists each registered route with its post-merge settings.
echo "Auto-detect checks:"
info=$(docker run --rm --network "container:$container" curlimages/curl:8.11.1 \
    -s --max-time 30 http://127.0.0.1:8001/l402/info)
detect=$(jq -r '.locations[] | "\(.path)=\(.auto_detect_payment)"' <<<"$info" | sort | tr '\n' ' ')
want="/paid/free/paid-again=true /paid/no-detect=false /paid=true "
if [ "$detect" = "$want" ]; then
    echo "  ✓ a nested l402_auto_detect_payment off overrides the parent, and on is inherited"
else
    echo "  ✗ auto_detect_payment per route: got '$detect', want '$want'"
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures nested-off check(s) failed"
    exit 1
fi
echo "✅ Nested l402 off checks passed"
