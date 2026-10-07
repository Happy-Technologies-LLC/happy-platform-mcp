/**
 * OAuth endpoint origin policy.
 *
 * Authorization and token endpoints default to the ServiceNow instance origin.
 * An external identity provider origin is accepted only when the local
 * operator lists it in SERVICENOW_OAUTH_TRUSTED_ORIGINS. The allow-list is read
 * from the process environment only — never from the instance registry, which
 * MCP tools can write, or from tool arguments — so a caller cannot approve its
 * own endpoint. The policy is enforced when instance metadata is registered or
 * loaded and again immediately before every token request.
 */

export const TRUSTED_OAUTH_ORIGINS_ENV = 'SERVICENOW_OAUTH_TRUSTED_ORIGINS';

const LOOPBACK_HOSTS = new Set(['127.0.0.1', 'localhost', '[::1]']);

export class OAuthEndpointPolicyError extends Error {
  constructor(code, message, field) {
    super(message);
    this.name = 'OAuthEndpointPolicyError';
    this.code = code;
    if (field) this.field = field;
  }

  toJSON() {
    return { name: this.name, message: this.message, code: this.code, ...(this.field ? { field: this.field } : {}) };
  }
}

function isAllowedScheme(url) {
  return url.protocol === 'https:' || (url.protocol === 'http:' && LOOPBACK_HOSTS.has(url.hostname));
}

function invalidTrustedOrigins(position) {
  return new OAuthEndpointPolicyError(
    'OAUTH_TRUSTED_ORIGINS_INVALID',
    `${TRUSTED_OAUTH_ORIGINS_ENV} entry ${position} must be an exact https:// origin (or loopback http://) ` +
      'with no path, query, fragment, credentials, or wildcard'
  );
}

/**
 * Parse the operator allow-list: comma-separated exact origins such as
 * "https://login.example.com,https://idp.example.net:8443". Malformed entries
 * fail loudly instead of being ignored or widened.
 * @param {string|undefined} value
 * @returns {Set<string>} Normalized origins
 */
export function parseTrustedOAuthOrigins(value = process.env[TRUSTED_OAUTH_ORIGINS_ENV]) {
  const origins = new Set();
  if (value === undefined) return origins;
  if (typeof value !== 'string') throw invalidTrustedOrigins(1);
  const entries = value.split(',');
  for (let index = 0; index < entries.length; index += 1) {
    const entry = entries[index].trim();
    if (!entry) continue;
    let parsed;
    try {
      parsed = new URL(entry);
    } catch {
      throw invalidTrustedOrigins(index + 1);
    }
    if (entry.includes('*') || entry.includes('?') || entry.includes('#') || parsed.username || parsed.password ||
        parsed.pathname !== '/' || parsed.search || parsed.hash || !isAllowedScheme(parsed)) {
      throw invalidTrustedOrigins(index + 1);
    }
    origins.add(parsed.origin);
  }
  return origins;
}

/**
 * Resolve the effective authorize/token endpoints for an instance URL,
 * defaulting to the instance-hosted ServiceNow OAuth endpoints.
 * @param {string} instanceUrl - Instance URL without a trailing slash
 * @param {{ authorizeUrl?: string, tokenUrl?: string }} [configured]
 */
export function resolveOAuthEndpoints(instanceUrl, { authorizeUrl, tokenUrl } = {}) {
  return {
    authorizeUrl: authorizeUrl || `${instanceUrl}/oauth_auth.do`,
    tokenUrl: tokenUrl || `${instanceUrl}/oauth_token.do`
  };
}

/**
 * Require an OAuth endpoint to share the instance origin or to belong to an
 * operator-approved origin.
 * @param {string} endpoint - authorize or token endpoint URL
 * @param {string} instanceUrl - ServiceNow instance URL
 * @param {'authorizeUrl'|'tokenUrl'} field
 * @param {object} [env] - Environment supplying the allow-list
 * @returns {string} Normalized endpoint URL
 */
export function assertApprovedOAuthEndpoint(endpoint, instanceUrl, field, env = process.env) {
  const trusted = parseTrustedOAuthOrigins(env[TRUSTED_OAUTH_ORIGINS_ENV]);
  let parsed;
  let instance;
  try {
    parsed = new URL(endpoint);
    instance = new URL(instanceUrl);
  } catch {
    throw new OAuthEndpointPolicyError('OAUTH_ENDPOINT_NOT_APPROVED', `${field} must be a valid URL`, field);
  }
  if (parsed.username || parsed.password || parsed.hash || !isAllowedScheme(parsed)) {
    throw new OAuthEndpointPolicyError(
      'OAUTH_ENDPOINT_NOT_APPROVED',
      `${field} must be an HTTPS URL (or loopback HTTP) without credentials or fragment`,
      field
    );
  }
  if (parsed.origin === instance.origin || trusted.has(parsed.origin)) return parsed.href;
  throw new OAuthEndpointPolicyError(
    'OAUTH_ENDPOINT_NOT_APPROVED',
    `${field} origin ${parsed.origin} is not the instance origin and is not listed in ${TRUSTED_OAUTH_ORIGINS_ENV}`,
    field
  );
}
