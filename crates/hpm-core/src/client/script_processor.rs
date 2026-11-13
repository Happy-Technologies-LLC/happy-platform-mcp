//! `/sys.scripts.do` response parsing and outcome classification (ports
//! `src/script-processor.js`). Character scanners replace the JS regexes.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::error::ClientError;
use super::http::{HttpResponse, TransportError};

pub const SCRIPT_PROCESSOR_PATH: &str = "/sys.scripts.do";
pub const SCRIPT_HISTORY_TABLE: &str = "sys_script_execution_history";
pub const ROLLBACK_CONTEXT_TABLE: &str = "sys_rollback_context";
pub const PROCESSOR_MAX_LINES: usize = 50_001;
pub const PROCESSOR_DEFAULT_TIMEOUT_MS: u64 = 300_000;
pub const MAX_REASON_LENGTH: usize = 1000;
pub const NOT_RUN: &str = "SCRIPT_PROCESSOR_NOT_RUN";
pub const UNKNOWN_OUTCOME: &str = "SCRIPT_PROCESSOR_UNKNOWN_OUTCOME";

const GLOBAL_OUTPUT_PREFIX: &str = "*** Script: ";
const EXECUTION_ERROR_PREFIXES: [&str; 2] =
    ["Script execution error: ", "Script compilation error: "];
const SCOPED_SYNTAX_ERROR_PREFIX: &str = "SyntaxError: ";
const READ_ONLY_DENIAL: &str = "ReadOnlyRoleAccessHandler";
const PRE_SEND_ERROR_CODES: [&str; 5] = [
    "ECONNREFUSED",
    "ENOTFOUND",
    "EAI_AGAIN",
    "EHOSTUNREACH",
    "ENETUNREACH",
];
const TIMEOUT_ERROR_CODES: [&str; 2] = ["ECONNABORTED", "ETIMEDOUT"];

pub fn not_run_error(reason: &str) -> ClientError {
    let mut e = ClientError::new(
        "ScriptProcessorNotRunError",
        format!("The /sys.scripts.do processor did not run the script: {reason}"),
        Some(NOT_RUN),
    );
    e.reason = Some(reason.to_owned());
    e
}

pub fn unknown_outcome_error(reason: &str) -> ClientError {
    let mut e = ClientError::new(
        "ScriptProcessorUnknownOutcomeError",
        format!("Unknown outcome from the /sys.scripts.do processor: {reason}"),
        Some(UNKNOWN_OUTCOME),
    );
    e.reason = Some(reason.to_owned());
    e
}

/// JS `\s` (ASCII whitespace plus the Unicode spaces JS includes).
fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

pub fn is_hex_sys_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `withReason`.
pub fn with_reason(script: &str, reason: Option<&str>) -> String {
    let flattened: String = reason
        .map(|r| {
            let words: Vec<&str> = r.split(is_js_space).filter(|w| !w.is_empty()).collect();
            words
                .join(" ")
                .encode_utf16()
                .take(MAX_REASON_LENGTH)
                .collect::<Vec<u16>>()
        })
        .map(|u| String::from_utf16_lossy(&u))
        .unwrap_or_default();
    if flattened.is_empty() {
        script.to_owned()
    } else {
        format!("{script}\n// happy-platform-mcp: {flattened}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeRequest {
    Global,
    SysId(String),
    Name(String),
}

/// `classifyScope`.
pub fn classify_scope(scope: &str) -> Result<ScopeRequest, ClientError> {
    if scope == "global" {
        return Ok(ScopeRequest::Global);
    }
    if is_hex_sys_id(scope) {
        return Ok(ScopeRequest::SysId(scope.to_ascii_lowercase()));
    }
    if !scope.is_empty()
        && scope.len() <= 100
        && scope
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Ok(ScopeRequest::Name(scope.to_owned()));
    }
    Err(ClientError::plain(
        "scope must be 'global', a 32-character hexadecimal application sys_id, or a scope name of lowercase letters, digits and underscores (at most 100 characters)",
    ))
}

/// `decodeEntities`.
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').and_then(|semi| {
            let entity = &after[..semi];
            let lower = entity.to_ascii_lowercase();
            let valid = if let Some(hex) = lower.strip_prefix("#x") {
                !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
            } else if let Some(dec) = lower.strip_prefix('#') {
                !dec.is_empty() && dec.bytes().all(|b| b.is_ascii_digit())
            } else {
                !lower.is_empty() && lower.bytes().all(|b| b.is_ascii_lowercase())
            };
            if !valid {
                return None;
            }
            let replacement = if let Some(hex) = lower.strip_prefix("#x") {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
            } else if let Some(dec) = lower.strip_prefix('#') {
                dec.parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
            } else {
                match lower.as_str() {
                    "amp" => Some("&".into()),
                    "lt" => Some("<".into()),
                    "gt" => Some(">".into()),
                    "quot" => Some("\"".into()),
                    "apos" => Some("'".into()),
                    "nbsp" => Some("\u{a0}".into()),
                    _ => None,
                }
            };
            Some((semi, replacement.unwrap_or_else(|| format!("&{entity};"))))
        });
        match decoded {
            Some((semi, value)) => {
                out.push_str(&value);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `textContent`: drops comments and tags in one left-to-right scan.
pub fn text_content(html: &str) -> String {
    let mut text = String::new();
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = rest[4..].find("-->").map_or(html.len(), |e| i + 4 + e + 3);
        } else if rest.starts_with('<') {
            i = rest[1..].find('>').map_or(html.len(), |e| i + 1 + e + 1);
        } else {
            let stop = rest.find('<').unwrap_or(rest.len());
            text.push_str(&rest[..stop]);
            i += stop;
        }
    }
    decode_entities(&text)
}

/// Finds `<name` (case-insensitive) followed by a non-word char, from `from`.
fn find_tag_open(lower: &str, name: &str, from: usize) -> Option<usize> {
    let needle = format!("<{name}");
    let mut start = from;
    while let Some(pos) = lower[start..].find(&needle) {
        let at = start + pos;
        let next = lower.as_bytes().get(at + needle.len());
        if next.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_')) {
            return Some(at);
        }
        start = at + 1;
    }
    None
}

fn to_segments(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let Some(open) = find_tag_open(&lower, "pre", 0) else {
        return Vec::new();
    };
    let Some(gt) = lower[open..].find('>') else {
        return Vec::new();
    };
    let content_start = open + gt + 1;
    // `</pre\s*>`
    let mut search = content_start;
    let content_end = loop {
        let Some(pos) = lower[search..].find("</pre") else {
            return Vec::new();
        };
        let at = search + pos;
        let tail = lower[at + 5..].trim_start_matches(is_js_space);
        if tail.starts_with('>') {
            break at;
        }
        search = at + 1;
    };
    let inner = &html[content_start..content_end];
    let inner_lower = &lower[content_start..content_end];
    let mut segments = Vec::new();
    let mut seg_start = 0;
    let mut from = 0;
    while let Some(br) = find_tag_open(inner_lower, "br", from) {
        let Some(gt) = inner_lower[br..].find('>') else {
            break;
        };
        segments.push(text_content(&inner[seg_start..br]));
        seg_start = br + gt + 1;
        from = seg_start;
    }
    segments.push(text_content(&inner[seg_start..]));
    segments
}

/// First `<a href=...>` pointing at the execution history table.
fn find_execution_history_id(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(open) = find_tag_open(&lower, "a", from) {
        from = open + 2;
        let tag_end = lower[open..].find('>').map_or(lower.len(), |e| open + e);
        let tag = &html[open..tag_end];
        let tag_lower = &lower[open..tag_end];
        let mut search = 0;
        let href = loop {
            let Some(pos) = tag_lower[search..].find("href") else {
                break None;
            };
            let at = search + pos;
            let boundary = at == 0 || !tag_lower.as_bytes()[at - 1].is_ascii_alphanumeric();
            let after = tag[at + 4..].trim_start_matches(is_js_space);
            if boundary {
                if let Some(value) = after.strip_prefix('=') {
                    let value = value.trim_start_matches(is_js_space);
                    let raw = if let Some(v) = value.strip_prefix('"') {
                        v.find('"').map(|e| &v[..e])
                    } else if let Some(v) = value.strip_prefix('\'') {
                        v.find('\'').map(|e| &v[..e])
                    } else {
                        let e = value
                            .find(|c: char| is_js_space(c) || c == '>')
                            .unwrap_or(value.len());
                        (e > 0).then(|| &value[..e])
                    };
                    break raw;
                }
            }
            search = at + 4;
        };
        let Some(raw) = href else { continue };
        let href = decode_entities(raw);
        if !href.contains(&format!("{SCRIPT_HISTORY_TABLE}.do")) {
            continue;
        }
        let query = href.find('?').map_or(href.as_str(), |q| &href[q + 1..]);
        let sys_id = url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "sys_id")
            .map(|(_, v)| v.into_owned());
        return sys_id.filter(|s| is_hex_sys_id(s));
    }
    None
}

fn scope_of(text: &str) -> Option<String> {
    const HEAD: &str = "Script completed in scope ";
    let mut from = 0;
    while let Some(pos) = text[from..].find(HEAD) {
        let start = from + pos + HEAD.len();
        // `(.+?): script` — lazy, no newlines.
        let line_end = text[start..].find('\n').map_or(text.len(), |e| start + e);
        if let Some(end) = text[start..line_end].find(": script") {
            if end > 0 {
                return Some(text[start..start + end].to_owned());
            }
            if let Some(next) = text[start + 1..line_end].find(": script") {
                return Some(text[start..start + 1 + next].to_owned());
            }
        }
        from = start;
    }
    None
}

fn error_description(rest: &str) -> Option<&str> {
    const HEAD: &str = "Error Description: ";
    const TAIL: &str = ", Script ES Level:";
    let start = rest.find(HEAD)? + HEAD.len();
    let end = rest[start..].rfind(TAIL)?;
    Some(&rest[start..start + end])
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptResult {
    pub ok: bool,
    pub scope: String,
    pub output: Vec<String>,
    pub errors: Vec<String>,
    pub log: Vec<String>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub execution_history_id: Option<String>,
}

impl ScriptResult {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("ok".into(), json!(self.ok));
        m.insert("scope".into(), json!(self.scope));
        m.insert("output".into(), json!(self.output));
        m.insert("errors".into(), json!(self.errors));
        m.insert("log".into(), json!(self.log));
        m.insert("warnings".into(), json!(self.warnings));
        if self.truncated {
            m.insert("truncated".into(), json!(true));
        }
        if let Some(id) = &self.execution_history_id {
            m.insert("executionHistoryId".into(), json!(id));
        }
        Value::Object(m)
    }
}

/// `parseScriptResponse`.
pub fn parse_script_response(html: &str) -> Option<ScriptResult> {
    if js_trim(html).is_empty() {
        return None;
    }
    let scope = scope_of(&text_content(html))?;
    let output_prefix = if scope == "global" {
        GLOBAL_OUTPUT_PREFIX.to_owned()
    } else {
        format!("{scope}: ")
    };
    let mut output = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut log = Vec::new();
    let mut syntax_errors = Vec::new();
    let mut line_count = 0usize;
    for segment in to_segments(html) {
        let text = js_trim(&segment);
        if text.is_empty() {
            continue;
        }
        line_count += 1;
        let printed = segment.trim_start_matches(is_js_space);
        let exec_prefix = EXECUTION_ERROR_PREFIXES
            .iter()
            .find(|p| text.starts_with(**p));
        if let Some(out) = printed.strip_prefix(output_prefix.as_str()) {
            output.push(out.to_owned());
        } else if let Some(prefix) = exec_prefix {
            let rest = &text[prefix.len()..];
            let message = error_description(rest).unwrap_or(rest).to_owned();
            if !errors.contains(&message) {
                errors.push(message);
            }
        } else if text.starts_with(SCOPED_SYNTAX_ERROR_PREFIX) {
            syntax_errors.push(text.split('\n').next().unwrap_or("").to_owned());
            log.push(text.to_owned());
        } else {
            log.push(text.to_owned());
        }
    }
    if errors.is_empty() {
        for message in syntax_errors {
            if !errors.contains(&message) {
                errors.push(message);
            }
        }
    }
    let mut warnings = Vec::new();
    if log.iter().any(|l: &String| l.contains(READ_ONLY_DENIAL)) {
        warnings.push("The user has the snc_read_only role, so the instance ran the script in sandbox mode and kept no execution history.".to_owned());
    }
    let truncated = line_count >= PROCESSOR_MAX_LINES;
    if truncated {
        warnings.push(format!(
            "The instance keeps only the last {PROCESSOR_MAX_LINES} lines of a run, so earlier output may be missing."
        ));
    }
    Some(ScriptResult {
        ok: errors.is_empty(),
        scope,
        output,
        errors,
        log,
        warnings,
        truncated,
        execution_history_id: find_execution_history_id(html),
    })
}

fn contains_ci_word_login(lower: &str) -> bool {
    // `<title>[^<]*\blog\s?in\b`
    let mut from = 0;
    while let Some(pos) = lower[from..].find("<title>") {
        let start = from + pos + 7;
        let end = lower[start..].find('<').map_or(lower.len(), |e| start + e);
        let title = &lower[start..end];
        let bytes = title.as_bytes();
        for (i, _) in title.match_indices("log") {
            let word_start =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if !word_start {
                continue;
            }
            let mut j = i + 3;
            if bytes.get(j).is_some_and(|b| b.is_ascii_whitespace())
                && bytes.get(j + 1) == Some(&b'i')
            {
                j += 1;
            }
            if title[j..].starts_with("in") {
                let after = bytes.get(j + 2);
                if after.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_')) {
                    return true;
                }
            }
        }
        from = start;
    }
    false
}

fn has_password_field(lower: &str) -> bool {
    // `name\s*=\s*["']?user_password\b`
    for (i, _) in lower.match_indices("name") {
        let rest = lower[i + 4..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        if let Some(after) = rest.strip_prefix("user_password") {
            if after
                .bytes()
                .next()
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
            {
                return true;
            }
        }
    }
    false
}

fn has_login_form(lower: &str) -> bool {
    // `<form\b[^>]*action\s*=\s*["']?[^"'>]*login\.do`
    let mut from = 0;
    while let Some(open) = find_tag_open(lower, "form", from) {
        from = open + 1;
        let end = lower[open..].find('>').map_or(lower.len(), |e| open + e);
        let tag = &lower[open..end];
        for (i, _) in tag.match_indices("action") {
            let rest = tag[i + 6..].trim_start_matches(|c: char| c.is_ascii_whitespace());
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
            let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
            let value_end = rest.find(['"', '\'']).unwrap_or(rest.len());
            if rest[..value_end].contains("login.do") {
                return true;
            }
        }
    }
    false
}

/// `isAuthFailurePage`.
pub fn is_auth_failure_page(body: &str, response: &HttpResponse) -> bool {
    if response
        .header("x-is-logged-in")
        .is_some_and(|v| v.eq_ignore_ascii_case("false"))
    {
        return true;
    }
    if js_trim(body).is_empty() {
        return true;
    }
    let lower = body.to_ascii_lowercase();
    has_password_field(&lower) || has_login_form(&lower) || contains_ci_word_login(&lower)
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessorOutcome {
    Ran(Box<ScriptResult>),
    NotRun(String),
    Unknown(String),
}

/// `classifyProcessorOutcome`.
pub fn classify_processor_outcome(
    result: &Result<HttpResponse, TransportError>,
) -> ProcessorOutcome {
    let response = match result {
        Ok(r) => r,
        Err(error) => {
            let code = error.code.as_deref();
            if let Some(c) = code.filter(|c| PRE_SEND_ERROR_CODES.contains(c)) {
                return ProcessorOutcome::NotRun(format!(
                    "connection error before the request was sent ({c})"
                ));
            }
            if code.is_some_and(|c| TIMEOUT_ERROR_CODES.contains(&c))
                || error.message.to_ascii_lowercase().contains("timeout")
            {
                return ProcessorOutcome::Unknown("stopped waiting for the processor response (timeout); the script may still be running on the instance".into());
            }
            let suffix = code.map(|c| format!(" ({c})")).unwrap_or_default();
            return ProcessorOutcome::Unknown(format!(
                "the connection failed after the request may have been sent{suffix}"
            ));
        }
    };
    let status = response.status;
    match status {
        401 | 403 => {
            let hint = if status == 403 {
                " (running background scripts requires the admin role)"
            } else {
                ""
            };
            ProcessorOutcome::NotRun(format!("HTTP {status} from {SCRIPT_PROCESSOR_PATH}{hint}"))
        }
        404 => ProcessorOutcome::NotRun(format!(
            "HTTP 404: {SCRIPT_PROCESSOR_PATH} is not available on this instance"
        )),
        300..=399 => {
            let location = response
                .header("location")
                .unwrap_or("")
                .to_ascii_lowercase();
            if ["login", "logout", "auth", "saml", "sso"]
                .iter()
                .any(|w| location.contains(w))
            {
                ProcessorOutcome::NotRun(format!(
                    "HTTP {status} redirect to a login/authentication page"
                ))
            } else {
                ProcessorOutcome::Unknown(format!(
                    "HTTP {status} redirect that is not a login page"
                ))
            }
        }
        200..=299 => {
            let body = response.text();
            if let Some(result) = parse_script_response(&body) {
                return ProcessorOutcome::Ran(Box::new(result));
            }
            if js_trim(&body).to_lowercase() == "not authorized" {
                return ProcessorOutcome::NotRun("the processor refused the request (not authorized): the user may not be allowed to run scripts in this scope, e.g. a protected store application".into());
            }
            if is_auth_failure_page(&body, response) {
                return ProcessorOutcome::NotRun(
                    "the processor returned an unauthenticated/login page instead of a script result".into(),
                );
            }
            ProcessorOutcome::Unknown(format!(
                "HTTP {status} response that is not a recognizable script result"
            ))
        }
        _ => ProcessorOutcome::Unknown(format!("HTTP {status} from {SCRIPT_PROCESSOR_PATH}")),
    }
}

/// Insertion-ordered cookie jar (`Map` semantics: re-set keeps position).
#[derive(Clone, Default)]
pub struct CookieJar {
    order: Vec<String>,
    values: BTreeMap<String, String>,
}

/// Session cookies are credentials: `Debug` lists cookie names only.
impl std::fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieJar")
            .field("names", &self.order)
            .finish()
    }
}

impl Drop for CookieJar {
    fn drop(&mut self) {
        for v in self.values.values_mut() {
            zeroize::Zeroize::zeroize(v);
        }
    }
}

impl CookieJar {
    pub fn set(&mut self, name: &str, value: &str) {
        if self
            .values
            .insert(name.to_owned(), value.to_owned())
            .is_none()
        {
            self.order.push(name.to_owned());
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// `mergeSetCookies`.
    pub fn merge_set_cookies(&mut self, response: &HttpResponse) {
        for entry in response.header_all("set-cookie") {
            let pair = entry.split(';').next().unwrap_or("");
            let Some(sep) = pair.find('=') else { continue };
            if sep == 0 {
                continue;
            }
            self.set(js_trim(&pair[..sep]), js_trim(&pair[sep + 1..]));
        }
    }

    /// `serializeCookies`.
    pub fn serialize(&self) -> String {
        self.order
            .iter()
            .map(|n| format!("{n}={}", self.values.get(n).map_or("", String::as_str)))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// `scriptExecutionLinks`.
pub fn script_execution_links(
    execution_history_id: Option<&str>,
    rollback_context_id: Option<&str>,
) -> Value {
    let mut m = Map::new();
    if let Some(id) = execution_history_id {
        m.insert(
            "executionHistory".into(),
            json!(format!("/{SCRIPT_HISTORY_TABLE}.do?sys_id={id}")),
        );
    }
    if let Some(id) = rollback_context_id {
        m.insert(
            "rollbackContext".into(),
            json!(format!("/{ROLLBACK_CONTEXT_TABLE}.do?sys_id={id}")),
        );
    }
    Value::Object(m)
}

/// `rollbackContextOf`.
pub fn rollback_context_of(record: Option<&Value>) -> Option<String> {
    let value = record?.get("rollback_context")?;
    let id = match value {
        Value::Object(o) => o.get("value")?,
        other => other,
    };
    id.as_str().filter(|s| is_hex_sys_id(s)).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HISTORY: &str = "0123456789abcdef0123456789abcdef";

    fn page(scope: &str, lines: &[&str]) -> String {
        format!(
            "<html><body><a href=\"sys_script_execution_history.do?sys_id={HISTORY}\">history</a><p>Script completed in scope {scope}: script</p><pre>{}<BR/></pre></body></html>",
            lines.join("<BR/>")
        )
    }

    fn ok(resp: HttpResponse) -> ProcessorOutcome {
        classify_processor_outcome(&Ok(resp))
    }

    #[test]
    fn parses_global_output_and_history() {
        let r = parse_script_response(&page(
            "global",
            &[
                "*** Script: hello",
                "*** Script: a &lt;b&gt; &amp; &#39;c&#39;",
            ],
        ));
        let r = r.unwrap_or_else(|| panic!("parse"));
        assert!(r.ok);
        assert_eq!(r.scope, "global");
        assert_eq!(r.output, vec!["hello", "a <b> & 'c'"]);
        assert_eq!(r.execution_history_id.as_deref(), Some(HISTORY));
    }

    #[test]
    fn scoped_prefix_and_noise_to_log() {
        let r = parse_script_response(&page("x_app", &["x_app: one", "session noise"]))
            .unwrap_or_else(|| panic!("parse"));
        assert_eq!(r.output, vec!["one"]);
        assert_eq!(r.log, vec!["session noise"]);
    }

    #[test]
    fn execution_errors_and_syntax_errors() {
        let r = parse_script_response(&page(
            "global",
            &["Script execution error: Script ES Level: 0, Error Description: boom, Script ES Level: 0, Interpreted Mode: true"],
        ))
        .unwrap_or_else(|| panic!("parse"));
        assert!(!r.ok);
        assert_eq!(r.errors, vec!["boom"]);
        let s = parse_script_response(&page("x_app", &["SyntaxError: missing ;\nmore"]))
            .unwrap_or_else(|| panic!("parse"));
        assert_eq!(s.errors, vec!["SyntaxError: missing ;"]);
        assert!(!s.ok);
    }

    #[test]
    fn markup_cannot_survive() {
        assert_eq!(
            text_content("a<scr<script>ipt>b<!-- x -->c<unterminated"),
            "aipt>bc"
        );
        assert_eq!(
            decode_entities("&#x41;&#66;&bogus;&#99999999;"),
            "AB&bogus;&#99999999;"
        );
    }

    #[test]
    fn non_result_bodies() {
        assert!(parse_script_response("").is_none());
        assert!(parse_script_response("<html>hello</html>").is_none());
        assert!(matches!(
            ok(HttpResponse::new(200, "not authorized")),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(
                200,
                "<form action=\"/login.do\"><input name=\"user_password\"></form>"
            )),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(200, "<title>Log in</title>")),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(200, "something else")),
            ProcessorOutcome::Unknown(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(200, "x").with_header("X-Is-Logged-In", "false")),
            ProcessorOutcome::NotRun(_)
        ));
    }

    #[test]
    fn status_classification() {
        assert_eq!(
            ok(HttpResponse::new(401, "")),
            ProcessorOutcome::NotRun("HTTP 401 from /sys.scripts.do".into())
        );
        assert_eq!(
            ok(HttpResponse::new(403, "")),
            ProcessorOutcome::NotRun("HTTP 403 from /sys.scripts.do (running background scripts requires the admin role)".into())
        );
        assert!(matches!(
            ok(HttpResponse::new(404, "")),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(302, "").with_header("location", "/login.do")),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(302, "").with_header("location", "/home")),
            ProcessorOutcome::Unknown(_)
        ));
        assert!(matches!(
            ok(HttpResponse::new(500, "")),
            ProcessorOutcome::Unknown(_)
        ));
    }

    #[test]
    fn transport_classification() {
        let c = |code: &str, msg: &str| {
            classify_processor_outcome(&Err(TransportError::new(code, msg)))
        };
        assert!(matches!(
            c("ECONNREFUSED", "x"),
            ProcessorOutcome::NotRun(_)
        ));
        assert!(
            matches!(c("ETIMEDOUT", "x"), ProcessorOutcome::Unknown(r) if r.contains("timeout"))
        );
        assert_eq!(
            c("ECONNRESET", "x"),
            ProcessorOutcome::Unknown(
                "the connection failed after the request may have been sent (ECONNRESET)".into()
            )
        );
    }

    #[test]
    fn scope_classification_and_reason() {
        assert_eq!(classify_scope("global").ok(), Some(ScopeRequest::Global));
        assert_eq!(
            classify_scope(&HISTORY.to_uppercase()).ok(),
            Some(ScopeRequest::SysId(HISTORY.into()))
        );
        assert_eq!(
            classify_scope("x_app").ok(),
            Some(ScopeRequest::Name("x_app".into()))
        );
        assert!(classify_scope("X App").is_err());
        assert_eq!(
            with_reason("s", Some("  a\n b  ")),
            "s\n// happy-platform-mcp: a b"
        );
        assert_eq!(with_reason("s", None), "s");
    }

    #[test]
    fn cookies_and_rollback() {
        let mut jar = CookieJar::default();
        jar.merge_set_cookies(
            &HttpResponse::new(200, "")
                .with_header("set-cookie", "JSESSIONID=abc; Path=/")
                .with_header("set-cookie", "glide_session_store=XYZ; HttpOnly")
                .with_header("set-cookie", "=bad"),
        );
        jar.set("JSESSIONID", "def");
        assert_eq!(jar.serialize(), "JSESSIONID=def; glide_session_store=XYZ");
        assert_eq!(
            rollback_context_of(Some(&json!({"rollback_context": {"value": HISTORY}}))).as_deref(),
            Some(HISTORY)
        );
        assert_eq!(
            rollback_context_of(Some(&json!({"rollback_context": "nope"}))),
            None
        );
        assert_eq!(
            script_execution_links(Some("a"), None),
            json!({"executionHistory": "/sys_script_execution_history.do?sys_id=a"})
        );
    }
}
