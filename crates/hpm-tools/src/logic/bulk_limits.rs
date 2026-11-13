//! Input bounds for multi-record ServiceNow operations. Ports `src/bulk-limits.js`
//! (issue #67: VULN-012, VULN-032..035).
//!
//! MCP clients are not required to enforce JSON-schema limits, so every bulk
//! entry point validates its whole input here before the first ServiceNow
//! request. Limits fail loudly; nothing is silently truncated.
//!
//! Inputs are untyped JSON as in JavaScript: a JavaScript `undefined`
//! property is an absent key (`None`), and JSON `null` is a present value.
//! Lengths are JavaScript `string.length` (UTF-16 code units); byte sizes are
//! UTF-8 bytes of the `JSON.stringify` text.

use super::js;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub const MAX_BATCH_OPERATIONS: usize = 100;
pub const MAX_WORKFLOW_ACTIVITIES: usize = 100;
pub const MAX_WORKFLOW_TRANSITIONS: usize = 200;
pub const MAX_UPDATE_SET_RECORD_IDS: usize = 200;
pub const MAX_OPERATION_DATA_BYTES: usize = 64 * 1024;
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
pub const MAX_WORKFLOW_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_NAME_LENGTH: usize = 255;
pub const MAX_TABLE_NAME_LENGTH: usize = 80;
pub const MAX_SAVE_AS_LENGTH: usize = 64;
/// sys_ids per `sys_idIN` lookup; keeps each encoded query near 3.3 KB.
pub const UPDATE_SET_ID_CHUNK_SIZE: usize = 100;
/// Page size for paginated update-set record queries.
pub const UPDATE_SET_QUERY_PAGE_SIZE: usize = 1000;
/// Upper bound on records a single move/clone may touch.
pub const MAX_UPDATE_SET_QUERY_RECORDS: usize = 10000;

pub const SYS_ID_PATTERN: &str = "^[0-9a-f]{32}$";
pub const TABLE_NAME_PATTERN: &str = "^[A-Za-z0-9_]{1,80}$";
pub const SAVE_AS_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_]{0,63}$";
pub const DATE_TIME_PATTERN: &str = "^\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}:\\d{2}$";

/// A bulk input failed validation. `name` is `BulkInputError`, `code` is
/// [`BulkInputError::CODE`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BulkInputError {
    pub message: String,
}

impl BulkInputError {
    pub const NAME: &'static str = "BulkInputError";
    pub const CODE: &'static str = "BULK_INPUT_INVALID";
}

type Result<T, E = BulkInputError> = std::result::Result<T, E>;

fn fail<T>(message: String) -> Result<T> {
    Err(BulkInputError { message })
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `SYS_ID_PATTERN` test.
fn matches_sys_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `TABLE_NAME_PATTERN` test.
fn matches_table_name(value: &str) -> bool {
    (1..=MAX_TABLE_NAME_LENGTH).contains(&value.len()) && value.bytes().all(is_word_byte)
}

/// `SAVE_AS_PATTERN` test.
fn matches_save_as(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=MAX_SAVE_AS_LENGTH).contains(&bytes.len())
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes.iter().all(|b| is_word_byte(*b))
}

/// `DATE_TIME_PATTERN` test (`\d` is ASCII-only in JavaScript).
fn matches_date_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            10 => *b == b' ',
            13 | 16 => *b == b':',
            _ => b.is_ascii_digit(),
        })
}

/// `/^operation_\d+$/` test.
fn is_default_operation_key(value: &str) -> bool {
    value
        .strip_prefix("operation_")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn plain_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn assert_array<'a>(value: Option<&'a Value>, field: &str, max: usize) -> Result<&'a [Value]> {
    let Some(Value::Array(items)) = value else {
        return fail(format!("{field} must be an array"));
    };
    if items.len() > max {
        return fail(format!(
            "{field} accepts at most {max} entries (received {})",
            items.len()
        ));
    }
    Ok(items)
}

/// `isSysId(value)`: a string matching [`SYS_ID_PATTERN`].
pub fn is_sys_id(value: &str) -> bool {
    matches_sys_id(value)
}

/// `assertSysId(value, field)`.
pub fn assert_sys_id(value: Option<&Value>, field: &str) -> Result<()> {
    match value {
        Some(Value::String(text)) if matches_sys_id(text) => Ok(()),
        _ => fail(format!(
            "{field} must be a 32-character lowercase hexadecimal sys_id"
        )),
    }
}

/// `assertTableName(value, field)`.
pub fn assert_table_name(value: Option<&Value>, field: &str) -> Result<()> {
    match value {
        Some(Value::String(text)) if matches_table_name(text) => Ok(()),
        _ => fail(format!(
            "{field} must be a table name of 1-{MAX_TABLE_NAME_LENGTH} letters, digits or underscores"
        )),
    }
}

/// `assertName(value, field, { required })`.
pub fn assert_name(value: Option<&Value>, field: &str, required: bool) -> Result<()> {
    if value.is_none() && !required {
        return Ok(());
    }
    match value {
        Some(Value::String(text))
            if !(required && text.is_empty()) && !js::utf16_len_exceeds(text, MAX_NAME_LENGTH) =>
        {
            Ok(())
        }
        _ => fail(format!(
            "{field} must be a {}string of at most {MAX_NAME_LENGTH} characters",
            if required { "non-empty " } else { "" }
        )),
    }
}

fn assert_optional_sys_id(value: Option<&Value>, field: &str) -> Result<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(text)) if text.is_empty() => Ok(()),
        Some(_) => assert_sys_id(value, field),
    }
}

fn assert_script(value: Option<&Value>, field: &str) -> Result<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        // UTF-8 bytes >= UTF-16 code units, so the byte check covers both JS checks.
        Some(Value::String(text)) if text.len() > MAX_SCRIPT_BYTES => {
            fail(format!("{field} must be at most {MAX_SCRIPT_BYTES} bytes"))
        }
        Some(Value::String(_)) => Ok(()),
        Some(_) => fail(format!("{field} must be a string")),
    }
}

fn assert_record_data(value: Option<&Value>, field: &str) -> Result<String> {
    let Some(object @ Value::Object(_)) = value else {
        return fail(format!("{field} must be an object of field values"));
    };
    let serialized = js::json_stringify(object);
    if serialized.len() > MAX_OPERATION_DATA_BYTES {
        return fail(format!(
            "{field} must serialize to at most {MAX_OPERATION_DATA_BYTES} bytes"
        ));
    }
    Ok(serialized)
}

/// Names that would alias Object.prototype members or the default `operation_N` keys.
const RESERVED_SAVE_AS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// Validate an entire SN-Batch-Create input. Returns the serialized data per
/// operation (reused for placeholder resolution).
pub fn validate_batch_create_operations(operations: Option<&Value>) -> Result<Vec<String>> {
    let operations = assert_array(operations, "operations", MAX_BATCH_OPERATIONS)?;
    let mut save_as_names: HashSet<&str> = HashSet::new();
    let mut serialized = Vec::with_capacity(operations.len());
    for (i, op) in operations.iter().enumerate() {
        let Some(op) = op.as_object() else {
            return fail(format!("operations[{i}] must be an object"));
        };
        assert_table_name(op.get("table"), &format!("operations[{i}].table"))?;
        if let Some(save_as) = op.get("save_as") {
            let field = format!("operations[{i}].save_as");
            let save_as = match save_as {
                Value::String(text) if matches_save_as(text) => text.as_str(),
                _ => return fail(format!("{field} must match {SAVE_AS_PATTERN}")),
            };
            if RESERVED_SAVE_AS.contains(&save_as) || is_default_operation_key(save_as) {
                return fail(format!("{field} '{save_as}' is reserved"));
            }
            if !save_as_names.insert(save_as) {
                return fail(format!("{field} '{save_as}' duplicates an earlier save_as"));
            }
        }
        serialized.push(assert_record_data(
            op.get("data"),
            &format!("operations[{i}].data"),
        )?);
    }
    Ok(serialized)
}

/// Validate an entire SN-Batch-Update input.
pub fn validate_batch_updates(updates: Option<&Value>) -> Result<()> {
    let updates = assert_array(updates, "updates", MAX_BATCH_OPERATIONS)?;
    for (i, update) in updates.iter().enumerate() {
        let Some(update) = update.as_object() else {
            return fail(format!("updates[{i}] must be an object"));
        };
        assert_table_name(update.get("table"), &format!("updates[{i}].table"))?;
        assert_sys_id(update.get("sys_id"), &format!("updates[{i}].sys_id"))?;
        assert_record_data(update.get("data"), &format!("updates[{i}].data"))?;
    }
    Ok(())
}

/// A transition endpoint or start_activity is an activity index, a declared
/// activities[].id or unique activities[].name, or an existing activity sys_id.
fn assert_activity_reference(
    value: Option<&Value>,
    field: &str,
    activity_count: usize,
    references: &HashMap<String, usize>,
    ambiguous: &HashSet<String>,
) -> Result<()> {
    if let Some(Value::Number(number)) = value {
        let index = number.as_f64().unwrap_or(f64::NAN);
        if !index.is_finite()
            || index.fract() != 0.0
            || index < 0.0
            || index >= activity_count as f64
        {
            return fail(format!(
                "{field} must be an activity index from 0 to {}",
                activity_count as i64 - 1
            ));
        }
        return Ok(());
    }
    assert_name(value, field, true)?;
    let Some(Value::String(name)) = value else {
        return fail(format!(
            "{field} must be a non-empty string of at most {MAX_NAME_LENGTH} characters"
        ));
    };
    if ambiguous.contains(name) {
        return fail(format!(
            "{field} '{name}' matches more than one activity id or name"
        ));
    }
    if !references.contains_key(name) && !matches_sys_id(name) {
        return fail(format!(
            "{field} must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id"
        ));
    }
    Ok(())
}

/// Validate an entire SN-Create-Workflow specification. Returns the
/// unambiguous activity id/name -> activity index map.
pub fn validate_workflow_spec(spec: Option<&Value>) -> Result<HashMap<String, usize>> {
    let Some(object) = plain_object(spec) else {
        return fail("workflow specification must be an object".to_owned());
    };
    assert_name(object.get("name"), "name", true)?;
    assert_table_name(object.get("table"), "table")?;
    if matches!(object.get("description"), Some(value) if !value.is_string()) {
        return fail("description must be a string".to_owned());
    }
    assert_script(object.get("condition"), "condition")?;

    let empty = Value::Array(Vec::new());
    let activities = assert_array(
        Some(object.get("activities").unwrap_or(&empty)),
        "activities",
        MAX_WORKFLOW_ACTIVITIES,
    )?;
    let transitions = assert_array(
        Some(object.get("transitions").unwrap_or(&empty)),
        "transitions",
        MAX_WORKFLOW_TRANSITIONS,
    )?;

    let mut references: HashMap<String, usize> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    let mut declared_ids: HashSet<&str> = HashSet::new();
    let mut add_reference = |key: &str, index: usize| match references.get(key) {
        None => {
            references.insert(key.to_owned(), index);
        }
        Some(previous) if *previous != index => {
            ambiguous.insert(key.to_owned());
        }
        Some(_) => {}
    };
    for (i, activity) in activities.iter().enumerate() {
        let field = format!("activities[{i}]");
        let Some(activity) = activity.as_object() else {
            return fail(format!("{field} must be an object"));
        };
        assert_name(activity.get("name"), &format!("{field}.name"), true)?;
        assert_name(activity.get("id"), &format!("{field}.id"), false)?;
        if let Some(Value::String(id)) = activity.get("id") {
            if !declared_ids.insert(id) {
                return fail(format!(
                    "{field}.id '{id}' duplicates an earlier activity id"
                ));
            }
            add_reference(id, i);
        }
        if let Some(Value::String(name)) = activity.get("name") {
            add_reference(name, i);
        }
        for key in ["script", "input", "vars"] {
            assert_script(activity.get(key), &format!("{field}.{key}"))?;
        }
        for key in [
            "activity_definition_sys_id",
            "activity_type",
            "stage",
            "parent",
        ] {
            assert_optional_sys_id(activity.get(key), &format!("{field}.{key}"))?;
        }
    }
    for key in &ambiguous {
        references.remove(key);
    }

    for (i, transition) in transitions.iter().enumerate() {
        let field = format!("transitions[{i}]");
        let Some(transition) = transition.as_object() else {
            return fail(format!("{field} must be an object"));
        };
        for end in ["from", "to"] {
            assert_activity_reference(
                transition.get(end),
                &format!("{field}.{end}"),
                activities.len(),
                &references,
                &ambiguous,
            )?;
        }
        for key in ["condition", "condition_script"] {
            assert_script(transition.get(key), &format!("{field}.{key}"))?;
        }
        assert_name(
            transition.get("condition_name"),
            &format!("{field}.condition_name"),
            false,
        )?;
        assert_optional_sys_id(
            transition.get("condition_sys_id"),
            &format!("{field}.condition_sys_id"),
        )?;
    }
    if let Some(start) = object.get("start_activity") {
        assert_activity_reference(
            Some(start),
            "start_activity",
            activities.len(),
            &references,
            &ambiguous,
        )?;
    }

    if spec.map_or(0, |spec| js::json_stringify(spec).len()) > MAX_WORKFLOW_INPUT_BYTES {
        return fail(format!(
            "workflow specification must serialize to at most {MAX_WORKFLOW_INPUT_BYTES} bytes"
        ));
    }
    Ok(references)
}

fn assert_query_value(value: Option<&Value>, field: &str) -> Result<()> {
    assert_name(value, field, true)?;
    if let Some(Value::String(text)) = value
        && (crate::logic::encoded_query::query_value(field, text).is_err()
            // The shared guard covers `^` and C0/DEL/C1; also refuse the JS
            // line terminators U+2028/U+2029.
            || text.contains(['\u{2028}', '\u{2029}']))
    {
        return fail(format!(
            "{field} must not contain '^' or control characters"
        ));
    }
    Ok(())
}

/// Validate SN-Move-Records-To-Update-Set input. `options` holds the
/// `record_sys_ids`, `time_range`, `source_update_set` and `table` arguments;
/// an absent key takes the JavaScript default. Returns the de-duplicated
/// record sys_ids in first-seen order.
pub fn validate_update_set_move(
    update_set_id: Option<&Value>,
    options: &Map<String, Value>,
) -> Result<Vec<String>> {
    let default_table = Value::String("sys_update_xml".to_owned());
    let empty = Value::Array(Vec::new());
    assert_sys_id(update_set_id, "update_set_id")?;
    assert_table_name(
        Some(options.get("table").unwrap_or(&default_table)),
        "table",
    )?;
    let record_sys_ids = assert_array(
        Some(options.get("record_sys_ids").unwrap_or(&empty)),
        "record_sys_ids",
        MAX_UPDATE_SET_RECORD_IDS,
    )?;
    for (i, id) in record_sys_ids.iter().enumerate() {
        assert_sys_id(Some(id), &format!("record_sys_ids[{i}]"))?;
    }
    match options.get("time_range") {
        None | Some(Value::Null) => {}
        Some(Value::Object(range)) => {
            for key in ["start", "end"] {
                if !matches!(range.get(key), Some(Value::String(text)) if matches_date_time(text)) {
                    return fail(format!(
                        "time_range.{key} must use the format YYYY-MM-DD HH:MM:SS"
                    ));
                }
            }
        }
        Some(_) => return fail("time_range must be an object".to_owned()),
    }
    match options.get("source_update_set") {
        None | Some(Value::Null) => {}
        source => assert_query_value(source, "source_update_set")?,
    }
    let mut seen: HashSet<&str> = HashSet::new();
    Ok(record_sys_ids
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| seen.insert(id))
        .map(str::to_owned)
        .collect())
}

/// Validate SN-Clone-Update-Set input.
pub fn validate_update_set_clone(
    source_update_set_id: Option<&Value>,
    new_name: Option<&Value>,
) -> Result<()> {
    assert_sys_id(source_update_set_id, "source_update_set_id")?;
    assert_name(new_name, "new_name", true)
}

/// Replace `${name}` references to previously saved sys_ids inside serialized
/// JSON in one left-to-right pass. Names are bounded by [`MAX_SAVE_AS_LENGTH`],
/// so each candidate costs O(1) and the scan is linear in the input. Unknown,
/// forward, malformed or spaced references are left as literal text, and
/// substituted values are never rescanned.
pub fn resolve_batch_placeholders(serialized: &str, saved_ids: &HashMap<String, String>) -> String {
    if saved_ids.is_empty() {
        return serialized.to_owned();
    }
    let bytes = serialized.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut next = serialized.find("${");
    while let Some(i) = next {
        let mut end = i + 2;
        let limit = bytes.len().min(end + MAX_SAVE_AS_LENGTH);
        while end < limit && is_word_byte(bytes[end]) {
            end += 1;
        }
        if end > i + 2
            && bytes.get(end) == Some(&b'}')
            && let Some(id) = saved_ids.get(&serialized[i + 2..end])
        {
            // JSON-escape the saved value so the result remains valid JSON.
            let quoted = js::json_stringify_str(id);
            out.push_str(&serialized[copied..i]);
            out.push_str(&quoted[1..quoted.len() - 1]);
            copied = end + 1;
            next = serialized[copied..].find("${").map(|at| at + copied);
            continue;
        }
        next = serialized[i + 1..].find("${").map(|at| at + i + 1);
    }
    if copied == 0 {
        serialized.to_owned()
    } else {
        out.push_str(&serialized[copied..]);
        out
    }
}
