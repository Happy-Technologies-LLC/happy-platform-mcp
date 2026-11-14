#!/usr/bin/env bash
# Regenerates .github/workflows/release.yml from dist-workspace.toml with cargo-dist 0.34.0.
#
# dist generates in a scratch git repo (so it never rewrites other files in this checkout), then
# this script applies a fixed set of patches. Every patch asserts that it matched, so a dist
# upgrade that changes the template fails loudly instead of silently dropping a fix.
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
p('announce strict if', 1, sub { $y =~ s{(\n  announce:\n(?:    .*\n|      .*\n)*?)    if: [^\n]*\n}{$1    if: \${{ always() && needs.host.result == 'success' && needs.custom-release-supply-chain.result == 'success' && needs.publish-homebrew-formula.result == 'success' && needs.custom-publish-npm.result == 'success' && needs.custom-publish-image.result == 'success' }}\n}s });
p('announce undraft step', 1, sub { $y =~ s{(\n  announce:\n.*?    steps:\n)      - uses: actions/checkout@[0-9a-f]{40}\n        with:\n          persist-credentials: false\n          submodules: recursive\n}{$1      - name: Publish the draft release (stays a prerelease)\n        env:\n          GH_REPO: \${{ github.repository }}\n          TAG: \${{ needs.plan.outputs.tag }}\n        run: gh release edit "\$TAG" --draft=false --prerelease\n}s });

open my $out, '>', $f or die $!; print $out $y; close $out;
PERL

if grep -q 'uses: \$/' "$gen"; then echo "gen-dist-ci: unpatched \$/ reference left" >&2; exit 1; fi

if [ "${1:-}" = "--check" ]; then
  diff -u "$out" "$gen" && echo "release.yml is up to date"
else
  cp "$gen" "$out"
  echo "wrote ${out#"$root"/}"
fi
