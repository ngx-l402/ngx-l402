#!/usr/bin/env bash
# CI readiness helpers.
#
# Integration steps used to `sleep <n>` and immediately assert, which is a bet
# that startup finished within n seconds. On a loaded runner it often hasn't —
# the assertion then fails hard and takes the whole job with it. These helpers
# poll for the condition instead: they return as soon as it holds (usually much
# faster than the old sleep) and only fail after a real timeout.
#
# Usage:  source .github/scripts/ci-wait.sh

# Wait until <url> returns <expected-status>.
#   wait_http <url> <expected-status> [timeout_secs]
wait_http() {
    local url=$1 want=$2 timeout=${3:-90}
    local waited=0 code=""
    while [ "$waited" -lt "$timeout" ]; do
        # curl already prints 000 on a failed connection; don't append another.
        code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url" 2>/dev/null) || code="000"
        if [ "$code" = "$want" ]; then
            [ "$waited" -gt 0 ] && echo "  ✓ $url -> $want after ${waited}s"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "  ✗ timeout after ${timeout}s waiting for $url -> $want (last: $code)"
    return 1
}

# Wait until <container> logs match <pattern>. Use for readiness signals that
# aren't HTTP, e.g. "start worker processes".
#   wait_log <container> <pattern> [timeout_secs]
wait_log() {
    local container=$1 pattern=$2 timeout=${3:-90}
    local waited=0
    while [ "$waited" -lt "$timeout" ]; do
        if docker logs "$container" 2>&1 | grep -q "$pattern"; then
            [ "$waited" -gt 0 ] && echo "  ✓ $container logged '$pattern' after ${waited}s"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "  ✗ timeout after ${timeout}s waiting for '$pattern' in $container"
    docker logs "$container" 2>&1 | tail -30
    return 1
}

# Wait until <container> is running (and, if it has one, healthy).
#   wait_container <container> [timeout_secs]
wait_container() {
    local container=$1 timeout=${2:-90}
    local waited=0 state=""
    while [ "$waited" -lt "$timeout" ]; do
        state=$(docker inspect -f '{{.State.Status}}' "$container" 2>/dev/null || echo "missing")
        if [ "$state" = "running" ]; then
            [ "$waited" -gt 0 ] && echo "  ✓ $container running after ${waited}s"
            return 0
        fi
        if [ "$state" = "exited" ]; then
            echo "  ✗ $container exited"
            docker logs "$container" 2>&1 | tail -30
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "  ✗ timeout after ${timeout}s waiting for $container (state: $state)"
    return 1
}

# Fail if any nginx worker exited on a signal. nginx restarts the worker, so
# without this the only symptom is an empty response.
#   assert_no_crash <container>
assert_no_crash() {
    local container=$1
    local crashes
    crashes=$(docker logs "$container" 2>&1 | grep -E "worker process [0-9]+ exited on signal" || true)
    if [ -n "$crashes" ]; then
        echo "  ✗ worker exited on a signal in $container:"
        echo "$crashes" | sed 's/^/      /'
        resolve_crash_offsets "$container"
        echo "  --- last 40 log lines ---"
        docker logs "$container" 2>&1 | tail -40 | sed 's/^/      /'
        return 1
    fi
    return 0
}

# Turn the module offsets the crash handler printed into function names. The
# offset only means anything against the .so that produced it, so resolve it
# here rather than leaving it for someone to match up by hand later.
#   resolve_crash_offsets <container>
resolve_crash_offsets() {
    local container=$1 so=/tmp/${1}-l402.so offsets

    offsets=$(docker logs "$container" 2>&1 \
        | sed -n 's/.*module offset \(0x[0-9a-f]*\).*/\1/p' | sort -u)
    [ -n "$offsets" ] || return 0

    if ! docker cp "$container":/etc/nginx/modules/libngx_l402_lib.so "$so" 2>/dev/null; then
        echo "  (could not copy the module out of $container to resolve offsets)"
        return 0
    fi

    echo "  --- crash site ---"
    for off in $offsets; do
        echo "      $off  ->  $(addr2line -f -C -e "$so" "$off" 2>/dev/null | head -1)"
    done
    rm -f "$so"
}

# Retry a command until it succeeds. For inherently racy operations.
#   retry <attempts> <delay_secs> <command...>
retry() {
    local attempts=$1 delay=$2
    shift 2
    local i=1
    while [ "$i" -le "$attempts" ]; do
        if "$@"; then
            return 0
        fi
        [ "$i" -lt "$attempts" ] && echo "  retry $i/$attempts failed; waiting ${delay}s..."
        sleep "$delay"
        i=$((i + 1))
    done
    echo "  ✗ still failing after $attempts attempts: $*"
    return 1
}

# Request a route until it returns <expected-status>, for routes that mint an
# invoice from a third-party provider. Sets `response` and `status_code` so the
# caller asserts on them exactly as it would after a plain curl.
#   curl_until <expected-status> <curl args...>
curl_until() {
    local want=$1
    shift
    local i=1
    while [ "$i" -le 3 ]; do
        response=$(curl -s -i -w "\n%{http_code}" --max-time 45 "$@") || true
        status_code=$(echo "$response" | tail -n1); status_code=${status_code:-000}
        [ "$status_code" = "$want" ] && return 0
        [ "$i" -lt 3 ] && echo "  attempt $i: got $status_code, want $want; retrying..."
        sleep 5
        i=$((i + 1))
    done
    return 0
}
