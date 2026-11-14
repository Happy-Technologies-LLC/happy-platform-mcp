#!/usr/bin/env bash
# Verifies a downloaded happy-platform-mcp 7.x release against its pinned signer (design §2.9 R-3).
#
# Usage: scripts/release/verify-release.sh <version> <download-dir>
#   e.g. scripts/release/verify-release.sh 7.0.0 ~/Downloads/hpm-7.0.0
#   Download first:  gh release download v7.0.0 -R Happy-Technologies-LLC/happy-platform-mcp -D <dir>
#
# Signers (which workflow signs what):
#   release-supply-chain.yml (reusable workflow called by release.yml, job `sign`) signs
#     - SLSA build provenance for every archive, installer and SHA256SUMS (actions/attest-build-provenance)
#     - CycloneDX SBOM attestations (one per binary) on every archive (actions/attest-sbom)
#     - the keyless cosign bundle SHA256SUMS.sigstore.json over SHA256SUMS
#   Fulcio puts the CALLED workflow's job_workflow_ref in the certificate SAN, so the identity is
#   release-supply-chain.yml@refs/tags/v<version>, not release.yml. [UNVERIFIED until the first tag run]
#   A bare `gh attestation verify --repo` would accept an attestation from ANY workflow in the repo;
#   this script always pins --signer-workflow / --certificate-identity.
#
# Requires: gh (authenticated), cosign >= 2, sha256sum or shasum.
set -euo pipefail

[ $# -eq 2 ] || { echo "usage: $0 <version> <download-dir>" >&2; exit 2; }
version="${1#v}"
dir="$2"
repo="Happy-Technologies-LLC/happy-platform-mcp"
tag="v$version"
signer_workflow="$repo/.github/workflows/release-supply-chain.yml"
identity="https://github.com/$signer_workflow@refs/tags/$tag"
issuer="https://token.actions.githubusercontent.com"
targets=(aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-msvc)

cd "$dir"
for f in SHA256SUMS SHA256SUMS.sigstore.json; do
  [ -f "$f" ] || { echo "missing $dir/$f" >&2; exit 1; }
done

echo "== cosign: SHA256SUMS signed by $identity"
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-oidc-issuer "$issuer" \
  --certificate-identity "$identity"

echo "== checksums of present files"
if command -v sha256sum >/dev/null; then sum=(sha256sum); else sum=(shasum -a 256); fi
"${sum[@]}" --ignore-missing -c SHA256SUMS

verify_attestation() { # file [predicate-type]
  local args=(--repo "$repo" --signer-workflow "$signer_workflow" --source-ref "refs/tags/$tag" --deny-self-hosted-runners)
  [ -z "${2:-}" ] || args+=(--predicate-type "$2")
  gh attestation verify "$1" "${args[@]}" >/dev/null
  echo "ok  $1${2:+ ($2)}"
}

echo "== attestations (signer $signer_workflow, ref refs/tags/$tag)"
found=0
for t in "${targets[@]}"; do
  ext=tar.xz; [ "$t" = x86_64-pc-windows-msvc ] && ext=zip
  a="happy-platform-mcp-$t.$ext"
  [ -f "$a" ] || continue
  found=$((found + 1))
  verify_attestation "$a" https://slsa.dev/provenance/v1
  verify_attestation "$a" https://cyclonedx.org/bom
done
for f in happy-platform-mcp-installer.sh happy-platform-mcp-installer.ps1 SHA256SUMS; do
  [ -f "$f" ] && verify_attestation "$f" https://slsa.dev/provenance/v1
done
[ "$found" -gt 0 ] || { echo "no release archives found in $dir" >&2; exit 1; }
echo "verified $found archive(s) of $tag"
