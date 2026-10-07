#!/usr/bin/env bash
# History-aware secret scanning for CI and release (issue #67).
#
#   scripts/secret-scan.sh install <dir>  install the pinned, checksum-verified gitleaks (Linux x64)
#   scripts/secret-scan.sh history        scan every commit reachable from HEAD (needs full clone)
#   scripts/secret-scan.sh package        npm pack, extract, and scan the exact tarball contents
#
# Findings are always redacted. Detection rules live in .gitleaks.toml; verified,
# already-rotated historical findings are baselined by exact fingerprint in
# .gitleaksignore so that any new occurrence still fails.
set -euo pipefail

GITLEAKS_VERSION="8.30.1"
# sha256 of gitleaks_8.30.1_linux_x64.tar.gz from the upstream release checksums file.
GITLEAKS_LINUX_X64_SHA256="551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config="$repo_root/.gitleaks.toml"
baseline="$repo_root/.gitleaksignore"

usage() {
  echo "usage: $0 install <dir> | history | package" >&2
  exit 2
}

install_gitleaks() {
  local dest="$1"
  if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
    echo "pinned gitleaks install supports Linux x86_64 only; install gitleaks $GITLEAKS_VERSION manually" >&2
    exit 1
  fi
  local archive="gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz"
  local download
  download="$(mktemp -d)"
  curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$download/$archive" \
    "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/${archive}"
  echo "$GITLEAKS_LINUX_X64_SHA256  $download/$archive" | sha256sum --check --strict
  mkdir -p "$dest"
  tar -xzf "$download/$archive" -C "$dest" gitleaks
  rm -rf "$download"
  "$dest/gitleaks" version
}

require_gitleaks() {
  if ! command -v gitleaks >/dev/null 2>&1; then
    echo "gitleaks is not on PATH; run: $0 install <dir>" >&2
    exit 1
  fi
  local version
  version="$(gitleaks version)"
  if [ "$version" != "$GITLEAKS_VERSION" ]; then
    echo "gitleaks $GITLEAKS_VERSION is required (found $version)" >&2
    exit 1
  fi
}

scan_history() {
  require_gitleaks
  if [ "$(git -C "$repo_root" rev-parse --is-shallow-repository)" != "false" ]; then
    echo "history scan requires a full clone (actions/checkout fetch-depth: 0)" >&2
    exit 1
  fi
  gitleaks git --config "$config" --gitleaks-ignore-path "$baseline" \
    --redact --no-banner --exit-code 1 --log-opts="--full-history HEAD" "$repo_root"
}

scan_package() {
  require_gitleaks
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  (cd "$repo_root" && npm pack --json --pack-destination "$work") > "$work/pack.json"
  local filename
  # Paths go through the environment: an argv[1] would trigger the verifier's own main().
  filename="$(VERIFIER="$repo_root/scripts/verify-package.mjs" PACK_JSON="$work/pack.json" node --input-type=module -e '
    import { readFileSync } from "node:fs";
    import { pathToFileURL } from "node:url";
    const { parsePackOutput } = await import(pathToFileURL(process.env.VERIFIER).href);
    process.stdout.write(parsePackOutput(readFileSync(process.env.PACK_JSON, "utf8")).filename);
  ')"
  mkdir "$work/extract"
  tar -xzf "$work/$filename" -C "$work/extract"
  # gitleaks' default global allowlist skips any node_modules directory, but the
  # bundled SDK dependency tree ships inside the tarball, so scan it under a neutral name.
  if [ -d "$work/extract/package/node_modules" ]; then
    mv "$work/extract/package/node_modules" "$work/extract/package/bundled-dependencies"
  fi
  # The packed tree is scanned without the history baseline: published files must be clean.
  (cd "$work" && gitleaks dir --config "$config" --redact --no-banner --exit-code 1 "$work/extract/package")
}

case "${1:-}" in
  install) [ "$#" -eq 2 ] || usage; install_gitleaks "$2" ;;
  history) [ "$#" -eq 1 ] || usage; scan_history ;;
  package) [ "$#" -eq 1 ] || usage; scan_package ;;
  *) usage ;;
esac
