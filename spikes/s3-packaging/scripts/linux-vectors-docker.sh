#!/usr/bin/env bash
# Local Linux run of the keychain vectors (dbus-secret-service + headless gnome-keyring) in Docker.
# Mirrors the CI job in dist-ci/ci.yml. Repo is mounted read-only; work happens on a copy.
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
docker run --rm -v "$root":/src:ro node:22-bookworm bash -lc '
  set -euo pipefail
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gnome-keyring libdbus-1-dev pkg-config dbus build-essential curl ca-certificates >/dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null 2>&1
  . "$HOME/.cargo/env"
  mkdir -p /w/spikes && cp -r /src/src /src/package.json /src/package-lock.json /w/
  rsync -a --exclude target --exclude node_modules --exclude .tmp /src/spikes/s3-packaging /w/spikes/ 2>/dev/null || { mkdir -p /w/spikes/s3-packaging; tar -C /src/spikes/s3-packaging --exclude=target --exclude=node_modules --exclude=.tmp -cf - . | tar -C /w/spikes/s3-packaging -xf -; }
  cd /w && npm ci --omit=dev --no-audit --no-fund >/dev/null
  cd /w/spikes/s3-packaging
  cargo test --test identity_vectors 2>&1 | tail -4
  cargo test --test keychain_vectors --no-run 2>&1 | tail -2
  dbus-run-session -- bash -c "eval \$(echo -n ci-test-pw | gnome-keyring-daemon --unlock --components=secrets); node scripts/run-keychain-vectors.mjs"
'
