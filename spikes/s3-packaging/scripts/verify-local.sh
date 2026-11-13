#!/usr/bin/env bash
# Host-only end-to-end check: cargo-dist build -> npm pack -> install into a temp prefix ->
# both bin shims launch the native binary (argv, stdin, exit code) -> npx -y path.
# No registry access needed (registry is pointed at a dead port on purpose).
set -euo pipefail
cd "$(dirname "$0")/.."
host="$(rustc -vV | sed -n 's/^host: //p')"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
case "$host" in *windows*) exe=happy-platform-mcp.exe ;; *) exe=happy-platform-mcp ;; esac

echo "== dist build ($host)"
dist build --artifacts=local --target "$host"
bin="target/distrib/happy-platform-mcp-$host/$exe"
[ -f "$bin" ] || bin="target/distrib/happy-platform-mcp-$host.zip" # windows archives are zip
ls -l "target/distrib/happy-platform-mcp-$host".* | sed 's/^/   /'

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
echo "== assemble + npm pack"
node scripts/assemble-npm.mjs --version "$version" --out "$tmp/npm" --bin "$host=$bin"
mkdir "$tmp/packs"
for d in "$tmp"/npm/*/; do (cd "$d" && npm pack --silent --pack-destination "$tmp/packs"); done
ls "$tmp/packs" | sed 's/^/   /'

echo "== install into temp prefix (dead registry: only the tarballs may be used)"
mkdir "$tmp/prefix" && (cd "$tmp/prefix" && npm init -y >/dev/null && \
  npm install --no-audit --no-fund --registry http://127.0.0.1:9/ --fetch-retries=0 "$tmp"/packs/*.tgz | tail -1)
b="$tmp/prefix/node_modules/.bin"
for name in happy-platform-mcp servicenow-mcp-server; do
  echo "== $name --version -> $("$b/$name" --version)"
done
echo "== argv passthrough -> $("$b/happy-platform-mcp" --s3-info "a b" c)"
echo "== stdin passthrough -> $(echo ping | "$b/servicenow-mcp-server" --s3-echo)"
set +e; "$b/happy-platform-mcp" --s3-exit 7; code=$?; set -e
echo "== exit code passthrough -> $code (want 7)"; [ "$code" = 7 ]

echo "== npx -y path (cwd=/, both tarballs via --package, separate cache)"
main_tgz="$(ls "$tmp"/packs/happy-platform-mcp-[0-9]*.tgz)"; plat_tgz="$(ls "$tmp"/packs/happy-platform-mcp-*-*.tgz | grep -v "$main_tgz" | head -1)"
( cd / && npm_config_cache="$tmp/npxcache" npx -y --registry http://127.0.0.1:9/ --fetch-retries=0 \
    --package "$main_tgz" --package "$plat_tgz" servicenow-mcp-server --version )
echo "RESULT: PASS"
