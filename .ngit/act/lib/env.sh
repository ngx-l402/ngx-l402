#!/usr/bin/env bash
# Test-stack configuration, shared by every slice.
#
# These are the same values .github/workflows/tests.yml uses. They are secrets
# in name only — a regtest root key and a mint URL that only resolves inside
# the compose network — but keeping them in one file means two slices can never
# disagree about what the stack was configured with.
#
# Written to .env as well as exported: compose interpolates ${VAR} from .env,
# and a missing variable there degrades to an empty string rather than an
# error, which is how a service ends up running with no root key at all.

# shellcheck shell=bash

# Deterministic project name. Volume names are derived from it, and one step of
# the Eclair slice removes a volume by its full name, which silently does
# nothing if the project is named after whatever directory the job checked out
# into.
export COMPOSE_PROJECT_NAME="ngx_l402"

export ROOT_KEY="a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
export LNURL_ADDRESS="hello@getalby.com"
export CURRENCY="USD"
export AMOUNT="0.01"
export CASHU_WHITELISTED_MINTS="http://cashu-mint:3338"
export CASHU_WALLET_SECRET="test_wallet_secret_for_ci"
# Empty is meaningful: the module generates a wallet when none is supplied, and
# the DB-regeneration test depends on that path.
export CASHU_WALLET_MNEMONIC=""

# Interpolated by compose for services that are only started in some slices.
export BOLT12_OFFER="${BOLT12_OFFER:-}"
export NWC_URI="${NWC_URI:-}"
export LNC_PAIRING_PHRASE="${LNC_PAIRING_PHRASE:-}"

export RUST_BACKTRACE="full"
export RUST_LOG="debug"

# The Dockerfile builds with `--mount=type=cache`, which the legacy builder
# rejects outright. Each workflow step is its own shell, so this has to be set
# where the slices read it rather than where the daemon is started.
export DOCKER_BUILDKIT=1

# Compose reads this file for ${VAR} interpolation. Regenerated each run so a
# leftover .env from a previous job cannot change what is under test.
write_env_file() {
  cat > .env <<EOF
ROOT_KEY=${ROOT_KEY}
LNURL_ADDRESS=${LNURL_ADDRESS}
CURRENCY=${CURRENCY}
AMOUNT=${AMOUNT}
CASHU_WHITELISTED_MINTS=${CASHU_WHITELISTED_MINTS}
CASHU_WALLET_SECRET=${CASHU_WALLET_SECRET}
CASHU_WALLET_MNEMONIC=${CASHU_WALLET_MNEMONIC}
BOLT12_OFFER=${BOLT12_OFFER}
NWC_URI=${NWC_URI}
LNC_PAIRING_PHRASE=${LNC_PAIRING_PHRASE}
EOF
}

# The suite talks to whichever nginx flavour is currently bound to 8000; only
# one of them runs at a time, exactly as in the GitHub original.
export BASE_URL="${BASE_URL:-http://0.0.0.0:8000}"
