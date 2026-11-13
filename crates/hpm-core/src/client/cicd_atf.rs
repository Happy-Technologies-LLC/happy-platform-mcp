//! Automated Test Framework (ATF) tools over the sn_cicd REST API
//! (`src/cicd-atf.js`, 6.2.0).
//!
//! sn_cicd protocol behaviour (run_test / testsuite run dispatch, progress
//! polling, result and log endpoints, error-body shapes, test-name resolution)
//! ported from @servicenow/sdk-api 4.14.0, MIT.

use std::sync::LazyLock;
use std::time::Duration;

use super::pickers::is_sys_id;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};

use super::ServiceNowClient;
use super::error::ClientError;
use super::http::{Method, form_encode};

pub const DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 45.0;
pub const MAX_WAIT_TIMEOUT_SECONDS: f64 = 600.0;
pub const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 2.0;
pub const MIN_POLL_INTERVAL_SECONDS: f64 = 1.0;
pub const MAX_POLL_INTERVAL_SECONDS: f64 = 30.0;
pub const DEFAULT_LOG_LIMIT: i64 = 100;
pub const MAX_LOG_LIMIT: i64 = 1000;
pub const MAX_LOG_PATTERN_LENGTH: usize = 256;
pub const MAX_SUITE_TEST_ROWS: usize = 200;
/// `bulk-limits.js` `MAX_NAME_LENGTH`.
pub const MAX_NAME_LENGTH: usize = 255;
const MAX_OPTION_LENGTH: usize = 100;
const MAX_OUTPUT_FIELD_CHARS: usize = 2000;
const MAX_ERROR_DETAIL_CHARS: usize = 500;
const MAX_LOG_PAYLOAD_CHARS: usize = 256 * 1024;

const CICD_PLUGIN_ID: &str = "com.glide.continuousdelivery";
const CICD_ROLE: &str = "sn_cicd.sys_ci_automation";

const BROWSER_NAMES: [&str; 6] = ["any", "chrome", "firefox", "edge", "ie", "safari"];
const RESULT_TYPES: [&str; 2] = ["test", "suite"];
const RUN_PATHS: [&str; 2] = ["/api/sn_cicd/tests/run_test", "/api/sn_cicd/testsuite/run"];

pub const ATF_TOOL_NAMES: [&str; 5] = [
    "SN-Run-ATF-Test",
    "SN-Run-ATF-Suite",
    "SN-Get-CICD-Progress",
    "SN-Get-ATF-Results",
    "SN-Get-ATF-Test-Logs",
];

pub fn is_atf_tool(name: &str) -> bool {
    ATF_TOOL_NAMES.contains(&name)
}

fn static_regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("static regex {pattern}: {e}"))
}

static UNKNOWN_API_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    static_regex(
        r"(?i)does not represent any resource|no such api|api (?:was )?not found|invalid api|not a valid (?:api|resource)",
    )
});
// e.g. "Start ATF test Failed with the given test ID" when sn_atf.runner.enabled=false.
static START_FAILED_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)\bstart\b.*\bfail"));
static NO_LOGS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)^\s*No logs found for test result\b"));

fn matches(re: &Regex, text: &str) -> bool {
    re.is_match(text)
}

/// Compile budget for validating a user log pattern (it is never matched
/// locally, only forwarded as `sysparm_regex`).
const LOG_PATTERN_SIZE_LIMIT: usize = 1 << 20;

/// Constructs the linear `regex` engine cannot bound (or ECMAScript lacks),
/// rejected before anything is compiled: backreferences, named
/// back/subroutine references, recursion and lookaround.
fn unsupported_construct(pattern: &str) -> Option<&'static str> {
    let mut chars = pattern.chars();
    let mut prev_open = String::new();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('1'..='9') => return Some("backreferences are not supported"),
                Some('g' | 'k') => {
                    return Some("named backreferences and subroutine calls are not supported");
                }
                _ => {}
            }
            prev_open.clear();
            continue;
        }
        if c == '(' {
            prev_open = String::from("(");
            continue;
        }
        if !prev_open.is_empty() {
            prev_open.push(c);
            match prev_open.as_str() {
                "(?" | "(?P" | "(?<" => continue,
                "(?=" | "(?!" | "(?<=" | "(?<!" => {
                    return Some("lookaround assertions are not supported");
                }
                "(?P>" | "(?R" | "(?&" => return Some("recursion is not supported"),
                _ => {}
            }
            prev_open.clear();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn wait_properties() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("wait".into(), json!({
        "type": "boolean",
        "description": "If true (default), poll the sn_cicd progress tracker until the run finishes or timeout_seconds elapses, then return pass/fail results. If false, return the progress_id immediately.",
        "default": true
    }));
    m.insert("timeout_seconds".into(), timeout_property());
    m.insert("poll_interval_seconds".into(), poll_interval_property());
    m
}

fn timeout_property() -> Value {
    json!({
        "type": "number",
        "description": format!("Maximum seconds to wait when wait is true (default {}, max {}). On timeout the run keeps going on the instance; the response carries progress_id for SN-Get-CICD-Progress. Many MCP clients cancel requests after about 60 seconds.", js_num(DEFAULT_WAIT_TIMEOUT_SECONDS), js_num(MAX_WAIT_TIMEOUT_SECONDS)),
        "minimum": 1,
        "maximum": 600,
        "default": 45
    })
}

fn poll_interval_property() -> Value {
    json!({
        "type": "number",
        "description": format!("Seconds between progress polls (default {}, {}-{}).", js_num(DEFAULT_POLL_INTERVAL_SECONDS), js_num(MIN_POLL_INTERVAL_SECONDS), js_num(MAX_POLL_INTERVAL_SECONDS)),
        "minimum": 1,
        "maximum": 30,
        "default": 2
    })
}

fn sys_id_schema(description: &str) -> Value {
    json!({"type": "string", "pattern": "^[0-9a-f]{32}$", "description": description})
}

fn with_wait(mut properties: Map<String, Value>) -> Value {
    properties.extend(wait_properties());
    Value::Object(properties)
}

fn props(entries: Vec<(&str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}

/// `createAtfToolDefinitions`: fresh definitions per call.
pub fn atf_tool_definitions() -> Vec<Value> {
    let test_props = props(vec![
        (
            "test_sys_id",
            sys_id_schema(
                "sys_id of the ATF test (sys_atf_test). Mutually exclusive with test_name.",
            ),
        ),
        (
            "test_name",
            json!({"type": "string", "maxLength": MAX_NAME_LENGTH, "description": "Exact name of the ATF test; resolved to a sys_id first. Fails when zero or several tests share the name. Mutually exclusive with test_sys_id."}),
        ),
        (
            "run_in_cloud",
            json!({"type": "boolean", "description": "Run UI steps on the ATF Cloud Runner instead of a scheduled client test runner."}),
        ),
        (
            "capture_node_logs",
            json!({"type": "boolean", "description": "Capture node (browser) logs during the run."}),
        ),
    ]);
    let suite_props = props(vec![
        (
            "test_suite_sys_id",
            sys_id_schema(
                "sys_id of the test suite (sys_atf_test_suite). Mutually exclusive with test_suite_name.",
            ),
        ),
        (
            "test_suite_name",
            json!({"type": "string", "maxLength": MAX_NAME_LENGTH, "description": "Name of the test suite; the instance resolves it. Mutually exclusive with test_suite_sys_id."}),
        ),
        (
            "browser_name",
            json!({"type": "string", "enum": BROWSER_NAMES, "description": "Browser to run UI tests in."}),
        ),
        (
            "browser_version",
            json!({"type": "string", "maxLength": MAX_OPTION_LENGTH, "description": "Browser version to run UI tests in."}),
        ),
        (
            "os_name",
            json!({"type": "string", "maxLength": MAX_OPTION_LENGTH, "description": "Operating system to run UI tests on."}),
        ),
        (
            "os_version",
            json!({"type": "string", "maxLength": MAX_OPTION_LENGTH, "description": "Operating system version to run UI tests on."}),
        ),
        (
            "run_in_cloud",
            json!({"type": "boolean", "description": "Run the suite on the ATF Cloud Runner instead of a manual or scheduled client test runner."}),
        ),
        (
            "is_performance_run",
            json!({"type": "boolean", "description": "Run the suite as a performance test instead of a functional test."}),
        ),
    ]);
    vec![
        json!({
            "name": "SN-Run-ATF-Test",
            "description": format!("Run one Automated Test Framework test on the instance through the CI/CD REST API (POST /api/sn_cicd/tests/run_test) and, by default, wait for its pass/fail result. Provide exactly one of test_sys_id or test_name (a name must match exactly one sys_atf_test record). This EXECUTES the test, which may create, change, or roll back data. Requires the CI/CD REST API plugin ({CICD_PLUGIN_ID}) and the {CICD_ROLE} or admin role."),
            "inputSchema": {"type": "object", "properties": with_wait(test_props)}
        }),
        json!({
            "name": "SN-Run-ATF-Suite",
            "description": format!("Run an Automated Test Framework test suite through the CI/CD REST API (POST /api/sn_cicd/testsuite/run) and, by default, wait for pass/fail/error/skip counts and per-test status. Provide exactly one of test_suite_sys_id or test_suite_name. This EXECUTES every test in the suite, which may create, change, or roll back data. Requires the CI/CD REST API plugin ({CICD_PLUGIN_ID}) and the {CICD_ROLE} or admin role."),
            "inputSchema": {"type": "object", "properties": with_wait(suite_props)}
        }),
        json!({
            "name": "SN-Get-CICD-Progress",
            "description": "Read a CI/CD REST API progress tracker (GET /api/sn_cicd/progress/{progress_id}), such as the progress_id returned by SN-Run-ATF-Test or SN-Run-ATF-Suite. Returns state (pending, running, successful, failed, canceled), percent complete, messages, and the results id once finished. Read-only: it never starts work. Set wait to keep polling until the tracker finishes.",
            "inputSchema": {
                "type": "object",
                "properties": Value::Object(props(vec![
                    ("progress_id", sys_id_schema("Progress tracker sys_id returned by a run tool.")),
                    ("wait", json!({"type": "boolean", "description": "If true, poll until the tracker finishes or timeout_seconds elapses. Default false (single read).", "default": false})),
                    ("timeout_seconds", timeout_property()),
                    ("poll_interval_seconds", poll_interval_property()),
                ])),
                "required": ["progress_id"]
            }
        }),
        json!({
            "name": "SN-Get-ATF-Results",
            "description": "Read an ATF run result through the CI/CD REST API. result_type \"test\" reads GET /api/sn_cicd/tests/test/results/{result_id} (status, output, links). result_type \"suite\" reads GET /api/sn_cicd/testsuite/results/{result_id} (pass/fail/error/skip counts, child suites, links) plus per-test status from sys_atf_test_result. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": Value::Object(props(vec![
                    ("result_id", sys_id_schema("Result sys_id: results.id from a run tool or SN-Get-CICD-Progress.")),
                    ("result_type", json!({"type": "string", "enum": RESULT_TYPES, "description": "\"test\" for a single-test result (sys_atf_test_result), \"suite\" for a test suite result (sys_atf_test_suite_result)."})),
                ])),
                "required": ["result_id", "result_type"]
            }
        }),
        json!({
            "name": "SN-Get-ATF-Test-Logs",
            "description": "Read the logs captured for one ATF test result (GET /api/sn_cicd/tests/test/results/{result_id}/logs). An optional regular expression filters log lines on the instance, which always matches case-insensitively. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": Value::Object(props(vec![
                    ("result_id", sys_id_schema("sys_id of the ATF test result (sys_atf_test_result).")),
                    ("pattern", json!({"type": "string", "maxLength": MAX_LOG_PATTERN_LENGTH, "description": format!("Optional regular expression (at most {MAX_LOG_PATTERN_LENGTH} characters); only matching lines are returned. Invalid expressions and nested quantifiers such as (a+)+ are rejected.")})),
                    ("limit", json!({"type": "integer", "description": format!("Maximum log lines to return (default {DEFAULT_LOG_LIMIT}, max {MAX_LOG_LIMIT})."), "minimum": 1, "maximum": MAX_LOG_LIMIT, "default": DEFAULT_LOG_LIMIT})),
                ])),
                "required": ["result_id"]
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// JS value helpers
// ---------------------------------------------------------------------------

/// `String(n)` for a finite number.
fn js_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `String(value)` / template-literal interpolation.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), js_num),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(Some(v))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// A finite JS number as JSON (integral values serialize without `.0`).
fn js_number_value(n: f64) -> Value {
    if !n.is_finite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// `Number(value ?? 0)`.
fn js_to_number(value: Option<&Value>) -> Value {
    let n = match value {
        None | Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        Some(_) => f64::NAN,
    };
    js_number_value(n)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that is at most `max` UTF-16 units.
fn utf16_prefix(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

/// `truncate` on a string.
fn truncate_str(value: &str, max: usize) -> String {
    if utf16_len(value) > max {
        format!("{}… (truncated)", utf16_prefix(value, max))
    } else {
        value.to_owned()
    }
}

/// `truncate` on any value: only strings are shortened.
fn truncate_value(value: Option<&Value>, max: usize) -> Option<Value> {
    match value? {
        Value::String(s) => Some(Value::String(truncate_str(s, max))),
        other => Some(other.clone()),
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `present`: not undefined, null or the empty string.
fn present(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null)) && value != Some(&Value::String(String::new()))
}

/// Object builder that drops `undefined` (absent) entries, like `compact`.
#[derive(Default)]
struct Obj(Map<String, Value>);

impl Obj {
    fn set(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_owned(), value.into());
        self
    }
    fn opt(mut self, key: &str, value: Option<Value>) -> Self {
        if let Some(v) = value {
            self.0.insert(key.to_owned(), v);
        }
        self
    }
    /// Copies `source[key]` when it is defined (null is kept).
    fn copy(self, key: &str, source: &Value) -> Self {
        let v = source.get(key).cloned();
        self.opt(key, v)
    }
    fn build(self) -> Value {
        Value::Object(self.0)
    }
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

fn input_error(message: impl Into<String>) -> ClientError {
    ClientError::plain(message)
}

/// `bulk-limits.js` failures are `BulkInputError`s.
fn bulk_error(message: impl Into<String>) -> ClientError {
    ClientError::new("BulkInputError", message, None)
}

fn assert_sys_id(value: Option<&Value>, field: &str) -> Result<String, ClientError> {
    match value {
        Some(Value::String(s))
            if s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(s.clone())
        }
        _ => Err(bulk_error(format!(
            "{field} must be a 32-character lowercase hexadecimal sys_id"
        ))),
    }
}

fn assert_name(value: Option<&Value>, field: &str) -> Result<String, ClientError> {
    match value {
        Some(Value::String(s)) if !s.is_empty() && utf16_len(s) <= MAX_NAME_LENGTH => Ok(s.clone()),
        _ => Err(bulk_error(format!(
            "{field} must be a non-empty string of at most {MAX_NAME_LENGTH} characters"
        ))),
    }
}

fn assert_exactly_one_of(args: &Value, a: &str, b: &str) -> Result<(), ClientError> {
    let has_a = present(args.get(a));
    let has_b = present(args.get(b));
    if !has_a && !has_b {
        return Err(input_error(format!("Provide either {a} or {b}")));
    }
    if has_a && has_b {
        return Err(input_error(format!(
            "{a} and {b} are mutually exclusive; provide only one"
        )));
    }
    Ok(())
}

fn optional_boolean(args: &Value, field: &str) -> Result<Option<bool>, ClientError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(input_error(format!("{field} must be a boolean"))),
    }
}

fn optional_string(args: &Value, field: &str) -> Result<Option<String>, ClientError> {
    let value = args.get(field);
    if !present(value) {
        return Ok(None);
    }
    match value {
        Some(Value::String(s)) if utf16_len(s) <= MAX_OPTION_LENGTH => Ok(Some(s.clone())),
        _ => Err(input_error(format!(
            "{field} must be a string of at most {MAX_OPTION_LENGTH} characters"
        ))),
    }
}

fn bounded_number(
    args: &Value,
    field: &str,
    min: f64,
    max: f64,
    fallback: f64,
) -> Result<f64, ClientError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(fallback),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(v) if v.is_finite() && v >= min && v <= max => Ok(v),
            _ => Err(range_error(field, min, max)),
        },
        Some(_) => Err(range_error(field, min, max)),
    }
}

fn range_error(field: &str, min: f64, max: f64) -> ClientError {
    input_error(format!(
        "{field} must be a number from {} to {}",
        js_num(min),
        js_num(max)
    ))
}

#[derive(Clone, Copy, Debug)]
struct WaitOptions {
    wait: bool,
    timeout_ms: f64,
    interval_ms: f64,
}

fn wait_options(args: &Value, default_wait: bool) -> Result<WaitOptions, ClientError> {
    Ok(WaitOptions {
        wait: optional_boolean(args, "wait")?.unwrap_or(default_wait),
        timeout_ms: bounded_number(
            args,
            "timeout_seconds",
            1.0,
            MAX_WAIT_TIMEOUT_SECONDS,
            DEFAULT_WAIT_TIMEOUT_SECONDS,
        )? * 1000.0,
        interval_ms: bounded_number(
            args,
            "poll_interval_seconds",
            MIN_POLL_INTERVAL_SECONDS,
            MAX_POLL_INTERVAL_SECONDS,
            DEFAULT_POLL_INTERVAL_SECONDS,
        )? * 1000.0,
    })
}

/// Heuristic guard against catastrophic backtracking: rejects a quantified
/// group whose body is itself quantified, e.g. `(a+)+`, `(.*)*`, `(\w+){2,}`.
fn has_nested_quantifier(pattern: &str) -> bool {
    // `.replace(/\\./g, 'x').replace(/\[[^\]]*\]/g, 'x')`
    let mut escaped = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && chars
                .peek()
                .is_some_and(|n| !matches!(n, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        {
            chars.next();
            escaped.push('x');
        } else {
            escaped.push(c);
        }
    }
    let mut src = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == '['
            && let Some(end) = escaped[i + 1..].iter().position(|c| *c == ']')
        {
            src.push('x');
            i += end + 2;
            continue;
        }
        src.push(escaped[i]);
        i += 1;
    }
    let mut stack: Vec<bool> = Vec::new();
    for (i, ch) in src.iter().enumerate() {
        match ch {
            '(' => stack.push(false),
            ')' => {
                let inner = stack.pop().unwrap_or(false);
                let group = matches!(src.get(i + 1), Some('*' | '+' | '{'));
                if inner && group {
                    return true;
                }
                if let Some(top) = stack.last_mut()
                    && (inner || group)
                {
                    *top = true;
                }
            }
            '*' | '+' | '{' => {
                if let Some(top) = stack.last_mut() {
                    *top = true;
                }
            }
            _ => {}
        }
    }
    false
}

/// `validateLogPattern`. Returns the pattern when present.
pub fn validate_log_pattern(pattern: Option<&Value>) -> Result<Option<String>, ClientError> {
    if !present(pattern) {
        return Ok(None);
    }
    let Some(Value::String(pattern)) = pattern else {
        return Err(input_error("pattern must be a string"));
    };
    let len = utf16_len(pattern);
    if len > MAX_LOG_PATTERN_LENGTH {
        return Err(input_error(format!(
            "pattern must be at most {MAX_LOG_PATTERN_LENGTH} characters (received {len})"
        )));
    }
    if pattern.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err(input_error("pattern must not contain control characters"));
    }
    if has_nested_quantifier(pattern) {
        return Err(input_error(
            "pattern contains a nested quantifier such as (a+)+, which can backtrack catastrophically; simplify the expression",
        ));
    }
    if let Some(reason) = unsupported_construct(pattern) {
        return Err(input_error(format!(
            "pattern is not a valid regular expression: {reason}"
        )));
    }
    if let Err(e) = RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(LOG_PATTERN_SIZE_LIMIT)
        .dfa_size_limit(LOG_PATTERN_SIZE_LIMIT)
        .nest_limit(64)
        .build()
    {
        return Err(input_error(format!(
            "pattern is not a valid regular expression: Invalid regular expression: /{pattern}/i: {}",
            js_regex_reason(&e)
        )));
    }
    Ok(Some(pattern.clone()))
}

/// The V8 `SyntaxError` reason for a pattern the `regex` crate rejects, so the
/// text matches 6.2.0 (`new RegExp(pattern, 'i')`). Unmapped errors keep the
/// crate's own description.
fn js_regex_reason(e: &regex::Error) -> String {
    let text = e.to_string();
    const MAP: [(&str, &str); 5] = [
        ("unclosed group", "Unterminated group"),
        ("unopened group", "Unmatched ')'"),
        ("unclosed character class", "Unterminated character class"),
        (
            "repetition operator missing expression",
            "Nothing to repeat",
        ),
        ("incomplete escape sequence", "\\ at end of pattern"),
    ];
    MAP.iter()
        .find(|(rust, _)| text.contains(rust))
        .map_or(text.clone(), |(_, js)| (*js).to_owned())
}

// ---------------------------------------------------------------------------
// sn_cicd HTTP + error mapping
// ---------------------------------------------------------------------------

/// `sn_cicd` error bodies vary: `{error: {message, detail}}` from the platform
/// REST layer, `{result: {error}}` or `{result: {status_message}}` from
/// sn_cicd operations.
fn extract_error_message(body: Option<&Value>) -> Option<String> {
    let body = body?;
    if let Value::String(s) = body {
        let mut stripped = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(open) = rest.find('<') {
            match rest[open..].find('>') {
                Some(close) => {
                    stripped.push_str(&rest[..open]);
                    stripped.push(' ');
                    rest = &rest[open + close + 1..];
                }
                None => break,
            }
        }
        stripped.push_str(rest);
        let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
        return non_empty_string(Some(&Value::String(collapsed)));
    }
    let generic = body.get("error");
    let result = body.get("result");
    non_empty_string(generic.and_then(|g| g.get("message")))
        .or_else(|| non_empty_string(generic.filter(|g| g.is_string())))
        .or_else(|| non_empty_string(result.and_then(|r| r.get("error"))))
        .or_else(|| non_empty_string(result.and_then(|r| r.get("status_message"))))
}

fn cicd_error(
    message: String,
    code: &str,
    status: Option<u16>,
    detail: Option<String>,
) -> ClientError {
    let mut e = ClientError::new("CicdApiError", message, Some(code)).with_status(status);
    e.reason = detail.map(String::into_boxed_str);
    e
}

/// `CicdApiError.detail`.
pub fn cicd_detail(error: &ClientError) -> Option<&str> {
    (error.name == "CicdApiError")
        .then_some(error.reason.as_deref())
        .flatten()
}

/// `toCicdError`.
fn to_cicd_error(error: ClientError, path: &str) -> ClientError {
    // Stale errors keep their code: callers tell "never sent" from "sent,
    // outcome unknown" by it.
    if error.name == "CicdApiError" || error.is_stale() {
        return error;
    }
    let status = error.status;
    let detail = extract_error_message(error.response_data.as_deref())
        .map(|d| truncate_str(&d, MAX_ERROR_DETAIL_CHARS));
    let said = detail
        .as_ref()
        .map_or(String::new(), |d| format!(" ServiceNow said: {d}"));

    match status {
        Some(401) => {
            return cicd_error(
                format!(
                    "ServiceNow rejected the credentials (401) for {path}. Check the instance credentials.{said}"
                ),
                "CICD_UNAUTHORIZED",
                status,
                None,
            );
        }
        Some(403) => {
            return cicd_error(
                format!(
                    "Access denied (403) to the CI/CD REST API at {path}. The ServiceNow user needs the {CICD_ROLE} role (or admin). Grant the role to this integration user, then retry.{said}"
                ),
                "CICD_ROLE_MISSING",
                status,
                None,
            );
        }
        _ => {}
    }
    if status == Some(400)
        && RUN_PATHS.contains(&path)
        && let Some(d) = detail.as_deref()
        && matches(&START_FAILED_PATTERN, d)
    {
        let stop = if said.is_empty() || said.ends_with(['.', '!', '?']) {
            ""
        } else {
            "."
        };
        return cicd_error(
            format!(
                "The instance refused to start the run (400 for {path}).{said}{stop} ATF test execution may be disabled: check the sys_properties `sn_atf.runner.enabled` (must be true). The same message can also mean the test or suite id cannot be started."
            ),
            "ATF_RUNNER_DISABLED",
            status,
            detail,
        );
    }
    if let Some(s @ (400 | 404)) = status
        && detail
            .as_deref()
            .is_none_or(|d| matches(&UNKNOWN_API_PATTERN, d))
    {
        return cicd_error(
            format!(
                "The CI/CD REST API is not available on this instance ({s} for {path}). Activate the \"CI/CD REST API\" plugin ({CICD_PLUGIN_ID}) under System Definition > Plugins, then retry.{said}"
            ),
            "CICD_PLUGIN_INACTIVE",
            status,
            None,
        );
    }
    if let Some(s) = status {
        let tail = if said.is_empty() {
            format!(" {}", truncate_str(&error.message, MAX_ERROR_DETAIL_CHARS))
        } else {
            said
        };
        return cicd_error(
            format!("sn_cicd request to {path} failed ({s}).{tail}"),
            "CICD_REQUEST_FAILED",
            status,
            detail,
        );
    }
    let message = if error.message.is_empty() {
        "network error"
    } else {
        error.message.as_str()
    };
    cicd_error(
        format!(
            "sn_cicd request to {path} failed: {}",
            truncate_str(message, MAX_ERROR_DETAIL_CHARS)
        ),
        "CICD_REQUEST_FAILED",
        None,
        None,
    )
}

/// `withQuery`: `URLSearchParams` in insertion order.
fn with_query(path: &str, fields: &[(&str, Option<String>)]) -> String {
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .filter_map(|(k, v)| v.as_deref().map(|v| (*k, v)))
        .collect();
    let qs = form_encode(&pairs);
    if qs.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{qs}")
    }
}

/// Unwraps the sn_cicd `{result}` envelope; maps failures to `CicdApiError`.
async fn cicd_request(
    client: &ServiceNowClient,
    method: Method,
    path: &str,
    query: &[(&str, Option<String>)],
) -> Result<Value, ClientError> {
    let (status, body) = client
        .rest_with_status(method, &with_query(path, query), None)
        .await
        .map_err(|e| to_cicd_error(e, path))?;
    match body {
        Value::Object(mut m) => Ok(match m.remove("result") {
            Some(r) if !r.is_null() => r,
            removed => {
                if let Some(r) = removed {
                    m.insert("result".into(), r);
                }
                Value::Object(m)
            }
        }),
        array @ Value::Array(_) => Ok(array),
        // An HTML login/landing page instead of JSON means the API route is missing.
        _ => Err(cicd_error(
            format!(
                "The CI/CD REST API at {path} returned a non-JSON response; the \"CI/CD REST API\" plugin ({CICD_PLUGIN_ID}) is probably not active. Activate it under System Definition > Plugins, then retry."
            ),
            "CICD_PLUGIN_INACTIVE",
            Some(status),
            None,
        )),
    }
}

// ---------------------------------------------------------------------------
// Progress tracking
// ---------------------------------------------------------------------------

fn state_of(status: Option<&Value>) -> String {
    let key = js_string(status);
    match key.as_str() {
        "0" => "pending".into(),
        "1" => "running".into(),
        "2" => "successful".into(),
        "3" => "failed".into(),
        "4" => "canceled".into(),
        _ => format!("unknown:{key}"),
    }
}

fn link_of(links: Option<&Value>, key: &str) -> Option<Value> {
    let link = links?.get(key)?;
    if !(link.is_object() || link.is_array()) {
        return None;
    }
    let mut out = Map::new();
    for field in ["id", "url"] {
        if let Some(v @ Value::String(_)) = link.get(field) {
            out.insert(field.into(), v.clone());
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn snapshot_of(progress_id: &str, result: &Value) -> Value {
    let state = state_of(result.get("status"));
    let finished = matches!(state.as_str(), "successful" | "failed" | "canceled");
    Obj::default()
        .set("progress_id", progress_id)
        .set("state", state)
        .copy("status", result)
        .copy("status_label", result)
        .copy("status_message", result)
        .copy("status_detail", result)
        .opt(
            "error",
            non_empty_string(result.get("error")).map(Value::String),
        )
        .copy("percent_complete", result)
        .set("finished", finished)
        .opt("results", link_of(result.get("links"), "results"))
        .build()
}

async fn fetch_progress(
    client: &ServiceNowClient,
    progress_id: &str,
) -> Result<Value, ClientError> {
    let result = cicd_request(
        client,
        Method::Get,
        &format!("/api/sn_cicd/progress/{progress_id}"),
        &[],
    )
    .await?;
    Ok(snapshot_of(progress_id, &result))
}

fn snapshot_state(snapshot: &Value) -> &str {
    snapshot.get("state").and_then(Value::as_str).unwrap_or("")
}

/// Polls until terminal or the deadline. Never sleeps past the deadline.
/// Returns `(snapshot, timed_out)`.
async fn poll_progress(
    client: &ServiceNowClient,
    progress_id: &str,
    options: WaitOptions,
) -> Result<(Value, bool), ClientError> {
    #[allow(clippy::cast_precision_loss)]
    let now = || client.now_ms() as f64;
    let deadline = now() + options.timeout_ms;
    // Every poll targets the instance the run was started on: a
    // `set_instance` mid-wait fails the call instead of retargeting it.
    let generation = client.generation();
    let mut attempt: u64 = 0;
    let mut last = snapshot_of(progress_id, &json!({}));
    loop {
        // The run was already dispatched: a moved generation leaves its
        // outcome unknown, never "not sent".
        if client.generation() != generation {
            return Err(ClientError::stale_after_send());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let budget = Duration::from_millis((deadline - now()).max(0.0).round() as u64);
        let Ok(fetched) = tokio::time::timeout(budget, fetch_progress(client, progress_id)).await
        else {
            return Ok((last, true));
        };
        let snapshot = fetched?;
        if client.generation() != generation {
            return Err(ClientError::stale_after_send());
        }
        last = snapshot.clone();
        attempt += 1;
        let percent = snapshot
            .get("percent_complete")
            .map_or(String::new(), |p| format!(" ({}%)", js_string(Some(p))));
        let message = snapshot
            .get("status_message")
            .filter(|m| crate::client::pickers::truthy(m))
            .map_or(String::new(), |m| format!(" {}", js_string(Some(m))));
        client
            .notify_progress(
                &format!(
                    "sn_cicd {progress_id}: {}{percent}{message}",
                    snapshot_state(&snapshot)
                ),
                Some(attempt),
                None,
            )
            .await;
        if snapshot.get("finished") == Some(&Value::Bool(true)) {
            return Ok((snapshot, false));
        }
        let remaining = deadline - now();
        if remaining <= 0.0 {
            return Ok((snapshot, true));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        client
            .sleep_ms(options.interval_ms.min(remaining).round() as u64)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

fn instance_base(client: &ServiceNowClient) -> String {
    client.instance_url().trim_end_matches('/').to_owned()
}

fn record_url(client: &ServiceNowClient, table: &str, sys_id: &str) -> String {
    format!("{}/{table}.do?sys_id={sys_id}", instance_base(client))
}

fn api_url(client: &ServiceNowClient, path: &str) -> String {
    format!("{}{path}", instance_base(client))
}

/// `a ?? b` on object members.
fn coalesce<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    a.filter(|v| !v.is_null()).or(b)
}

async fn get_test_result(client: &ServiceNowClient, result_id: &str) -> Result<Value, ClientError> {
    let path = format!("/api/sn_cicd/tests/test/results/{result_id}");
    let result = cicd_request(client, Method::Get, &path, &[]).await?;
    Ok(Obj::default()
        .set("result_type", "test")
        .set("result_id", result_id)
        .copy("test_name", &result)
        .copy("test_status", &result)
        .opt(
            "output",
            truncate_value(result.get("output"), MAX_OUTPUT_FIELD_CHARS),
        )
        .copy("start_time", &result)
        .copy("end_time", &result)
        .opt(
            "duration",
            coalesce(result.get("test_duration"), result.get("run_time")).cloned(),
        )
        .set(
            "links",
            json!({
                "api": api_url(client, &path),
                "record": record_url(client, "sys_atf_test_result", result_id)
            }),
        )
        .build())
}

fn display_value(value: Option<&Value>) -> Option<&Value> {
    match value {
        Some(Value::Object(m)) if m.contains_key("display_value") => m.get("display_value"),
        other => other,
    }
}

/// Best-effort per-test rows for a suite result; never fails the call.
async fn get_suite_test_rows(
    client: &ServiceNowClient,
    suite_result_id: &str,
) -> Vec<(&'static str, Value)> {
    if !is_sys_id(suite_result_id) {
        return Vec::new();
    }
    let path = "/api/now/table/sys_atf_test_result";
    let url = with_query(
        path,
        &[
            (
                "sysparm_query",
                Some(format!("parent={suite_result_id}^ORDERBYstart_time")),
            ),
            (
                "sysparm_fields",
                Some("sys_id,test,status,output,start_time,end_time,run_time".into()),
            ),
            ("sysparm_display_value", Some("true".into())),
            ("sysparm_exclude_reference_link", Some("true".into())),
            ("sysparm_limit", Some((MAX_SUITE_TEST_ROWS + 1).to_string())),
        ],
    );
    match client.rest(Method::Get, &url, None).await {
        Ok(body) => {
            let records = match body.get("result") {
                Some(Value::Array(rows)) => rows.as_slice(),
                _ => &[],
            };
            let tests: Vec<Value> = records
                .iter()
                .take(MAX_SUITE_TEST_ROWS)
                .map(|row| {
                    let sys_id = display_value(row.get("sys_id"));
                    Obj::default()
                        .opt("test_result_sys_id", sys_id.cloned())
                        .opt("test_name", display_value(row.get("test")).cloned())
                        .opt("status", display_value(row.get("status")).cloned())
                        .opt(
                            "output",
                            truncate_value(
                                display_value(row.get("output")),
                                MAX_OUTPUT_FIELD_CHARS,
                            ),
                        )
                        .opt("start_time", display_value(row.get("start_time")).cloned())
                        .opt("end_time", display_value(row.get("end_time")).cloned())
                        .opt("run_time", display_value(row.get("run_time")).cloned())
                        .set(
                            "link",
                            record_url(client, "sys_atf_test_result", &js_string(sys_id)),
                        )
                        .build()
                })
                .collect();
            let mut out = vec![("tests", Value::Array(tests))];
            if records.len() > MAX_SUITE_TEST_ROWS {
                out.push(("tests_truncated", Value::Bool(true)));
            }
            out
        }
        Err(e) => vec![("tests_error", Value::String(to_cicd_error(e, path).message))],
    }
}

async fn get_suite_result(
    client: &ServiceNowClient,
    result_id: &str,
) -> Result<Value, ClientError> {
    let path = format!("/api/sn_cicd/testsuite/results/{result_id}");
    let result = cicd_request(client, Method::Get, &path, &[]).await?;
    let counts = json!({
        "passed": js_to_number(result.get("rolledup_test_success_count")),
        "failed": js_to_number(result.get("rolledup_test_failure_count")),
        "errored": js_to_number(result.get("rolledup_test_error_count")),
        "skipped": js_to_number(result.get("rolledup_test_skip_count")),
    });
    let rows = get_suite_test_rows(client, result_id).await;
    let children = result
        .get("child_suite_results")
        .filter(|c| c.as_array().is_some_and(|a| !a.is_empty()))
        .cloned();
    let mut obj = Obj::default()
        .set("result_type", "suite")
        .set("result_id", result_id)
        .copy("test_suite_name", &result)
        .copy("test_suite_status", &result)
        .opt("duration", result.get("test_suite_duration").cloned())
        .set("counts", counts)
        .opt("child_suite_results", children);
    for (key, value) in rows {
        obj = obj.set(key, value);
    }
    Ok(obj
        .set(
            "links",
            json!({
                "api": api_url(client, &path),
                "record": record_url(client, "sys_atf_test_suite_result", result_id)
            }),
        )
        .build())
}

async fn get_result(
    client: &ServiceNowClient,
    result_id: &str,
    result_type: &str,
) -> Result<Value, ClientError> {
    if result_type == "suite" {
        get_suite_result(client, result_id).await
    } else {
        get_test_result(client, result_id).await
    }
}

// ---------------------------------------------------------------------------
// Run orchestration
// ---------------------------------------------------------------------------

/// An MCP tool result: `JSON.stringify(payload, null, 2)` as text.
fn json_response(payload: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(payload).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

async fn resolve_test_sys_id(
    client: &ServiceNowClient,
    test_name: &str,
) -> Result<String, ClientError> {
    let path = "/api/now/table/sys_atf_test";
    let shown = truncate_str(test_name, 100);
    let url = with_query(
        path,
        &[
            (
                "sysparm_query",
                Some(format!("name={}", test_name.replace('^', "^^"))),
            ),
            ("sysparm_fields", Some("sys_id,name".into())),
            ("sysparm_limit", Some("2".into())),
        ],
    );
    let body = client.rest(Method::Get, &url, None).await.map_err(|e| {
        input_error(format!(
            "Failed to resolve ATF test name '{shown}': {}",
            to_cicd_error(e, path).message
        ))
    })?;
    let records = match body.get("result") {
        Some(Value::Array(rows)) => rows.as_slice(),
        _ => &[],
    };
    match records {
        [] => Err(input_error(format!(
            "No ATF test found with name '{shown}'"
        ))),
        [row] => match row.get("sys_id") {
            Some(Value::String(id)) if !id.is_empty() => Ok(id.clone()),
            _ => Err(input_error(format!(
                "ATF test '{shown}' did not return a sys_id"
            ))),
        },
        _ => Err(input_error(format!(
            "Multiple ATF tests found with name '{shown}'; use test_sys_id instead"
        ))),
    }
}

struct Dispatch {
    path: &'static str,
    query: Vec<(&'static str, Option<String>)>,
    result_type: &'static str,
    label: &'static str,
    target: Value,
}

async fn dispatch_and_maybe_wait(
    client: &ServiceNowClient,
    d: Dispatch,
    options: WaitOptions,
) -> Result<Value, ClientError> {
    let dispatch_result = match cicd_request(client, Method::Post, d.path, &d.query).await {
        Ok(result) => result,
        // The POST left before the instance moved: the run may have started.
        Err(e) if e.is_stale_after_send() => {
            return Ok(json_response(
                &json!({
                    "status": "unknown",
                    "target": d.target,
                    "message": format!("The {} request was sent but the instance changed before its response was read: {}", d.label, e.message),
                    "next_step": format!("Check the {} on the original instance before re-running it.", d.label)
                }),
                true,
            ));
        }
        Err(e) => return Err(e),
    };
    let raw_progress_id = link_of(dispatch_result.get("links"), "progress")
        .and_then(|l| l.get("id").and_then(Value::as_str).map(str::to_owned));
    // Server-returned ids go into URL paths: only well-formed sys_ids.
    let progress_id = raw_progress_id.clone().filter(|id| is_sys_id(id));
    let result_type = d.result_type;
    let label = d.label;

    if !options.wait {
        let payload = Obj::default()
            .set("status", "started")
            .set("target", d.target)
            .opt("progress_id", progress_id.clone().map(Value::String))
            .copy("status_message", &dispatch_result)
            .opt(
                "next_step",
                progress_id.as_ref().map(|id| {
                    Value::String(format!("Call SN-Get-CICD-Progress with progress_id {id} (wait: true to block), then SN-Get-ATF-Results with result_type \"{result_type}\"."))
                }),
            )
            .build();
        return Ok(json_response(&payload, progress_id.is_none()));
    }

    let Some(progress_id) = progress_id else {
        let message = if raw_progress_id.is_some() {
            // The POST succeeded, so the run was dispatched; only the
            // tracker id is unusable.
            format!(
                "sn_cicd request to {} dispatched the {label} ({}) but the instance returned a malformed progress id (not a 32-character sys_id); check the run on the instance instead of re-running it",
                d.path, d.target
            )
        } else {
            format!(
                "sn_cicd request to {} returned no progress id; the {label} may not have started",
                d.path
            )
        };
        return Err(cicd_error(message, "CICD_NO_PROGRESS_ID", None, None));
    };

    let (snapshot, timed_out) = match poll_progress(client, &progress_id, options).await {
        Ok(polled) => polled,
        Err(e) => {
            let message = to_cicd_error(e, &format!("/api/sn_cicd/progress/{progress_id}")).message;
            return Ok(json_response(
                &json!({
                    "status": "unknown",
                    "target": d.target,
                    "progress_id": progress_id,
                    "message": format!("The {label} was started but its progress could not be read: {message}"),
                    "next_step": format!("Retry SN-Get-CICD-Progress with progress_id {progress_id}. Do not re-run the {label} unless the tracker shows it never ran.")
                }),
                true,
            ));
        }
    };

    if timed_out {
        let state = snapshot_state(&snapshot).to_owned();
        return Ok(json_response(
            &json!({
                "status": "timeout",
                "target": d.target,
                "progress_id": progress_id,
                "progress": snapshot,
                "message": format!("Stopped waiting after {}s; the {label} is still {state} on the instance. This is not a result.", js_num(options.timeout_ms / 1000.0)),
                "next_step": format!("Call SN-Get-CICD-Progress with progress_id {progress_id} and wait: true, then SN-Get-ATF-Results with result_type \"{result_type}\" and the returned results.id.")
            }),
            false,
        ));
    }

    let state = snapshot_state(&snapshot).to_owned();
    let successful = state == "successful";
    let results_id = snapshot
        .get("results")
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .filter(|id| is_sys_id(id))
        .map(str::to_owned);
    let mut payload = Obj::default()
        .set("status", state)
        .set("successful", successful)
        .set("target", d.target)
        .set("progress_id", progress_id)
        .set("progress", snapshot);
    // The tracker state is authoritative for pass/fail; results only add detail.
    if let Some(id) = results_id {
        payload = match get_result(client, &id, result_type).await {
            Ok(results) => payload.set("results", results),
            Err(e) => payload.set("results_error", to_cicd_error(e, "results").message),
        };
    }
    Ok(json_response(&payload.build(), !successful))
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

fn bool_query(value: Option<bool>) -> Option<String> {
    value.map(|b| b.to_string())
}

async fn run_atf_test(client: &ServiceNowClient, args: &Value) -> Result<Value, ClientError> {
    assert_exactly_one_of(args, "test_sys_id", "test_name")?;
    let run_in_cloud = optional_boolean(args, "run_in_cloud")?;
    let capture_node_logs = optional_boolean(args, "capture_node_logs")?;
    let options = wait_options(args, true)?;
    let test_sys_id = if present(args.get("test_sys_id")) {
        assert_sys_id(args.get("test_sys_id"), "test_sys_id")?
    } else {
        let name = assert_name(args.get("test_name"), "test_name")?;
        resolve_test_sys_id(client, &name).await?
    };
    let test_name = args
        .get("test_name")
        .filter(|v| crate::client::pickers::truthy(v))
        .cloned();
    dispatch_and_maybe_wait(
        client,
        Dispatch {
            path: "/api/sn_cicd/tests/run_test",
            query: vec![
                ("test_sys_id", Some(test_sys_id.clone())),
                ("run_in_cloud", bool_query(run_in_cloud)),
                ("capture_node_logs", bool_query(capture_node_logs)),
            ],
            result_type: "test",
            label: "ATF test",
            target: Obj::default()
                .set("test_sys_id", test_sys_id)
                .opt("test_name", test_name)
                .build(),
        },
        options,
    )
    .await
}

async fn run_atf_suite(client: &ServiceNowClient, args: &Value) -> Result<Value, ClientError> {
    assert_exactly_one_of(args, "test_suite_sys_id", "test_suite_name")?;
    let sys_id = if present(args.get("test_suite_sys_id")) {
        Some(assert_sys_id(
            args.get("test_suite_sys_id"),
            "test_suite_sys_id",
        )?)
    } else {
        assert_name(args.get("test_suite_name"), "test_suite_name")?;
        None
    };
    let browser_name = optional_string(args, "browser_name")?;
    if let Some(b) = &browser_name
        && !BROWSER_NAMES.contains(&b.as_str())
    {
        return Err(input_error(format!(
            "browser_name must be one of: {}",
            BROWSER_NAMES.join(", ")
        )));
    }
    let suite_name =
        present(args.get("test_suite_name")).then(|| js_string(args.get("test_suite_name")));
    let query = vec![
        ("test_suite_sys_id", sys_id.clone()),
        ("test_suite_name", suite_name.clone()),
        ("browser_name", browser_name),
        ("browser_version", optional_string(args, "browser_version")?),
        ("os_name", optional_string(args, "os_name")?),
        ("os_version", optional_string(args, "os_version")?),
        (
            "run_in_cloud",
            bool_query(optional_boolean(args, "run_in_cloud")?),
        ),
        (
            "is_performance_run",
            bool_query(optional_boolean(args, "is_performance_run")?),
        ),
    ];
    let options = wait_options(args, true)?;
    dispatch_and_maybe_wait(
        client,
        Dispatch {
            path: "/api/sn_cicd/testsuite/run",
            query,
            result_type: "suite",
            label: "ATF test suite",
            target: Obj::default()
                .opt("test_suite_sys_id", sys_id.map(Value::String))
                .opt("test_suite_name", suite_name.map(Value::String))
                .build(),
        },
        options,
    )
    .await
}

async fn get_cicd_progress(client: &ServiceNowClient, args: &Value) -> Result<Value, ClientError> {
    let progress_id = assert_sys_id(args.get("progress_id"), "progress_id")?;
    let options = wait_options(args, false)?;
    if !options.wait {
        return Ok(json_response(
            &fetch_progress(client, &progress_id).await?,
            false,
        ));
    }
    let (mut snapshot, timed_out) = poll_progress(client, &progress_id, options).await?;
    if timed_out {
        let message = format!(
            "Still {} after {}s; call again to keep waiting.",
            snapshot_state(&snapshot),
            js_num(options.timeout_ms / 1000.0)
        );
        if let Value::Object(m) = &mut snapshot {
            m.insert("timed_out".into(), Value::Bool(true));
            m.insert("message".into(), Value::String(message));
        }
    }
    Ok(json_response(&snapshot, false))
}

async fn get_atf_results(client: &ServiceNowClient, args: &Value) -> Result<Value, ClientError> {
    let result_id = assert_sys_id(args.get("result_id"), "result_id")?;
    let result_type = match args.get("result_type") {
        Some(Value::String(t)) if RESULT_TYPES.contains(&t.as_str()) => t.clone(),
        _ => {
            return Err(input_error(format!(
                "result_type must be one of: {}",
                RESULT_TYPES.join(", ")
            )));
        }
    };
    Ok(json_response(
        &get_result(client, &result_id, &result_type).await?,
        false,
    ))
}

async fn get_atf_test_logs(client: &ServiceNowClient, args: &Value) -> Result<Value, ClientError> {
    let result_id = assert_sys_id(args.get("result_id"), "result_id")?;
    let pattern = validate_log_pattern(args.get("pattern"))?;
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LOG_LIMIT,
        Some(Value::Number(n)) => match n.as_f64() {
            #[allow(clippy::cast_possible_truncation)]
            Some(v) if v.fract() == 0.0 && (1.0..=1000.0).contains(&v) => v as i64,
            _ => return Err(limit_error()),
        },
        Some(_) => return Err(limit_error()),
    };
    let path = format!("/api/sn_cicd/tests/test/results/{result_id}/logs");
    let query = [
        ("sysparm_regex", pattern.clone()),
        ("sysparm_limit", Some(limit.to_string())),
    ];
    let head = Obj::default()
        .set("result_id", result_id.as_str())
        .opt("pattern", pattern.clone().map(Value::String))
        .set("limit", limit);
    let result = match cicd_request(client, Method::Get, &path, &query).await {
        Ok(result) => result,
        Err(e) => {
            if e.status == Some(404) && matches(&NO_LOGS_PATTERN, cicd_detail(&e).unwrap_or("")) {
                return Ok(json_response(
                    &head
                        .set("logs", json!([]))
                        .set("note", "No logs were captured for this test result. Logs are captured only when the run used capture_node_logs: true.")
                        .build(),
                    false,
                ));
            }
            return Err(e);
        }
    };
    let serialized = serde_json::to_string(&result).unwrap_or_default();
    let payload = if utf16_len(&serialized) > MAX_LOG_PAYLOAD_CHARS {
        head.set("logs_truncated", true).set(
            "logs_text",
            format!("{}…", utf16_prefix(&serialized, MAX_LOG_PAYLOAD_CHARS)),
        )
    } else {
        head.set("logs", result)
    };
    Ok(json_response(&payload.build(), false))
}

fn limit_error() -> ClientError {
    input_error(format!(
        "limit must be an integer from 1 to {MAX_LOG_LIMIT}"
    ))
}

/// `handleAtfTool`: `client` is the per-request (instance-routed) client; its
/// clock drives polling.
pub async fn handle_atf_tool(
    name: &str,
    args: &Value,
    client: &ServiceNowClient,
) -> Result<Value, ClientError> {
    let empty = Value::Object(Map::new());
    let input = if args.is_null() { &empty } else { args };
    match name {
        "SN-Run-ATF-Test" => run_atf_test(client, input).await,
        "SN-Run-ATF-Suite" => run_atf_suite(client, input).await,
        "SN-Get-CICD-Progress" => get_cicd_progress(client, input).await,
        "SN-Get-ATF-Results" => get_atf_results(client, input).await,
        "SN-Get-ATF-Test-Logs" => get_atf_test_logs(client, input).await,
        _ => Err(input_error(format!("Unknown ATF tool: {name}"))),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn nested_quantifier_heuristic() {
        for bad in ["(a+)+", "(.*)*", r"(\w+){2,}", "((a)+)+"] {
            assert!(has_nested_quantifier(bad), "{bad}");
        }
        for ok in ["error|warn", "(ab)+", r"\(a+\)+", "[(a+)]+", "a+b*"] {
            assert!(!has_nested_quantifier(ok), "{ok}");
        }
    }

    #[test]
    fn extracts_error_messages_from_every_shape() {
        assert_eq!(
            extract_error_message(Some(&json!({"error": {"message": "m"}}))).as_deref(),
            Some("m")
        );
        assert_eq!(
            extract_error_message(Some(&json!({"error": "plain"}))).as_deref(),
            Some("plain")
        );
        assert_eq!(
            extract_error_message(Some(&json!({"result": {"error": "e"}}))).as_deref(),
            Some("e")
        );
        assert_eq!(
            extract_error_message(Some(&json!({"result": {"status_message": "s"}}))).as_deref(),
            Some("s")
        );
        assert_eq!(
            extract_error_message(Some(&json!("<html><body>Log  in</body></html>"))).as_deref(),
            Some("Log in")
        );
        assert_eq!(extract_error_message(Some(&json!({}))), None);
    }

    #[test]
    fn definitions_cover_every_tool() {
        let names: Vec<String> = atf_tool_definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(names, ATF_TOOL_NAMES);
        assert!(is_atf_tool("SN-Get-ATF-Results") && !is_atf_tool("SN-Query-Table"));
    }

    #[test]
    fn js_formatting() {
        assert_eq!(js_num(5.0), "5");
        assert_eq!(js_num(2.5), "2.5");
        assert_eq!(js_string(None), "undefined");
        assert_eq!(state_of(Some(&json!(2))), "successful");
        assert_eq!(state_of(Some(&json!("9"))), "unknown:9");
        assert_eq!(truncate_str("abcdef", 3), "abc… (truncated)");
        assert_eq!(js_to_number(Some(&json!("3"))), json!(3));
    }
}
