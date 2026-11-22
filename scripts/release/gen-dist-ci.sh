#!/usr/bin/env bash
# Regenerates .github/workflows/release.yml from dist-workspace.toml with cargo-dist 0.34.0.
#
# dist generates in a scratch git repo (so it never rewrites other files in this checkout), then
# this script applies a fixed set of patches. Every patch asserts its exact match count, so a dist
# upgrade that changes the template fails loudly instead of silently dropping a fix.
# CI (.github/workflows/release-lint.yml) runs `--check`, so hand edits to release.yml fail.
#
# Prerelease tags (e.g. v7.1.0-rc.1) are a dry run BY DESIGN: dist skips homebrew/npm/image for
# prereleases, so `announce` (which requires every channel to succeed) is skipped and the GitHub
# Release stays a DRAFT, with archives, SBOMs and signatures attached for inspection.
# Delete it afterwards: gh release delete <tag> --cleanup-tag (owner/lead only).
#
# Usage: scripts/release/gen-dist-ci.sh [--check]
#   --check  regenerate into a temp file and diff against the committed workflow (exit 1 on drift)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/.github/workflows/release.yml"
want_dist="0.34.0"

have="$(dist --version 2>/dev/null | awk '{print $2}')" || true
[ "$have" = "$want_dist" ] || { echo "need dist $want_dist (have '${have:-none}'): cargo install cargo-dist --version $want_dist --locked" >&2; exit 1; }

scratch="$(mktemp -d)"
trap '[ -n "${KEEP:-}" ] && echo "$scratch" || rm -rf "$scratch"' EXIT
rsync -a --exclude target --exclude node_modules --exclude .git --exclude spikes "$root/" "$scratch/"
# Custom-job workflows owned by other lanes may not exist yet in this checkout; dist only needs
# the file names, so give it inert stand-ins inside the scratch copy.
for job in release-preflight release-supply-chain publish-npm publish-image; do
  f="$scratch/.github/workflows/$job.yml"
  [ -f "$f" ] || printf 'name: %s\non:\n  workflow_call:\njobs: {}\n' "$job" > "$f"
done
sed -i.bak '/^allow-dirty/d' "$scratch/dist-workspace.toml" && rm "$scratch/dist-workspace.toml.bak"
( cd "$scratch" && git init -q && dist generate --mode ci >/dev/null )
gen="$scratch/.github/workflows/release.yml"

# Each substitution must match exactly the expected number of times, or generation fails.
GEN="$gen" perl -0 <<'PERL'
use strict; use warnings;
my $f = $ENV{GEN};
open my $in, '<', $f or die $!; local $/; my $y = <$in>; close $in;
sub p { my ($desc, $n, $code) = @_; my $got = $code->(); $got == $n or die "gen-dist-ci: patch '$desc' matched $got times, expected $n\n"; }

# 1. dist 0.34.0 bug (templates/ci/github/release.yml.j2): custom jobs are emitted as
#    `uses: $/.github/workflows/<job>.yml`, which GitHub rejects; reusable workflows need `./`.
p('custom-job $/ path', 4, sub { $y =~ s{uses: \$/\.github/}{uses: ./.github/}g });
# 2. Trigger: `v*` version tags only (dist's default also matches `<pkg>/x.y.z` tags).
p('v* tag trigger', 1, sub { $y =~ s{      - '\*\*\[0-9\]\+\.\[0-9\]\+\.\[0-9\]\+\*'}{      - 'v[0-9]+.[0-9]+.[0-9]+*'} });
# 3. Least privilege: read-only at the top; contents:write only where dist talks to the release API.
p('top-level permissions', 1, sub { $y =~ s{^permissions:\n  "contents": "write"\n}{permissions:\n  "contents": "read"\n}m });
p('host permissions', 1, sub { $y =~ s{^(  host:\n)}{$1    permissions:\n      "contents": "write"\n}m });
p('announce permissions', 1, sub { $y =~ s{^(  announce:\n)}{$1    permissions:\n      "contents": "write"\n}m });
# 4. R-1: nothing is hosted unless the preflight passed (dist's host `if` treats skipped builds as ok).
p('host needs preflight', 1, sub { $y =~ s{^(  host:\n(?:    .*\n)*?    needs:\n      - plan\n)}{$1      - custom-release-preflight\n}m });
p('host if preflight', 1, sub { $y =~ s{(\n  host:\n(?:    .*\n)*?    if: \$\{\{ always\(\) && needs\.plan\.result == 'success' && )}{$1needs.custom-release-preflight.result == 'success' && }s });
# 5. Supply chain attaches to the draft release, so it runs after host (and before every publish job).
p('supply chain needs host', 1, sub { $y =~ s{^(  custom-release-supply-chain:\n    needs:\n      - plan\n)}{$1      - host\n}m });
# 6. Homebrew publishing holds HOMEBREW_TAP_TOKEN: run it in the `release` environment.
p('homebrew environment', 1, sub { $y =~ s{^(  publish-homebrew-formula:\n)}{$1    environment: release\n}m });
# 7. The release is created as a draft + prerelease.
p('draft+prerelease create', 1, sub { $y =~ s{gh release create "\$\{\{ needs\.plan\.outputs\.tag \}\}" --target "\$RELEASE_COMMIT" \$PRERELEASE_FLAG }{gh release create "\${{ needs.plan.outputs.tag }}" --draft --prerelease --target "\$RELEASE_COMMIT" } });
# 8. announce undrafts only after supply chain and EVERY publish job succeeded (no "skipped is ok");
#    the release stays a prerelease until promote.yml (owner-approved promotion to latest).
p('announce strict if', 1, sub { $y =~ s{(\n  announce:\n(?:    .*\n|      .*\n)*?)    if: [^\n]*\n}{$1    if: \${{ always() && needs.host.result == 'success' && needs.custom-release-supply-chain.result == 'success' && needs.custom-publish-npm.result == 'success' && needs.custom-publish-image.result == 'success' }}\n}s });
p('announce undraft step', 1, sub { $y =~ s{(\n  announce:\n.*?    steps:\n)      - uses: actions/checkout@[0-9a-f]{40}\n        with:\n          persist-credentials: false\n          submodules: recursive\n}{$1      - name: Publish the draft release (stays a prerelease)\n        env:\n          GH_REPO: \${{ github.repository }}\n          TAG: \${{ needs.plan.outputs.tag }}\n        run: gh release edit "\$TAG" --draft=false --prerelease\n}s });

# 9. Pinned tool installs (review #2). plan: the dist 0.34.0 linux tarball checked against a pinned
#    sha256 (host/global reuse that binary via this run's cargo-dist-cache artifact). Build runners
#    (mac/linux/windows): `cargo install --locked --version` for dist and cargo-auditable.
my $dist_sha = '7fee44ff3ff71ed0064b32d4ec62fee6b58bd37c5887b12a86aedddc5af0c7b5';
my $plan_install = q{      - name: Install dist 0.34.0 (sha256-pinned)
        shell: bash
        run: |
          set -euo pipefail
          f=cargo-dist-x86_64-unknown-linux-gnu.tar.xz
          curl --proto '=https' --tlsv1.2 -fLsS -o "$RUNNER_TEMP/$f" "https://github.com/axodotdev/cargo-dist/releases/download/v0.34.0/$f"
          echo "} . $dist_sha . q{  $RUNNER_TEMP/$f" | sha256sum -c -
          mkdir -p ~/.cargo/bin
          tar -xJf "$RUNNER_TEMP/$f" -C ~/.cargo/bin --strip-components=1 cargo-dist-x86_64-unknown-linux-gnu/dist
          ~/.cargo/bin/dist --version
};
p('plan dist install', 1, sub { $y =~ s{      - name: Install dist\n(?:        #.*\n)*        shell: bash\n        run: "curl [^\n]*cargo-dist-installer\.sh \| sh"\n}{$plan_install} });
p('matrix dist install', 1, sub { $y =~ s{run: \$\{\{ matrix\.install_dist\.run \}\}}{run: cargo install cargo-dist --version 0.34.0 --locked} });
p('matrix cargo-auditable install', 1, sub { $y =~ s{run: \$\{\{ matrix\.install_cargo_auditable\.run \}\}}{run: cargo install cargo-auditable --version 0.7.7 --locked} });
# 10. Tag name reaches the plan shell through env only, validated first (review #14).
my $plan_step = q{      - id: plan
        env:
          TAG: ${{ github.ref_name }}
        run: |
          set -euo pipefail
          [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "::error::bad tag"; exit 1; }
          dist host --steps=create --tag="$TAG" --output-format=json > plan-dist-manifest.json
};
p('plan step env tag', 1, sub { $y =~ s{      - id: plan\n        run: \|\n          dist \$\{\{ \(!github\.event\.pull_request && format\('host --steps=create --tag=\{0\}', github\.ref_name\)\) \|\| 'plan' \}\} --output-format=json > plan-dist-manifest\.json\n}{$plan_step} });
# 11. Homebrew (review #3, #6): runs after `announce` published the release (formula URLs resolve
#     anonymously), in environment `release`; no live Homebrew code; the tap token is not persisted
#     in .git/config and is visible only to the push step.
p('homebrew needs announce', 1, sub { $y =~ s{(\n  publish-homebrew-formula:\n    environment: release\n    needs:\n      - plan\n)      - host\n      - custom-release-supply-chain\n}{$1      - announce\n} });
p('announce drops homebrew need', 1, sub { $y =~ s{(\n  announce:\n(?:    .*\n)*?    needs:\n(?:      - .*\n)*?)      - publish-homebrew-formula\n}{$1} });
p('homebrew checkout no token', 1, sub { $y =~ s{          persist-credentials: true\n          repository: "Happy-Technologies-LLC/homebrew-tap"\n          token: \$\{\{ secrets\.HOMEBREW_TAP_TOKEN \}\}\n}{          persist-credentials: false\n          repository: "Happy-Technologies-LLC/homebrew-tap"\n} });
p('homebrew no brew', 1, sub { $y =~ s{\n            export PATH="/home/linuxbrew/\.linuxbrew/bin:\$PATH"\n            brew update\n(?:            #.*\n)?            brew style [^\n]*\n}{\n} });
my $push = q{          done
      - name: Push formula to the tap
        env:
          TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}
        run: git push "https://x-access-token:${TAP_TOKEN}@github.com/Happy-Technologies-LLC/homebrew-tap.git" HEAD
};
p('homebrew push step', 1, sub { $y =~ s{          done\n          git push\n}{$push} });
# 12. No `secrets: inherit` (review #7): preflight and supply chain get none; each publish workflow
#     gets only the secrets it declares.
p('preflight no secrets', 1, sub { $y =~ s{(uses: \./\.github/workflows/release-preflight\.yml\n)    secrets: inherit\n}{$1} });
p('supply chain no secrets', 1, sub { $y =~ s{(uses: \./\.github/workflows/release-supply-chain\.yml\n    with:\n      plan: .*\n)    secrets: inherit\n}{$1} });
my $npm_secrets = q{    secrets:
      NPM_TOKEN: ${{ secrets.NPM_TOKEN }}
};
my $img_secrets = q{    secrets:
      DOCKERHUB_USERNAME: ${{ secrets.DOCKERHUB_USERNAME }}
      DOCKERHUB_TOKEN: ${{ secrets.DOCKERHUB_TOKEN }}
};
p('npm secrets', 1, sub { $y =~ s{(uses: \./\.github/workflows/publish-npm\.yml\n    with:\n      plan: .*\n)    secrets: inherit\n}{$1$npm_secrets} });
p('image secrets', 1, sub { $y =~ s{(uses: \./\.github/workflows/publish-image\.yml\n    with:\n      plan: .*\n)    secrets: inherit\n}{$1$img_secrets} });

# 13. Windows Authenticode (Azure Artifact Signing). `dist build` compiles and zips in one step, so
#     the Windows runner first builds the same binaries with dist's exact cargo invocation (dist sets
#     RUSTFLAGS=-Ctarget-feature=+crt-static for msvc; any other value makes dist recompile and drop the
#     signatures, verified on a probe run), signs them in place, and `dist build` then reuses them and
#     zips the signed files. The final step fails the build unless every exe in the zip is validly signed.
#     OIDC login uses the `release-signing` environment (no reviewers, v* tags only), which is also
#     the Entra federated-credential subject; it sits on the whole matrix because a job-level
#     environment cannot vary per matrix entry. Account `happytech-signing`, profile `happytech-public`.
p('build environment + oidc', 1, sub { $y =~ s{^(  build-local-artifacts:\n    name: [^\n]*\n)}{$1    environment: release-signing\n    permissions:\n      "contents": "read"\n      "id-token": "write"\n}m });
my $win_sign = q{      - name: Prebuild Windows binaries for signing
        if: ${{ runner.os == 'Windows' }}
        shell: bash
        env:
          RUSTFLAGS: "-Ctarget-feature=+crt-static"
        run: cargo auditable build --locked --profile dist --workspace --target x86_64-pc-windows-msvc
      - name: Azure login (OIDC)
        if: ${{ runner.os == 'Windows' }}
        uses: azure/login@a641126d1b8aa4d1fa005f4f92df94a3a4c4c906 # v3.1.0
        with:
          client-id: ${{ secrets.AZURE_CLIENT_ID }}
          tenant-id: ${{ secrets.AZURE_TENANT_ID }}
          subscription-id: ${{ secrets.AZURE_SUBSCRIPTION_ID }}
      - name: Authenticode-sign Windows binaries
        if: ${{ runner.os == 'Windows' }}
        uses: azure/artifact-signing-action@c7ab2a863ab5f9a846ddb8265964877ef296ee82 # v2.0.0
        with:
          endpoint: https://eus.codesigning.azure.net/
          signing-account-name: happytech-signing
          certificate-profile-name: happytech-public
          files: |
            ${{ github.workspace }}\target\x86_64-pc-windows-msvc\dist\happy-platform-mcp.exe
            ${{ github.workspace }}\target\x86_64-pc-windows-msvc\dist\servicenow-mcp-server.exe
            ${{ github.workspace }}\target\x86_64-pc-windows-msvc\dist\happy-platform-mcp-http.exe
          file-digest: SHA256
          timestamp-rfc3161: http://timestamp.acs.microsoft.com
          timestamp-digest: SHA256
          exclude-environment-credential: true
          exclude-workload-identity-credential: true
          exclude-managed-identity-credential: true
          exclude-shared-token-cache-credential: true
          exclude-visual-studio-credential: true
          exclude-visual-studio-code-credential: true
          exclude-azure-cli-credential: false
          exclude-azure-powershell-credential: true
          exclude-azure-developer-cli-credential: true
          exclude-interactive-browser-credential: true
      - name: Build artifacts
};
p('windows sign steps', 1, sub { $y =~ s{      - name: Build artifacts\n}{$win_sign} });
my $win_verify = q{          echo "dist ran successfully"
      - name: Verify Authenticode signatures in the Windows zip
        if: ${{ runner.os == 'Windows' }}
        shell: pwsh
        run: |
          $ErrorActionPreference = 'Stop'
          Expand-Archive target/distrib/happy-platform-mcp-x86_64-pc-windows-msvc.zip -DestinationPath signcheck
          $exes = Get-ChildItem signcheck -Recurse -Filter *.exe
          if ($exes.Count -lt 3) { throw "expected 3 executables, found $($exes.Count)" }
          foreach ($exe in $exes) {
            $sig = Get-AuthenticodeSignature $exe.FullName
            Write-Host "$($exe.Name): $($sig.Status) $($sig.SignerCertificate.Subject)"
            if ($sig.Status -ne 'Valid') { throw "$($exe.Name) Authenticode status is $($sig.Status)" }
          }
};
p('windows verify step', 1, sub { $y =~ s{          echo "dist ran successfully"\n}{$win_verify} });

open my $out, '>', $f or die $!; print $out $y; close $out;
PERL

if grep -q 'uses: \$/' "$gen"; then echo "gen-dist-ci: unpatched \$/ reference left" >&2; exit 1; fi

if [ "${1:-}" = "--check" ]; then
  diff -u "$out" "$gen" && echo "release.yml is up to date"
else
  cp "$gen" "$out"
  echo "wrote ${out#"$root"/}"
fi
