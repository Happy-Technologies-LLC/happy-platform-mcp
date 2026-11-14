#!/usr/bin/env bash
# Logs ./mcp-publisher in to the MCP registry for the server named in ./server.json, then
# fails unless the issued registry token grants `publish` on that name. Used by promote.yml.
#
# - Name under io.github.<repository_owner>/  -> `login github-oidc` (needs id-token: write).
# - Any other name (e.g. a personal io.github.<user>/ namespace) -> `login github --token`
#   with MCP_REGISTRY_GITHUB_TOKEN, a token of that namespace's owner held in environment
#   `release`. Missing token: fail closed.
set -euo pipefail

name=$(jq -r .name server.json)
owner="${GITHUB_REPOSITORY_OWNER:?}"
if [[ "$name" == "io.github.$owner/"* ]]; then
  echo "registry login: github-oidc ($name)"
  ./mcp-publisher login github-oidc
else
  [ -n "${MCP_REGISTRY_GITHUB_TOKEN:-}" ] || {
    echo "::error::$name is outside io.github.$owner/; set secret MCP_REGISTRY_GITHUB_TOKEN in environment release"
    exit 1
  }
  echo "registry login: github token ($name)"
  ./mcp-publisher login github --token "$MCP_REGISTRY_GITHUB_TOKEN"
fi

# The registry JWT lists permissions as {action, resource} with resource "*", an exact
# name or a "prefix/*" pattern.
token_file="$HOME/.config/mcp-publisher/token.json"
[ -f "$token_file" ] || token_file="$HOME/.mcp_publisher_token"
payload=$(jq -r .token "$token_file" | cut -d. -f2 | tr '_-' '/+')
while [ $(( ${#payload} % 4 )) -ne 0 ]; do payload="$payload="; done
if ! base64 -d <<<"$payload" | jq -e --arg n "$name" '
    [.permissions[]? | select(.action == "publish") | .resource as $r
     | select($r == "*" or $r == $n or (($r | endswith("/*")) and ($n | startswith($r[:-1]))))] | length > 0' >/dev/null; then
  echo "::error::registry token does not grant publish on $name"
  exit 1
fi
echo "registry token grants publish on $name"
