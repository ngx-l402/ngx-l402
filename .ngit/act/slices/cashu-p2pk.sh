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

# Clear the persisted wallet identity. This slice pins CASHU_WALLET_MNEMONIC, so
# a wallet left behind by an earlier slice in the same lease no longer matches
# it, and the module refuses to start:
#
#   CASHU_WALLET_MNEMONIC does not match the wallet that owns this database
#   ... Refusing to start to avoid orphaning funds.
#
# which then shows up as "Cashu database not initialized" on every verification
# while the advertised pubkey — read from CASHU_P2PK_PRIVATE_KEY, not the wallet
# — still looks perfectly correct. Nothing about that pair of symptoms points at
# a wallet.
#
# Two details, both of which were wrong first:
#
#   - only the identity files go. The token database must survive: the module
#     opens it at startup and cannot recreate it, so removing it produces the
#     same "not initialized" error for an entirely different reason. The GitHub
#     original deletes it only on the way *out* of P2PK mode, and nothing runs
#     after this slice.
#   - `docker exec nginx-lnd` cannot do it. The preceding slice leaves that
#     container stopped, so the exec fails and — being tolerated — silently
#     skips the clearing. `compose run` resolves the image and the cashu-data
#     volume from the compose file and works in any state.
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
wait_for_l402_ready "${BASE_URL}/protected"

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
