#!/usr/bin/env bash
# Slice 9 — Cashu P2PK mode (NUT-11 / NUT-24).
#
# Ports "Run Integration test - Cashu P2PK mode" from
# .github/workflows/tests.yml.
#
# In the GitHub monolith this section runs inside the shared job, so it ends
# with a long restore: put nginx-lnd back in plain mode, wipe the token
# database, and re-chmod, all so the steps after it still work. None of that is
# here. The lease is discarded when the slice ends, so the teardown that the
# monolith needs to protect its successors has nothing left to protect.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../lib/env.sh
source .ngit/act/lib/env.sh
# shellcheck source=../lib/l402.sh
source .ngit/act/lib/l402.sh
# shellcheck source=../lib/lightning.sh
source .ngit/act/lib/lightning.sh

export DIAG_CONTAINERS="nginx-lnd cashu-mint lndnode-receiver redis"

# k=1 as a secp256k1 scalar; its public key is the generator point, which is
# why both values can be hard-coded and checked against each other.
P2PK_PRIVATE_KEY="0000000000000000000000000000000000000000000000000000000000000001"
EXPECTED_PUBKEY="0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

# The module derives its wallet from this, so it has to be fixed for the
# pubkey assertion below to mean anything.
export CASHU_WALLET_MNEMONIC="abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
export CASHU_P2PK_PRIVATE_KEY="$P2PK_PRIVATE_KEY"
export CASHU_P2PK_MODE="true"
# nutshell does not put NUT-12 DLEQ proofs in its tokens, so requiring them
# would reject every token the CI mint issues. Production keeps this on.
export CASHU_REQUIRE_DLEQ="false"

write_env_file
# write_env_file only knows the shared keys; P2PK is specific to this slice.
cat >> .env <<EOF
CASHU_P2PK_MODE=${CASHU_P2PK_MODE}
CASHU_P2PK_PRIVATE_KEY=${CASHU_P2PK_PRIVATE_KEY}
CASHU_REQUIRE_DLEQ=${CASHU_REQUIRE_DLEQ}
EOF

setup_lightning_backbone

# ------------------------------------------------------------------ setup --

l402_log "== starting nginx-lnd in P2PK mode"

docker exec lndnode-receiver chmod a+rx \
  /root /root/.lnd /root/.lnd/data /root/.lnd/data/chain \
  /root/.lnd/data/chain/bitcoin /root/.lnd/data/chain/bitcoin/regtest
docker exec lndnode-receiver chmod a+r \
  /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon /root/.lnd/tls.cert

# The module derives its wallet once and remembers it in wallet.fingerprint /
# wallet.mnemonic. This slice pins CASHU_WALLET_MNEMONIC to a fixed phrase, so a
# wallet left behind by an earlier slice in the same lease no longer matches
# what the environment now says it should be, and redemption fails while the
# advertised pubkey — which comes from CASHU_P2PK_PRIVATE_KEY, not the wallet —
# still looks correct. That combination is very hard to read as a wallet
# problem, so clear the wallet rather than debug it.
#
# Only the wallet identity is cleared. The token database is deliberately left
# alone: the module opens it at startup and cannot recreate it once it is gone,
# so deleting it here produces "Cashu database not initialized" on every
# verification — which looks like a P2PK bug and is not one.
#
# The GitHub original does delete that database, but only on the way *out* of
# P2PK mode, because P2PK-locked proofs are stored straight as Unspent and the
# redemption thread cannot produce a witness for them. Nothing runs after this
# slice, so there is nothing to protect from them.
# Done through a throwaway container rather than `docker exec nginx-lnd`,
# because the slice before this one leaves nginx-lnd stopped: an exec would fail
# and, being tolerated, would silently skip the clearing. The failure then
# surfaces much later as "Cashu database not initialized", because the module
# refuses to start with a mismatched wallet:
#
#   CASHU_WALLET_MNEMONIC does not match the wallet that owns this database
#   ... Refusing to start to avoid orphaning funds.
#
# `compose run` resolves the image and the cashu-data volume from the compose
# file, so this works whether or not anything is currently running.
docker compose run --rm --no-deps --entrypoint sh nginx-lnd \
  -c 'rm -f /app/data/wallet.fingerprint /app/data/wallet.mnemonic' \
  >/dev/null 2>&1 || l402_log "WARNING: could not clear the persisted wallet"

release_serving_port
docker compose up -d --build --no-deps nginx-lnd redis
wait_for_container_running nginx-lnd
# Recreating the container resets the writable layer, so this has to come after
# every `up`: nginx workers need execute permission on /root to traverse the
# lndnode-receiver volume mounted at /root/.lnd.
docker exec nginx-lnd chmod 755 /root
wait_for_http_status "${BASE_URL}/" 200 180 "nginx-lnd"

# --------------------------------------------------------- NUT-24 header ---

l402_log "== 402 carries a NUT-24 payment request"

http_request "${BASE_URL}/protected"
assert_status 402 "protected route in P2PK mode"
assert_response_matches '^X-Cashu:' "X-Cashu payment request header present"

# NUT-24 is "creq" + a version character + base64url(CBOR), and the pubkey is
# a UTF-8 text string inside the CBOR — so decoding and grepping the raw bytes
# is enough to prove the module advertised the key it will actually accept.
xcashu_value="$(grep -i '^X-Cashu:' <<< "$HTTP_RESPONSE" \
  | sed 's/^[Xx]-[Cc]ashu:[[:space:]]*//' | tr -d '\r' || true)"
[ -n "$xcashu_value" ] || fail "X-Cashu header was empty"

xcashu_cbor="$(cut -c6- <<< "$xcashu_value" \
  | tr -- '-_' '+/' \
  | awk '{l=length($0)%4; if(l==2){print $0"=="} else if(l==3){print $0"="} else {print $0}}' \
  | base64 -d 2>/dev/null || true)"

grep -qa "$EXPECTED_PUBKEY" <<< "$xcashu_cbor" \
  || { HTTP_RESPONSE="X-Cashu: ${xcashu_value}"
       fail "the payment request does not carry pubkey ${EXPECTED_PUBKEY}"; }
pass "payment request advertises the expected public key"

# ------------------------------------------------------------ acceptance ---

l402_log "== only P2PK-locked tokens are accepted"

plain_token="$(cashu_send 10)"
http_request -H "Authorization: Cashu ${plain_token}" "${BASE_URL}/protected"
assert_status 401 "unlocked token rejected in P2PK mode"

# --lock is not in every nutshell release; when it is missing there is no way
# to mint a locked token, so the acceptance half is skipped rather than failed.
locked_output="$(docker exec cashu-mint sh -c \
  "poetry run cashu send 10 --lock 'P2PK:${EXPECTED_PUBKEY}' 2>/dev/null" || true)"
locked_token="$(grep -o 'cashu[A-Za-z0-9_-]*' <<< "$locked_output" | awk 'NR==1' || true)"

if [ -z "$locked_token" ]; then
  l402_log "INFO: this nutshell build has no 'cashu send --lock'; skipping the accept case"
else
  cashu_redeem "$locked_token" "${BASE_URL}/protected"
  assert_status 200 "P2PK-locked token accepted"
fi

l402_log "== Cashu P2PK slice passed"
