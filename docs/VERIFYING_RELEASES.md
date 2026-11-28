# Verifying 7.x releases

Every 7.x release is built once, and the same bytes go to every channel: the GitHub Release archives, the Homebrew formula, the npm platform packages and the Docker image. This page shows how to check that what you downloaded is what the project's release workflow built.

Replace `v7.2.3` and the archive name with the release and platform you downloaded. All commands below need the [GitHub CLI](https://cli.github.com/) (`gh`) 2.49 or newer and [cosign](https://docs.sigstore.dev/cosign/installation/) 2 or newer.

## What a release contains

| File | What it is |
|---|---|
| `happy-platform-mcp-<target>.tar.xz` (`.zip` on Windows) | The three binaries (`happy-platform-mcp`, `servicenow-mcp-server`, `happy-platform-mcp-http`) for one platform |
| `happy-platform-mcp-<target>.<tar.xz or zip>.sha256` | SHA-256 of that archive, produced by the build |
| `SHA256SUMS` | SHA-256 of every archive and of the SBOM |
| `SHA256SUMS.sigstore.json` | Keyless Sigstore signature (bundle) over `SHA256SUMS` |
| `happy-platform-mcp.cdx.json` | CycloneDX software bill of materials |

Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.

The archives also carry GitHub build-provenance attestations, and the SBOM is attested against them.

## 1. Check the archive against its checksum

```bash
sha256sum -c happy-platform-mcp-x86_64-unknown-linux-gnu.tar.xz.sha256
# macOS: shasum -a 256 -c happy-platform-mcp-aarch64-apple-darwin.tar.xz.sha256
```

A checksum alone only proves the download is intact. Use steps 2 and 3 to prove who built it.

## 2. Verify the build provenance, pinned to the release workflow

```bash
gh attestation verify happy-platform-mcp-x86_64-unknown-linux-gnu.tar.xz \
  --repo Happy-Technologies-LLC/happy-platform-mcp \
  --signer-workflow Happy-Technologies-LLC/happy-platform-mcp/.github/workflows/release-supply-chain.yml \
  --source-ref refs/tags/v7.2.3 --deny-self-hosted-runners
```

Pin the signer, as above. A bare `gh attestation verify --repo ...` accepts an attestation made by any workflow in the repository, which is not enough. `--source-ref` pins the tag the attestation was made for, and `--deny-self-hosted-runners` refuses a build that ran on a self-hosted runner.

`release-supply-chain.yml` is the only workflow that attests release archives; the build jobs do not. To run every check in this guide at once, use [`scripts/release/verify-release.sh`](../scripts/release/verify-release.sh) `<version> <download-dir>`.

## 3. Verify the signed checksum file

```bash
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity 'https://github.com/Happy-Technologies-LLC/happy-platform-mcp/.github/workflows/release-supply-chain.yml@refs/tags/v7.2.3'
sha256sum -c --ignore-missing SHA256SUMS
```

The certificate identity names the exact workflow file and tag that signed `SHA256SUMS`. A signature from any other workflow or tag does not verify. The second command then checks every file you downloaded against the signed list.

## 4. Check the SBOM attestation

```bash
gh attestation verify happy-platform-mcp-x86_64-unknown-linux-gnu.tar.xz \
  --repo Happy-Technologies-LLC/happy-platform-mcp \
  --signer-workflow Happy-Technologies-LLC/happy-platform-mcp/.github/workflows/release-supply-chain.yml \
  --source-ref refs/tags/v7.2.3 --deny-self-hosted-runners \
  --predicate-type https://cyclonedx.org/bom
```

## 5. Docker image

Pull and run the image by digest; the digest is in the GitHub Release notes. A tag such as `next` can move; a digest cannot.

```bash
docker pull nczitzer/happy-platform-mcp@sha256:<digest>
cosign verify nczitzer/happy-platform-mcp@sha256:<digest> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity 'https://github.com/Happy-Technologies-LLC/happy-platform-mcp/.github/workflows/publish-image.yml@refs/tags/v7.2.3'
gh attestation verify oci://nczitzer/happy-platform-mcp@sha256:<digest> \
  --repo Happy-Technologies-LLC/happy-platform-mcp \
  --signer-workflow Happy-Technologies-LLC/happy-platform-mcp/.github/workflows/publish-image.yml \
  --source-ref refs/tags/v7.2.3 --deny-self-hosted-runners
```

## 6. npm

The platform packages and the wrapper are published with npm provenance. After an install, check the registry signatures and provenance of the tree:

```bash
npm audit signatures
```

## What is not signed yet

The preview (`next`) binaries are not code-signed: there is no Apple Developer ID signature or notarisation and no Windows Authenticode signature yet. Both are added before 7.0 is promoted to `latest`. Until then macOS and Windows may warn about the binary (see [MIGRATING_TO_7.md](MIGRATING_TO_7.md)); the checks above are how you establish where it came from.

## Related

- [Migrating to 7.0](MIGRATING_TO_7.md)
- [README: Install 7.0 (preview)](../README.md#install-70-preview)
