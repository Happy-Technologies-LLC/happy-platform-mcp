/**
 * Background-script execution through the instance's `/sys.scripts.do`
 * processor (the Scripts - Background page), and the decision of whether a
 * failed request provably did not run the script.
 *
 * Protocol behavior ported from @servicenow/sdk-api 4.14.0, MIT
 * (connector.js runScript/sysParmCk/getCSRFToken, script-execution.js
 * parseScriptResponse, tools/exec withReason/resolveScope). htmlparser2 is
 * replaced by the minimal tag/entity handling the processor output needs.
 */

export const SCRIPT_PROCESSOR_PATH = '/sys.scripts.do';
export const SCRIPT_HISTORY_TABLE = 'sys_script_execution_history';
export const ROLLBACK_CONTEXT_TABLE = 'sys_rollback_context';
/** The processor keeps only this many lines of a run. */
export const PROCESSOR_MAX_LINES = 50001;
/** Same default as now-sdk: the processor itself cancels a run after four hours. */
export const PROCESSOR_DEFAULT_TIMEOUT_MS = 300000;
/** Upper bound on a flattened reason comment appended to the script. */
export const MAX_REASON_LENGTH = 1000;

export const NOT_RUN = 'SCRIPT_PROCESSOR_NOT_RUN';
export const UNKNOWN_OUTCOME = 'SCRIPT_PROCESSOR_UNKNOWN_OUTCOME';

const SCOPE_REGEX = /Script completed in scope (.+?): script/;
const GLOBAL_OUTPUT_PREFIX = '*** Script: ';
const EXECUTION_ERROR_PREFIXES = ['Script execution error: ', 'Script compilation error: '];
const SCOPED_SYNTAX_ERROR_PREFIX = 'SyntaxError: ';
const ERROR_DESCRIPTION_REGEX = /Error Description: ([\s\S]*), Script ES Level:/;
const READ_ONLY_DENIAL = 'ReadOnlyRoleAccessHandler';
const HEX_SYS_ID = /^[0-9a-f]{32}$/i;
const SCOPE_NAME = /^[a-z0-9_]{1,100}$/;

// Failures that happen before a single request byte reaches the instance.
const PRE_SEND_ERROR_CODES = new Set(['ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'EHOSTUNREACH', 'ENETUNREACH']);
const TIMEOUT_ERROR_CODES = new Set(['ECONNABORTED', 'ETIMEDOUT']);
const LOGIN_LOCATION = /login|logout|auth|saml|sso/i;

/** The processor provably did not run the script; a fallback path may run it. */
export class ScriptProcessorNotRunError extends Error {
  constructor(reason) {
    super(`The /sys.scripts.do processor did not run the script: ${reason}`);
    this.name = 'ScriptProcessorNotRunError';
    this.code = NOT_RUN;
    this.reason = reason;
  }
}

/** The script may or may not have run; it must not be re-run automatically. */
export class ScriptProcessorUnknownOutcomeError extends Error {
  constructor(reason) {
    super(`Unknown outcome from the /sys.scripts.do processor: ${reason}`);
    this.name = 'ScriptProcessorUnknownOutcomeError';
    this.code = UNKNOWN_OUTCOME;
    this.reason = reason;
  }
}

/** Mirrors now-sdk `withReason`: one-line reason as a trailing comment. */
export function withReason(script, reason) {
  const flattened = typeof reason === 'string'
    ? reason.replace(/\s+/g, ' ').trim().slice(0, MAX_REASON_LENGTH)
    : '';
  return flattened ? `${script}\n// happy-platform-mcp: ${flattened}` : script;
}

/**
 * Classifies a requested scope: 'global', an application sys_id, or a scope
 * name. Anything else is rejected before a request is made.
 */
export function classifyScope(scope) {
  if (scope === 'global') return { kind: 'global' };
  if (typeof scope === 'string' && HEX_SYS_ID.test(scope)) return { kind: 'sys_id', value: scope.toLowerCase() };
  if (typeof scope === 'string' && SCOPE_NAME.test(scope)) return { kind: 'name', value: scope };
  throw new Error("scope must be 'global', a 32-character hexadecimal application sys_id, or a scope name of lowercase letters, digits and underscores (at most 100 characters)");
}

const NAMED_ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: '\u00a0', '#39': "'" };

function decodeEntities(text) {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+|#39);/gi, (match, entity) => {
    const lower = entity.toLowerCase();
    if (lower.startsWith('#x')) return safeCodePoint(parseInt(lower.slice(2), 16), match);
    if (lower.startsWith('#')) return safeCodePoint(parseInt(lower.slice(1), 10), match);
    return Object.hasOwn(NAMED_ENTITIES, lower) ? NAMED_ENTITIES[lower] : match;
  });
}

function safeCodePoint(code, fallback) {
  return Number.isInteger(code) && code >= 0 && code <= 0x10ffff ? String.fromCodePoint(code) : fallback;
}

// Output is returned to the MCP client as plain text, never rendered as HTML.
// A single left-to-right scan drops `<!-- … -->` comments and everything from
// `<` through the next `>` (unterminated markup is dropped to the end), so
// markup cannot survive or reassemble; literal `<` in script output arrives
// entity-encoded and is restored by decodeEntities afterwards.
function textContent(html) {
  let text = '';
  let index = 0;
  while (index < html.length) {
    if (html.startsWith('<!--', index)) {
      const end = html.indexOf('-->', index + 4);
      index = end === -1 ? html.length : end + 3;
    } else if (html[index] === '<') {
      const end = html.indexOf('>', index + 1);
      index = end === -1 ? html.length : end + 1;
    } else {
      const next = html.indexOf('<', index);
      const stop = next === -1 ? html.length : next;
      text += html.slice(index, stop);
      index = stop;
    }
  }
  return decodeEntities(text);
}

function toSegments(html) {
  const pre = /<pre\b[^>]*>([\s\S]*?)<\/pre\s*>/i.exec(html);
  if (!pre) return [];
  return pre[1].split(/<br\b[^>]*>/i).map(textContent);
}

function findExecutionHistoryId(html) {
  const anchors = /<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi;
  for (const match of html.matchAll(anchors)) {
    const href = decodeEntities(match[1] ?? match[2] ?? match[3] ?? '');
    if (!href.includes(`${SCRIPT_HISTORY_TABLE}.do`)) continue;
    const sysId = new URLSearchParams(href.slice(href.indexOf('?') + 1)).get('sys_id');
    return sysId && HEX_SYS_ID.test(sysId) ? sysId : undefined;
  }
  return undefined;
}

/**
 * Turns a sys.scripts.do response into a result, or null when the body is
 * not a script-runner response. The processor answers 200 for everything, so
 * the "Script completed in scope" line is the presence check; the output
 * prefix is scope-dependent, and unprefixed <PRE> lines are session-log noise.
 */
export function parseScriptResponse(html) {
  if (typeof html !== 'string' || html.trim().length === 0) return null;
  const scopeMatch = SCOPE_REGEX.exec(textContent(html));
  if (!scopeMatch?.[1]) return null;

  const scope = scopeMatch[1];
  const outputPrefix = scope === 'global' ? GLOBAL_OUTPUT_PREFIX : `${scope}: `;
  const output = [];
  const errors = new Set();
  const log = [];
  const syntaxErrors = [];
  let lineCount = 0;
  for (const segment of toSegments(html)) {
    const text = segment.trim();
    if (text.length === 0) continue;
    lineCount++;
    const printed = segment.trimStart();
    const executionErrorPrefix = EXECUTION_ERROR_PREFIXES.find((prefix) => text.startsWith(prefix));
    if (printed.startsWith(outputPrefix)) {
      output.push(printed.slice(outputPrefix.length));
    } else if (executionErrorPrefix) {
      const rest = text.slice(executionErrorPrefix.length);
      errors.add(ERROR_DESCRIPTION_REGEX.exec(rest)?.[1] ?? rest);
    } else if (text.startsWith(SCOPED_SYNTAX_ERROR_PREFIX)) {
      syntaxErrors.push(text.split('\n', 1)[0]);
      log.push(text);
    } else {
      log.push(text);
    }
  }
  if (errors.size === 0) syntaxErrors.forEach((message) => errors.add(message));

  const warnings = [];
  if (log.some((line) => line.includes(READ_ONLY_DENIAL))) {
    warnings.push('The user has the snc_read_only role, so the instance ran the script in sandbox mode and kept no execution history.');
  }
  const truncated = lineCount >= PROCESSOR_MAX_LINES;
  if (truncated) {
    warnings.push(`The instance keeps only the last ${PROCESSOR_MAX_LINES} lines of a run, so earlier output may be missing.`);
  }
  const executionHistoryId = findExecutionHistoryId(html);
  return {
    ok: errors.size === 0,
    scope,
    output,
    errors: [...errors],
    log,
    warnings,
    ...(truncated ? { truncated: true } : {}),
    ...(executionHistoryId ? { executionHistoryId } : {})
  };
}

function headerValue(headers, name) {
  const value = typeof headers?.get === 'function' ? headers.get(name) : headers?.[name];
  return Array.isArray(value) ? value[0] : value;
}

/** A body that is an unauthenticated/login response rather than a processor result. */
export function isAuthFailurePage(body, headers) {
  if (String(headerValue(headers, 'x-is-logged-in') ?? '').toLowerCase() === 'false') return true;
  if (typeof body !== 'string' || body.trim().length === 0) return true;
  return /name\s*=\s*["']?user_password\b/i.test(body)
    || /<form\b[^>]*action\s*=\s*["']?[^"'>]*login\.do/i.test(body)
    || /<title>[^<]*\blog\s?in\b/i.test(body);
}

/**
 * Decides what a /sys.scripts.do response or transport error means.
 * Returns { ran: true, result } | { ran: false, reason } | { ran: 'unknown', reason }.
 * Only outcomes that prove the script never executed return ran: false.
 */
export function classifyProcessorOutcome({ response, error }) {
  if (!response) {
    const code = typeof error?.code === 'string' ? error.code : undefined;
    if (code && PRE_SEND_ERROR_CODES.has(code)) {
      return { ran: false, reason: `connection error before the request was sent (${code})` };
    }
    if ((code && TIMEOUT_ERROR_CODES.has(code)) || /timeout/i.test(error?.message || '')) {
      return { ran: 'unknown', reason: 'stopped waiting for the processor response (timeout); the script may still be running on the instance' };
    }
    return { ran: 'unknown', reason: `the connection failed after the request may have been sent${code ? ` (${code})` : ''}` };
  }

  const { status, headers, data } = response;
  if (status === 401 || status === 403) {
    const hint = status === 403 ? ' (running background scripts requires the admin role)' : '';
    return { ran: false, reason: `HTTP ${status} from ${SCRIPT_PROCESSOR_PATH}${hint}` };
  }
  if (status === 404) {
    return { ran: false, reason: `HTTP 404: ${SCRIPT_PROCESSOR_PATH} is not available on this instance` };
  }
  if (status >= 300 && status < 400) {
    const location = String(headerValue(headers, 'location') ?? '');
    return LOGIN_LOCATION.test(location)
      ? { ran: false, reason: `HTTP ${status} redirect to a login/authentication page` }
      : { ran: 'unknown', reason: `HTTP ${status} redirect that is not a login page` };
  }
  if (status >= 200 && status < 300) {
    const body = typeof data === 'string' ? data : '';
    const result = parseScriptResponse(body);
    if (result) return { ran: true, result };
    // Observed live: a scope the user may not run scripts in (e.g. a
    // protected store application) answers 200 with exactly this body and
    // creates no execution history.
    if (body.trim().toLowerCase() === 'not authorized') {
      return { ran: false, reason: 'the processor refused the request (not authorized): the user may not be allowed to run scripts in this scope, e.g. a protected store application' };
    }
    if (isAuthFailurePage(body, headers)) {
      return { ran: false, reason: 'the processor returned an unauthenticated/login page instead of a script result' };
    }
    return { ran: 'unknown', reason: `HTTP ${status} response that is not a recognizable script result` };
  }
  return { ran: 'unknown', reason: `HTTP ${status} from ${SCRIPT_PROCESSOR_PATH}` };
}

/** `name=value` pairs from Set-Cookie headers, merged into `jar` (a Map). */
export function mergeSetCookies(jar, headers) {
  const raw = typeof headers?.getSetCookie === 'function' ? headers.getSetCookie() : headers?.['set-cookie'];
  for (const entry of Array.isArray(raw) ? raw : raw ? [raw] : []) {
    const pair = String(entry).split(';', 1)[0];
    const separator = pair.indexOf('=');
    if (separator <= 0) continue;
    jar.set(pair.slice(0, separator).trim(), pair.slice(separator + 1).trim());
  }
  return jar;
}

export function serializeCookies(jar) {
  return Array.from(jar, ([name, value]) => `${name}=${value}`).join('; ');
}

export function scriptExecutionLinks({ executionHistoryId, rollbackContextId }) {
  return {
    ...(executionHistoryId ? { executionHistory: `/${SCRIPT_HISTORY_TABLE}.do?sys_id=${executionHistoryId}` } : {}),
    ...(rollbackContextId ? { rollbackContext: `/${ROLLBACK_CONTEXT_TABLE}.do?sys_id=${rollbackContextId}` } : {})
  };
}

/** Reads `rollback_context` from a Table API row (plain value or reference object). */
export function rollbackContextOf(record) {
  const value = record?.rollback_context;
  const id = typeof value === 'object' && value !== null ? value.value : value;
  return typeof id === 'string' && HEX_SYS_ID.test(id) ? id : undefined;
}
