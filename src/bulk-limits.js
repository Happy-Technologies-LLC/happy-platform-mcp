/**
 * Input bounds for multi-record ServiceNow operations (issue #67: VULN-012,
 * VULN-032..035).
 *
 * MCP clients are not required to enforce JSON-schema limits, so every bulk
 * entry point validates its whole input here before the first ServiceNow
 * request. Limits fail loudly; nothing is silently truncated.
 */

export const MAX_BATCH_OPERATIONS = 100;
export const MAX_WORKFLOW_ACTIVITIES = 100;
export const MAX_WORKFLOW_TRANSITIONS = 200;
export const MAX_UPDATE_SET_RECORD_IDS = 200;
export const MAX_OPERATION_DATA_BYTES = 64 * 1024;
export const MAX_SCRIPT_BYTES = 256 * 1024;
export const MAX_WORKFLOW_INPUT_BYTES = 4 * 1024 * 1024;
export const MAX_NAME_LENGTH = 255;
export const MAX_TABLE_NAME_LENGTH = 80;
export const MAX_SAVE_AS_LENGTH = 64;
/** sys_ids per `sys_idIN` lookup; keeps each encoded query near 3.3 KB. */
export const UPDATE_SET_ID_CHUNK_SIZE = 100;
/** Page size for paginated update-set record queries. */
export const UPDATE_SET_QUERY_PAGE_SIZE = 1000;
/** Upper bound on records a single move/clone may touch. */
export const MAX_UPDATE_SET_QUERY_RECORDS = 10000;

export const SYS_ID_PATTERN = '^[0-9a-f]{32}$';
export const TABLE_NAME_PATTERN = '^[A-Za-z0-9_]{1,80}$';
export const SAVE_AS_PATTERN = '^[A-Za-z_][A-Za-z0-9_]{0,63}$';
export const DATE_TIME_PATTERN = '^\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}:\\d{2}$';

const SYS_ID_RE = new RegExp(SYS_ID_PATTERN);
const TABLE_NAME_RE = new RegExp(TABLE_NAME_PATTERN);
const SAVE_AS_RE = new RegExp(SAVE_AS_PATTERN);
const DATE_TIME_RE = new RegExp(DATE_TIME_PATTERN);

export class BulkInputError extends Error {
  constructor(message) {
    super(message);
    this.name = 'BulkInputError';
    this.code = 'BULK_INPUT_INVALID';
  }
}

function fail(message) {
  throw new BulkInputError(message);
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function utf8Bytes(value) {
  return Buffer.byteLength(value, 'utf8');
}

/** JSON-serialize for size accounting; cyclic or too-deep input is invalid. */
function serialize(value, field) {
  try {
    return JSON.stringify(value);
  } catch {
    return fail(`${field} must be JSON-serializable`);
  }
}

function assertArray(value, field, max) {
  if (!Array.isArray(value)) fail(`${field} must be an array`);
  if (value.length > max) fail(`${field} accepts at most ${max} entries (received ${value.length})`);
}

export function isSysId(value) {
  return typeof value === 'string' && SYS_ID_RE.test(value);
}

export function assertSysId(value, field) {
  if (typeof value !== 'string' || !SYS_ID_RE.test(value)) {
    fail(`${field} must be a 32-character lowercase hexadecimal sys_id`);
  }
}

export function assertTableName(value, field) {
  if (typeof value !== 'string' || !TABLE_NAME_RE.test(value)) {
    fail(`${field} must be a table name of 1-${MAX_TABLE_NAME_LENGTH} letters, digits or underscores`);
  }
}

export function assertName(value, field, { required = true } = {}) {
  if (value === undefined && !required) return;
  if (typeof value !== 'string' || (required && value.length === 0) || value.length > MAX_NAME_LENGTH) {
    fail(`${field} must be a ${required ? 'non-empty ' : ''}string of at most ${MAX_NAME_LENGTH} characters`);
  }
}

function assertOptionalSysId(value, field) {
  if (value !== undefined && value !== null && value !== '') assertSysId(value, field);
}

function assertScript(value, field) {
  if (value === undefined || value === null) return;
  if (typeof value !== 'string') fail(`${field} must be a string`);
  // Byte length >= character length, so the cheap check short-circuits huge input.
  if (value.length > MAX_SCRIPT_BYTES || utf8Bytes(value) > MAX_SCRIPT_BYTES) {
    fail(`${field} must be at most ${MAX_SCRIPT_BYTES} bytes`);
  }
}

function assertRecordData(value, field) {
  if (!isPlainObject(value)) fail(`${field} must be an object of field values`);
  const serialized = serialize(value, field);
  if (utf8Bytes(serialized) > MAX_OPERATION_DATA_BYTES) {
    fail(`${field} must serialize to at most ${MAX_OPERATION_DATA_BYTES} bytes`);
  }
  return serialized;
}

/** Names that would alias Object.prototype members or the default `operation_N` keys. */
const RESERVED_SAVE_AS = new Set(['__proto__', 'constructor', 'prototype']);
const DEFAULT_OPERATION_KEY_RE = /^operation_\d+$/;

/**
 * Validate an entire SN-Batch-Create input.
 * @returns {string[]} serialized data per operation (reused for placeholder resolution)
 */
export function validateBatchCreateOperations(operations) {
  assertArray(operations, 'operations', MAX_BATCH_OPERATIONS);
  const saveAsNames = new Set();
  return operations.map((op, i) => {
    if (!isPlainObject(op)) fail(`operations[${i}] must be an object`);
    assertTableName(op.table, `operations[${i}].table`);
    if (op.save_as !== undefined) {
      const field = `operations[${i}].save_as`;
      if (typeof op.save_as !== 'string' || !SAVE_AS_RE.test(op.save_as)) fail(`${field} must match ${SAVE_AS_PATTERN}`);
      if (RESERVED_SAVE_AS.has(op.save_as) || DEFAULT_OPERATION_KEY_RE.test(op.save_as)) {
        fail(`${field} '${op.save_as}' is reserved`);
      }
      if (saveAsNames.has(op.save_as)) fail(`${field} '${op.save_as}' duplicates an earlier save_as`);
      saveAsNames.add(op.save_as);
    }
    return assertRecordData(op.data, `operations[${i}].data`);
  });
}

/** Validate an entire SN-Batch-Update input. */
export function validateBatchUpdates(updates) {
  assertArray(updates, 'updates', MAX_BATCH_OPERATIONS);
  updates.forEach((update, i) => {
    if (!isPlainObject(update)) fail(`updates[${i}] must be an object`);
    assertTableName(update.table, `updates[${i}].table`);
    assertSysId(update.sys_id, `updates[${i}].sys_id`);
    assertRecordData(update.data, `updates[${i}].data`);
  });
}

/**
 * A transition endpoint or start_activity is an activity index, a declared
 * activities[].id or unique activities[].name, or an existing activity sys_id.
 */
function assertActivityReference(value, field, activityCount, references, ambiguous) {
  if (typeof value === 'number') {
    if (!Number.isInteger(value) || value < 0 || value >= activityCount) {
      fail(`${field} must be an activity index from 0 to ${activityCount - 1}`);
    }
    return;
  }
  assertName(value, field);
  if (ambiguous.has(value)) fail(`${field} '${value}' matches more than one activity id or name`);
  if (!references.has(value) && !SYS_ID_RE.test(value)) {
    fail(`${field} must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id`);
  }
}

/**
 * Validate an entire SN-Create-Workflow specification.
 * @returns {Map<string, number>} unambiguous activity id/name -> activity index
 */
export function validateWorkflowSpec(spec) {
  if (!isPlainObject(spec)) fail('workflow specification must be an object');
  assertName(spec.name, 'name');
  assertTableName(spec.table, 'table');
  if (spec.description !== undefined && typeof spec.description !== 'string') fail('description must be a string');
  assertScript(spec.condition, 'condition');

  const activities = spec.activities === undefined ? [] : spec.activities;
  assertArray(activities, 'activities', MAX_WORKFLOW_ACTIVITIES);
  const transitions = spec.transitions === undefined ? [] : spec.transitions;
  assertArray(transitions, 'transitions', MAX_WORKFLOW_TRANSITIONS);

  const references = new Map();
  const ambiguous = new Set();
  const declaredIds = new Set();
  const addReference = (key, index) => {
    const previous = references.get(key);
    if (previous === undefined) references.set(key, index);
    else if (previous !== index) ambiguous.add(key);
  };
  activities.forEach((activity, i) => {
    const field = `activities[${i}]`;
    if (!isPlainObject(activity)) fail(`${field} must be an object`);
    assertName(activity.name, `${field}.name`);
    assertName(activity.id, `${field}.id`, { required: false });
    if (activity.id !== undefined) {
      if (declaredIds.has(activity.id)) fail(`${field}.id '${activity.id}' duplicates an earlier activity id`);
      declaredIds.add(activity.id);
      addReference(activity.id, i);
    }
    addReference(activity.name, i);
    for (const key of ['script', 'input', 'vars']) assertScript(activity[key], `${field}.${key}`);
    for (const key of ['activity_definition_sys_id', 'activity_type', 'stage', 'parent']) {
      assertOptionalSysId(activity[key], `${field}.${key}`);
    }
  });
  for (const key of ambiguous) references.delete(key);

  transitions.forEach((transition, i) => {
    const field = `transitions[${i}]`;
    if (!isPlainObject(transition)) fail(`${field} must be an object`);
    assertActivityReference(transition.from, `${field}.from`, activities.length, references, ambiguous);
    assertActivityReference(transition.to, `${field}.to`, activities.length, references, ambiguous);
    for (const key of ['condition', 'condition_script']) assertScript(transition[key], `${field}.${key}`);
    assertName(transition.condition_name, `${field}.condition_name`, { required: false });
    assertOptionalSysId(transition.condition_sys_id, `${field}.condition_sys_id`);
  });
  if (spec.start_activity !== undefined) {
    assertActivityReference(spec.start_activity, 'start_activity', activities.length, references, ambiguous);
  }

  if (utf8Bytes(serialize(spec, 'workflow specification')) > MAX_WORKFLOW_INPUT_BYTES) {
    fail(`workflow specification must serialize to at most ${MAX_WORKFLOW_INPUT_BYTES} bytes`);
  }
  return references;
}

function assertQueryValue(value, field) {
  assertName(value, field);
  for (let i = 0; i < value.length; i += 1) {
    const code = value.charCodeAt(i);
    if (code === 0x5e || code < 0x20 || code === 0x7f) {
      fail(`${field} must not contain '^' or control characters`);
    }
  }
}

/**
 * Validate SN-Move-Records-To-Update-Set input.
 * @returns {string[]} de-duplicated record sys_ids
 */
export function validateUpdateSetMove(updateSetId, { record_sys_ids = [], time_range = null, source_update_set = null, table = 'sys_update_xml' } = {}) {
  assertSysId(updateSetId, 'update_set_id');
  assertTableName(table, 'table');
  assertArray(record_sys_ids, 'record_sys_ids', MAX_UPDATE_SET_RECORD_IDS);
  record_sys_ids.forEach((id, i) => assertSysId(id, `record_sys_ids[${i}]`));
  if (time_range !== null && time_range !== undefined) {
    if (!isPlainObject(time_range)) fail('time_range must be an object');
    for (const key of ['start', 'end']) {
      if (typeof time_range[key] !== 'string' || !DATE_TIME_RE.test(time_range[key])) {
        fail(`time_range.${key} must use the format YYYY-MM-DD HH:MM:SS`);
      }
    }
  }
  if (source_update_set !== null && source_update_set !== undefined) {
    assertQueryValue(source_update_set, 'source_update_set');
  }
  return [...new Set(record_sys_ids)];
}

/** Validate SN-Clone-Update-Set input. */
export function validateUpdateSetClone(sourceUpdateSetId, newName) {
  assertSysId(sourceUpdateSetId, 'source_update_set_id');
  assertName(newName, 'new_name');
}

const CLOSE_BRACE = 0x7d;

function isNameChar(code) {
  return (code >= 0x30 && code <= 0x39) || (code >= 0x41 && code <= 0x5a) ||
    (code >= 0x61 && code <= 0x7a) || code === 0x5f;
}

/**
 * Replace `${name}` references to previously saved sys_ids inside serialized
 * JSON in one left-to-right pass. Names are bounded by MAX_SAVE_AS_LENGTH, so
 * each candidate costs O(1) and the scan is linear in the input. Unknown,
 * forward, malformed or spaced references are left as literal text, and
 * substituted values are never rescanned.
 * @param {string} serialized - JSON produced by JSON.stringify
 * @param {Map<string,string>} savedIds - name -> sys_id saved by earlier operations
 * @returns {string} JSON with references resolved
 */
export function resolveBatchPlaceholders(serialized, savedIds) {
  if (savedIds.size === 0) return serialized;
  let out = '';
  let copied = 0;
  let i = serialized.indexOf('${');
  while (i !== -1) {
    let end = i + 2;
    const limit = Math.min(serialized.length, end + MAX_SAVE_AS_LENGTH);
    while (end < limit && isNameChar(serialized.charCodeAt(end))) end += 1;
    if (end > i + 2 && serialized.charCodeAt(end) === CLOSE_BRACE) {
      const id = savedIds.get(serialized.slice(i + 2, end));
      if (id !== undefined) {
        // JSON-escape the saved value so the result remains valid JSON.
        out += serialized.slice(copied, i) + JSON.stringify(id).slice(1, -1);
        copied = end + 1;
        i = serialized.indexOf('${', copied);
        continue;
      }
    }
    i = serialized.indexOf('${', i + 1);
  }
  return copied === 0 ? serialized : out + serialized.slice(copied);
}
