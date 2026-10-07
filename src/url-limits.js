/**
 * URL/path input bounds shared by the instance registry, OAuth endpoint policy
 * and ServiceNow client (issue #67: VULN-024..029, VULN-031).
 *
 * Trailing/leading slash trimming uses linear character scans: a regex such as
 * /\/+$/ backtracks quadratically on long slash runs followed by another
 * character. Every URL or path accepted from configuration, environment, MCP
 * tool input or direct client construction is capped at MAX_URL_LENGTH before
 * it is parsed or normalized.
 */

export const MAX_URL_LENGTH = 4096;

const SLASH = 47;

/** Remove every trailing '/' in O(n). */
export function trimTrailingSlashes(value) {
  let end = value.length;
  while (end > 0 && value.charCodeAt(end - 1) === SLASH) end -= 1;
  return end === value.length ? value : value.slice(0, end);
}

/** Remove every leading '/' in O(n). */
export function trimLeadingSlashes(value) {
  let start = 0;
  while (start < value.length && value.charCodeAt(start) === SLASH) start += 1;
  return start === 0 ? value : value.slice(start);
}

/** True when value is a string longer than MAX_URL_LENGTH characters. */
export function exceedsUrlLength(value) {
  return typeof value === 'string' && value.length > MAX_URL_LENGTH;
}
