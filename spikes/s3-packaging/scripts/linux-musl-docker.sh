#!/usr/bin/env bash
# Local check that the static musl build runs on Alpine and that the npm shim picks the *-musl
# package there (libc detection). Builds for the container's native arch (aarch64 on Apple silicon).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
docker run --rm -v "$root":/src:ro rust:alpine sh -c '
  set -eu
  apk add --no-cache nodejs npm musl-dev tar >/dev/null
  mkdir /w && tar -C /src --exclude=./target --exclude=./node_modules --exclude=./.tmp -cf - . | tar -C /w -xf -
  cd /w && cargo build --release 2>&1 | tail -1
  triple="$(rustc -vV | sed -n "s/^host: //p")"; echo "host triple: $triple"
  file=./target/release/happy-platform-mcp; ls -l $file; (ldd $file 2>&1 || true) | head -2
  node scripts/assemble-npm.mjs --version 0.0.1-s3spike --out /tmp/npm --bin "$triple=$file"
  mkdir /tmp/packs /tmp/prefix; for d in /tmp/npm/*/; do (cd $d && npm pack --silent --pack-destination /tmp/packs >/dev/null); done
  cd /tmp/prefix && npm init -y >/dev/null && npm install --no-audit --no-fund --registry http://127.0.0.1:9/ --fetch-retries=0 /tmp/packs/*.tgz | tail -1
  ls node_modules/@happy-platform-mcp
  ./node_modules/.bin/happy-platform-mcp --s3-info
  echo ping | ./node_modules/.bin/servicenow-mcp-server --s3-echo
'
