#!/usr/bin/env bash
set -euo pipefail

# Release script for happy-platform-mcp
# Publishes to: GitHub (tag + release), npm (new + deprecation notice on old), Docker Hub
# Usage:
#   ./scripts/release.sh          # publish current version from package.json
#   ./scripts/release.sh 3.1.0    # bump to 3.1.0, commit, then publish

NPM_PACKAGE="happy-platform-mcp"
NPM_OLD_PACKAGE="servicenow-mcp-server"
DOCKER_USER="nczitzer"
DOCKER_IMAGE="happy-platform-mcp"
DOCKER_OLD_IMAGE="mcp-servicenow-nodejs"
REPO="Happy-Technologies-LLC/happy-platform-mcp"

cd "$(git rev-parse --show-toplevel)"

# --- Version resolution ---
if [ -n "${1:-}" ]; then
  echo "==> Bumping version to $1"
  npm version "$1" --no-git-tag-version
  git add package.json package-lock.json
  git commit -m "Release v$1"
  git push origin main
fi

VERSION=$(node -p "require('./package.json').version")
TAG="v${VERSION}"

echo ""
echo "=========================================="
echo "  Releasing ${NPM_PACKAGE} ${TAG}"
echo "=========================================="
echo ""

# --- Preflight checks ---
echo "==> Preflight checks"

if ! npm whoami &>/dev/null; then
  echo "ERROR: Not logged in to npm. Run: npm login"
  exit 1
fi

if ! docker info &>/dev/null; then
  echo "ERROR: Docker is not running."
  exit 1
fi

# Check for uncommitted changes
if [ -n "$(git status --porcelain)" ]; then
  echo "ERROR: Uncommitted changes. Commit or stash first."
  exit 1
fi

# Check we're on main
BRANCH=$(git branch --show-current)
if [ "$BRANCH" != "main" ]; then
  echo "WARNING: On branch '$BRANCH', not 'main'. Continue? (y/N)"
  read -r ans
  [ "$ans" = "y" ] || exit 1
fi

# --- 1. GitHub: Tag + Release ---
echo ""
echo "==> [1/5] GitHub tag & release"
if git rev-parse "$TAG" &>/dev/null; then
  echo "  Tag $TAG already exists, skipping tag creation"
else
  git tag -a "$TAG" -m "Release $TAG"
  git push origin "$TAG"
  echo "  Pushed tag $TAG"
fi

if gh release view "$TAG" &>/dev/null; then
  echo "  GitHub release $TAG already exists, skipping"
else
  gh release create "$TAG" \
    --title "Release $TAG" \
    --notes "See [CHANGELOG.md](https://github.com/${REPO}/blob/main/CHANGELOG.md) for details.

**Get release notes and security advisories by email:** https://happy-tech.biz/mcp
Running Happy MCP at your company? [Add yourself to the users list](https://github.com/${REPO}/issues/new?template=who-is-using.yml)." \
    --latest
  echo "  Created GitHub release $TAG"
fi

# --- 2. npm publish (new package) ---
echo ""
echo "==> [2/5] npm publish (${NPM_PACKAGE})"
PUBLISHED=$(npm view "${NPM_PACKAGE}" version 2>/dev/null || echo "none")
if [ "$PUBLISHED" = "$VERSION" ]; then
  echo "  v${VERSION} already on npm, skipping"
else
  npm publish --access public
  echo "  Published ${NPM_PACKAGE}@${VERSION} to npm"
fi

# --- 3. Deprecate old npm package ---
echo ""
echo "==> [3/5] Deprecate old npm package (${NPM_OLD_PACKAGE})"
npm deprecate "${NPM_OLD_PACKAGE}" "This package has been renamed to ${NPM_PACKAGE}. Please run: npm install ${NPM_PACKAGE}" 2>/dev/null || echo "  (already deprecated or not owned)"

# --- 4. Docker Hub: login check ---
echo ""
echo "==> [4/5] Docker Hub login"
if ! docker login --username "$DOCKER_USER" 2>/dev/null; then
  echo "  Need Docker Hub credentials."
  docker login --username "$DOCKER_USER"
fi

# --- 5. Docker build + push (new image name) ---
echo ""
echo "==> [5/5] Docker build & push"
FULL_IMAGE="${DOCKER_USER}/${DOCKER_IMAGE}"
FULL_OLD_IMAGE="${DOCKER_USER}/${DOCKER_OLD_IMAGE}"

# Extract major.minor for semver tags
MAJOR=$(echo "$VERSION" | cut -d. -f1)
MINOR=$(echo "$VERSION" | cut -d. -f1-2)

echo "  Building image..."
docker build \
  -t "${FULL_IMAGE}:${VERSION}" \
  -t "${FULL_IMAGE}:${MINOR}" \
  -t "${FULL_IMAGE}:${MAJOR}" \
  -t "${FULL_IMAGE}:latest" \
  -t "${FULL_OLD_IMAGE}:${VERSION}" \
  -t "${FULL_OLD_IMAGE}:latest" \
  .

echo "  Pushing new image tags..."
docker push "${FULL_IMAGE}:${VERSION}"
docker push "${FULL_IMAGE}:${MINOR}"
docker push "${FULL_IMAGE}:${MAJOR}"
docker push "${FULL_IMAGE}:latest"

echo "  Pushing final tags to old image (for migration)..."
docker push "${FULL_OLD_IMAGE}:${VERSION}"
docker push "${FULL_OLD_IMAGE}:latest"

echo "  Pushed ${FULL_IMAGE}:{${VERSION},${MINOR},${MAJOR},latest}"
echo "  Pushed ${FULL_OLD_IMAGE}:{${VERSION},latest} (migration)"

# --- Done ---
echo ""
echo "=========================================="
echo "  Release ${TAG} complete!"
echo "=========================================="
echo ""
echo "  GitHub:  https://github.com/${REPO}/releases/tag/${TAG}"
echo "  npm:     https://www.npmjs.com/package/${NPM_PACKAGE}/v/${VERSION}"
echo "  Docker:  https://hub.docker.com/r/${DOCKER_USER}/${DOCKER_IMAGE}/tags"
echo ""
echo "  Migration notes:"
echo "    npm:    Users should run 'npm install ${NPM_PACKAGE}' (old package deprecated)"
echo "    Docker: Users should pull '${FULL_IMAGE}' (old image gets final tag only)"
echo ""
