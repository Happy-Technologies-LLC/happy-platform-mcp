/**
 * Public links printed by the CLI and servers. One place to change them.
 */

export const UPDATES_URL = 'https://happy-tech.biz/mcp';
export const DISCUSSIONS_URL = 'https://github.com/Happy-Technologies-LLC/happy-platform-mcp/discussions';
/** Where opt-in usage pings go. Override with HAPPY_MCP_TELEMETRY_URL. */
export const TELEMETRY_URL = 'https://build01-lab.tail0490c9.ts.net:10000/api/mcp/telemetry';

/**
 * One line shown after a successful setup step. Kept short so it reads as a
 * pointer, not a banner. Goes to stderr so stdout stays machine-readable.
 */
export function updatesHint() {
  return `Release notes and security advisories: ${UPDATES_URL}`;
}
