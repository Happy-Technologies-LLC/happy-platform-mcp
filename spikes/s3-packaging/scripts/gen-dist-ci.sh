#!/usr/bin/env bash
# Regenerates dist-ci/release.yml. dist derives the workflow path from the *git* root, and this
# spike lives inside the product repo, so we generate in a scratch git repo and copy the result
# next to (not into) the repo's real .github/. Nothing here enables any workflow.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
rsync -a --exclude target --exclude node_modules --exclude dist-ci --exclude .tmp "$here/" "$scratch/"
mkdir -p "$scratch/.github/workflows"
cp "$here/dist-ci/package-npm.yml" "$scratch/.github/workflows/package-npm.yml"
sed -i.bak '/^allow-dirty/d' "$scratch/dist-workspace.toml" && rm "$scratch/dist-workspace.toml.bak"
( cd "$scratch" && git init -q && dist generate --mode ci )
# cargo-dist 0.34.0 templates/ci/github/release.yml.j2 emits `uses: $/.github/workflows/<job>.yml`
# for custom jobs, which is not a valid reusable-workflow reference; GitHub requires `./`.
sed 's|uses: \$/\.github/|uses: ./.github/|' "$scratch/.github/workflows/release.yml" > "$here/dist-ci/release.yml"
echo "wrote $here/dist-ci/release.yml"
