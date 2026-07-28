#!/usr/bin/env bash
# Start a Docker daemon *inside* the act job container.
#
# The integration suite talks to compose services over published ports
# (curl http://0.0.0.0:8000/) and reaches into them with `docker exec`. Both
# only behave like they do on a GitHub runner if the daemon shares the job's
# network namespace, so the daemon has to live in the job container rather
# than on the sandbox host. That is what --privileged buys, and why every
# integration workflow asks the coordinator for it.
#
# Binaries come from download.docker.com as pinned, checksummed tarballs
# rather than apt: the act image already ships a docker CLI, and installing
# docker.io over it fails on a dpkg file conflict.
#
# Sourcing this file gives you `start_docker_daemon`; running it directly
# does the same thing.

set -euo pipefail

DOCKER_STATIC_VERSION="${DOCKER_STATIC_VERSION:-27.5.1}"
COMPOSE_VERSION="${COMPOSE_VERSION:-v2.29.1}"
BUILDX_VERSION="${BUILDX_VERSION:-v0.19.3}"

# sha256 of the artefacts below. A silent mismatch here would mean running an
# unknown daemon as root inside the job, so a mismatch is fatal, never a warning.
DOCKER_SHA256_x86_64="4f798b3ee1e0140eab5bf30b0edc4e84f4cdb53255a429dc3bbae9524845d640"
DOCKER_SHA256_aarch64="e6b53725a73763ab3f988c73f8772eaed429754c1a579db5ff11f21990fd1817"
COMPOSE_SHA256_x86_64="5ea89dd65d33912a83737d8a4bf070d5de534a32b8493a21fbefc924484786a9"
COMPOSE_SHA256_aarch64="7f0023ba726b90347e4ebc1d94ec5970390b8bddb86402c0429f163dca70d745"
BUILDX_SHA256_x86_64="32ed111e941e385c2fb8261eba06a4056915718fd606f8278834ac1931d261a2"
BUILDX_SHA256_aarch64="138b587399b27bb61945a36d67866177b85dea1155101a2be63c7ab715f18a2e"

# Diagnostics go to stderr so functions that return a value through stdout
# stay usable in a command substitution.
log() { printf '[docker-in-job] %s\n' "$*" >&2; }
fail() { printf '[docker-in-job] ERROR: %s\n' "$*" >&2; exit 1; }

# Downloads that cross the public internet get three tries: a single TCP reset
# during setup should not cost a 40-minute pipeline run.
fetch() {
  local url="$1" dest="$2" attempt
  for attempt in 1 2 3; do
    if curl -fsSL --connect-timeout 20 --max-time 300 -o "$dest" "$url"; then
      return 0
    fi
    log "download failed (attempt ${attempt}/3): ${url}"
    sleep $((attempt * 5))
  done
  fail "could not download ${url}"
}

verify_sha256() {
  local file="$1" expected="$2" actual
  actual="$(sha256sum "$file" | cut -d' ' -f1)"
  [ "$actual" = "$expected" ] || fail "checksum mismatch for ${file}: got ${actual}, expected ${expected}"
}

# Tools the slices assume exist. A GitHub runner has all of them and so do the
# fuller act images, which is exactly why their absence is worth handling here:
# a missing `jq` does not announce itself, it turns every readiness poll that
# parses node output into a wait that runs to its timeout and reports the wrong
# cause.
#
# iptables is in the list because dockerd publishes ports by writing NAT rules;
# without it every `curl http://0.0.0.0:8000/` fails with a connection refused
# that looks like an application bug.
ensure_runtime_prereqs() {
  local tool missing=()
  for tool in iptables curl jq tar; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
  done
  [ "${#missing[@]}" -eq 0 ] && return 0

  log "installing ${missing[*]}"
  apt-get update -qq >/dev/null 2>&1 || true
  apt-get install -y -qq "${missing[@]}" >/dev/null 2>&1 || true

  for tool in "${missing[@]}"; do
    command -v "$tool" >/dev/null 2>&1 \
      || fail "${tool} is required by the integration slices and could not be installed"
  done
}

install_docker_binaries() {
  local arch tarball docker_sha compose_sha buildx_sha buildx_arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64)
      docker_sha="$DOCKER_SHA256_x86_64"
      compose_sha="$COMPOSE_SHA256_x86_64"
      buildx_sha="$BUILDX_SHA256_x86_64"
      buildx_arch="amd64"
      ;;
    aarch64)
      docker_sha="$DOCKER_SHA256_aarch64"
      compose_sha="$COMPOSE_SHA256_aarch64"
      buildx_sha="$BUILDX_SHA256_aarch64"
      buildx_arch="arm64"
      ;;
    *) fail "unsupported architecture: ${arch}" ;;
  esac

  if command -v dockerd >/dev/null 2>&1; then
    log "dockerd already present: $(dockerd --version)"
  else
    log "installing docker ${DOCKER_STATIC_VERSION} static binaries (${arch})"
    tarball="$(mktemp -d)/docker.tgz"
    fetch "https://download.docker.com/linux/static/stable/${arch}/docker-${DOCKER_STATIC_VERSION}.tgz" "$tarball"
    verify_sha256 "$tarball" "$docker_sha"
    # --strip-components drops the leading docker/ directory; the tarball holds
    # dockerd, containerd, runc, docker-proxy and the CLI.
    tar -xzf "$tarball" --strip-components=1 -C /usr/local/bin
    rm -rf "$(dirname "$tarball")"
  fi

  if docker compose version >/dev/null 2>&1; then
    log "compose plugin already present: $(docker compose version --short)"
  else
    log "installing compose ${COMPOSE_VERSION} (${arch})"
    mkdir -p /usr/local/lib/docker/cli-plugins
    local compose_arch="$arch"
    fetch "https://github.com/docker/compose/releases/download/${COMPOSE_VERSION}/docker-compose-linux-${compose_arch}" \
      /usr/local/lib/docker/cli-plugins/docker-compose
    verify_sha256 /usr/local/lib/docker/cli-plugins/docker-compose "$compose_sha"
    chmod +x /usr/local/lib/docker/cli-plugins/docker-compose
  fi

  # The Dockerfile builds with `--mount=type=cache`, which needs BuildKit.
  # `docker compose build` ships its own BuildKit client, but the plain
  # `docker build` the compatibility slice uses does not: without the buildx
  # plugin the CLI silently drops to the legacy builder and the build fails on
  # the first cache mount.
  if docker buildx version >/dev/null 2>&1; then
    log "buildx already present: $(docker buildx version)"
  else
    log "installing buildx ${BUILDX_VERSION} (${buildx_arch})"
    mkdir -p /usr/local/lib/docker/cli-plugins
    fetch "https://github.com/docker/buildx/releases/download/${BUILDX_VERSION}/buildx-${BUILDX_VERSION}.linux-${buildx_arch}" \
      /usr/local/lib/docker/cli-plugins/docker-buildx
    verify_sha256 /usr/local/lib/docker/cli-plugins/docker-buildx "$buildx_sha"
    chmod +x /usr/local/lib/docker/cli-plugins/docker-buildx
  fi

  # The slices all use `docker compose`, but tests.yml and anything copied out
  # of it use the v1 spelling; shim it so a command pasted from the GitHub
  # workflow behaves the same here.
  if ! command -v docker-compose >/dev/null 2>&1; then
    printf '#!/bin/sh\nexec docker compose "$@"\n' > /usr/local/bin/docker-compose
    chmod +x /usr/local/bin/docker-compose
  fi
}

# overlay2 refuses to stack on the overlayfs an act job container runs on, and
# the daemon then silently falls back to vfs — which copies every layer in full
# instead of sharing it. Measured on this stack that is the difference between
# a couple of gigabytes and tens of them, on a lease being paid for by the
# second, so it is worth some effort to avoid.
#
# A loopback ext4 image inside the job gives overlay2 a filesystem it accepts
# without needing the coordinator to mount anything. The file is sparse, so it
# costs what is written, not DOCKER_STORE_SIZE.
DOCKER_STORE_SIZE="${DOCKER_STORE_SIZE:-24G}"
DOCKER_STORE_IMAGE="${DOCKER_STORE_IMAGE:-/docker-store.img}"

prepare_data_root() {
  local data_root="${CI_DOCKER_DATA_ROOT:-/var/lib/docker}"
  mkdir -p "$data_root"

  # An explicit data root means the caller mounted something already.
  if [ -n "${CI_DOCKER_DATA_ROOT:-}" ]; then
    log "using caller-provided data root ${data_root}"
    printf '%s' "$data_root"
    return 0
  fi

  local fstype
  fstype="$(stat -f -c %T "$data_root" 2>/dev/null || echo unknown)"
  if [ "$fstype" != "overlayfs" ] && [ "$fstype" != "overlay" ]; then
    printf '%s' "$data_root"
    return 0
  fi

  # Falling back to vfs is slow but correct, so every failure here is a log
  # line rather than a dead pipeline.
  if ! command -v mkfs.ext4 >/dev/null 2>&1; then
    apt-get update -qq >/dev/null 2>&1 || true
    apt-get install -y -qq e2fsprogs >/dev/null 2>&1 || true
  fi
  if ! command -v mkfs.ext4 >/dev/null 2>&1; then
    log "no mkfs.ext4 available; docker will run on vfs"
    printf '%s' "$data_root"
    return 0
  fi

  log "backing ${data_root} with a ${DOCKER_STORE_SIZE} ext4 image so overlay2 is usable"
  if truncate -s "$DOCKER_STORE_SIZE" "$DOCKER_STORE_IMAGE" \
    && mkfs.ext4 -q -F "$DOCKER_STORE_IMAGE" \
    && mount -o loop "$DOCKER_STORE_IMAGE" "$data_root"; then
    log "mounted $(stat -f -c %T "$data_root") at ${data_root}"
  else
    log "could not mount the ext4 image; docker will run on vfs"
    rm -f "$DOCKER_STORE_IMAGE"
  fi

  printf '%s' "$data_root"
}

start_docker_daemon() {
  # The client-side tooling is installed even when a daemon is already up:
  # compose and buildx are CLI plugins, and an image that provides a socket
  # does not necessarily provide those. Skipping this is how the compatibility
  # slice ends up failing on a missing buildx three steps later.
  ensure_runtime_prereqs
  install_docker_binaries

  if docker info >/dev/null 2>&1; then
    log "a docker daemon is already reachable; using it"
    export DOCKER_BUILDKIT=1
    return 0
  fi

  local data_root
  data_root="$(prepare_data_root)"
  mkdir -p /var/log

  log "starting dockerd (data-root=${data_root})"
  dockerd \
    --host=unix:///var/run/docker.sock \
    --data-root="$data_root" \
    --iptables=true \
    >/var/log/dockerd.log 2>&1 &

  local waited=0
  until docker info >/dev/null 2>&1; do
    waited=$((waited + 1))
    if [ "$waited" -gt 60 ]; then
      log "dockerd did not come up within 60s; last 50 log lines:"
      tail -50 /var/log/dockerd.log >&2 || true
      fail "dockerd failed to start"
    fi
    sleep 1
  done

  log "dockerd ready after ${waited}s (storage driver: $(docker info -f '{{.Driver}}'))"

  # Make BuildKit the default for `docker build` too, so a slice does not have
  # to remember to ask for it.
  export DOCKER_BUILDKIT=1
}

# `docker exec`, `docker logs` and the compose build all assume the CLI can
# reach the daemon; make that failure loud and early rather than 30 steps in.
require_docker() {
  docker info >/dev/null 2>&1 || fail "no docker daemon reachable at unix:///var/run/docker.sock"
}

if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  start_docker_daemon
fi
