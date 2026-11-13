//! Rust port of `scripts/golden/lib/scrub.mjs` (`fixtures/golden/NORMALIZATION.json`,
//! stage `tools` = scrub + normalize). Applied to every JSON string value and
//! object key, never to the serialization. The `regex` crate has no look-around, so
//! the rules that use it re-check the neighbouring text by hand; the JS patterns
//! are quoted on each rule.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Map, Value};

/// What a rule does with one match. `None` keeps the matched text.
type Rewrite = fn(&str, usize, &Captures<'_>) -> Option<String>;

struct Rule {
    re: Regex,
    rewrite: Rewrite,
}

fn rule(pattern: &str, rewrite: Rewrite) -> Rule {
    Rule {
        re: Regex::new(pattern).expect("scrub rule"),
        rewrite,
    }
}

fn digit(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_digit())
}

fn before(text: &str, start: usize) -> Option<char> {
    text[..start].chars().next_back()
}

fn after(text: &str, end: usize) -> Option<char> {
    text[end..].chars().next()
}

/// `\b` in JS is ASCII-only; `(?-u:\b)` is the same here.
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // ---- scrub (SCRUB_RULES) ----
        // /\b[a-z0-9][a-z0-9-]*\.(?:service-now|servicenowservices)\.com\b/gi
        rule(
            r"(?i)(?-u:\b)[a-z0-9][a-z0-9-]*\.(?:service-now|servicenowservices)\.com(?-u:\b)",
            |_, _, _| Some("{{SN_HOST}}".into()),
        ),
        // /\b(?:dev451274|ccf[a-z0-9]*)\b/gi
        rule(
            r"(?i)(?-u:\b)(?:dev451274|ccf[a-z0-9]*)(?-u:\b)",
            |_, _, _| Some("{{REAL_INSTANCE}}".into()),
        ),
        // /\bBasic\s+[A-Za-z0-9+/=]{8,}/g
        rule(r"(?-u:\b)Basic\s+[A-Za-z0-9+/=]{8,}", |_, _, _| {
            Some("Basic {{CREDENTIAL}}".into())
        }),
        // /\bBearer\s+[A-Za-z0-9._~+/=-]{8,}/g
        rule(r"(?-u:\b)Bearer\s+[A-Za-z0-9._~+/=-]{8,}", |_, _, _| {
            Some("Bearer {{CREDENTIAL}}".into())
        }),
        // /\b(password|client_secret|refresh_token|access_token)=[^&\s"']+/gi
        rule(
            r#"(?i)(?-u:\b)(password|client_secret|refresh_token|access_token)=[^&\s"']+"#,
            |_, _, c| Some(format!("{}={{{{CREDENTIAL}}}}", &c[1])),
        ),
        // /\b(JSESSIONID|glide_user_route|glide_session_store|glide_user_activity|BIGipServer\w*)=[^;\s"']+/g
        rule(
            r#"(?-u:\b)(JSESSIONID|glide_user_route|glide_session_store|glide_user_activity|BIGipServer[A-Za-z0-9_]*)=[^;\s"']+"#,
            |_, _, c| Some(format!("{}={{{{COOKIE}}}}", &c[1])),
        ),
        // ---- normalize (NORMALIZE_RULES) ----
        // N-ISO-TIMESTAMP /\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b/g
        rule(
            r"(?-u:\b)[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?Z(?-u:\b)",
            |_, _, _| Some("{{ISO_TIMESTAMP}}".into()),
        ),
        // N-FILE-TIMESTAMP /(?<![0-9])\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z(?![0-9])/g
        rule(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}-[0-9]{3}Z",
            |t, s, c| {
                let end = s + c[0].len();
                (!digit(before(t, s)) && !digit(after(t, end))).then(|| "{{FILE_TIMESTAMP}}".into())
            },
        ),
        // N-SN-DATETIME /\b(?!2026-01-15 12:00:00\b)\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\b/g
        rule(
            r"(?-u:\b)[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}(?-u:\b)",
            |_, _, c| (&c[0] != "2026-01-15 12:00:00").then(|| "{{SN_DATETIME}}".into()),
        ),
        // N-DURATION-MS /\b(\d+)\s?ms\b/g
        rule(r"(?-u:\b)([0-9]+)\s?ms(?-u:\b)", |_, _, _| {
            Some("{{N}}ms".into())
        }),
        // N-EXEC-TIME-FIELD /("?execution_time_ms"?\s*:\s*)\d+/g
        rule(r#"("?execution_time_ms"?\s*:\s*)[0-9]+"#, |_, _, c| {
            Some(format!("{}{{{{N}}}}", &c[1]))
        }),
        // N-EPOCH-MS /\b(MCP_Script_|MCP_Workflow_|mcp_marker_)\d{10,}\b/g
        rule(
            r"(?-u:\b)(MCP_Script_|MCP_Workflow_|mcp_marker_)[0-9]{10,}(?-u:\b)",
            |_, _, c| Some(format!("{}{{{{EPOCH_MS}}}}", &c[1])),
        ),
        // N-RANDOM-MARKER /\bMCP_SCRIPT_[A-Z0-9]+_[0-9a-f]{8,}\b/g
        rule(
            r"(?-u:\b)MCP_SCRIPT_[A-Z0-9]+_[0-9a-f]{8,}(?-u:\b)",
            |_, _, _| Some("MCP_SCRIPT_{{MARKER}}".into()),
        ),
        // N-TRIGGER-MARKER /MCP_\d{10,}_[0-9a-f]{6,}(?![0-9a-f])/g: the greedy hex run
        // already ends before any further hex digit.
        rule(r"MCP_[0-9]{10,}_[0-9a-f]{6,}", |_, _, _| {
            Some("MCP_{{EPOCH_MS}}_{{MARKER}}".into())
        }),
        // N-SN-DATETIME-ENCODED
        // /(?<!\d)(?!2026-01-15\+12%3A00%3A00)\d{4}-\d{2}-\d{2}\+\d{2}%3A\d{2}%3A\d{2}\b/g
        rule(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}\+[0-9]{2}%3A[0-9]{2}%3A[0-9]{2}(?-u:\b)",
            |t, s, c| {
                (!digit(before(t, s)) && &c[0] != "2026-01-15+12%3A00%3A00")
                    .then(|| "{{SN_DATETIME_ENCODED}}".into())
            },
        ),
        // N-UUID /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g
        rule(
            r"(?-u:\b)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(?-u:\b)",
            |_, _, _| Some("{{UUID}}".into()),
        ),
    ]
});

/// `new Scrubber(literals, { rules: 'all' })`.
pub struct Scrubber {
    literals: Vec<(String, String)>,
}

impl Scrubber {
    /// `literals`: exact text → placeholder; empty values are dropped, longest first.
    pub fn new(literals: Vec<(String, String)>) -> Self {
        let mut literals: Vec<_> = literals
            .into_iter()
            .filter(|(v, _)| !v.is_empty())
            .collect();
        literals.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
        Self { literals }
    }

    pub fn string(&self, value: &str) -> String {
        let mut out = value.to_owned();
        for (literal, placeholder) in &self.literals {
            if out.contains(literal.as_str()) {
                out = out.replace(literal.as_str(), placeholder);
            }
        }
        for r in RULES.iter() {
            let text = out.clone();
            out =
                r.re.replace_all(&text, |c: &Captures<'_>| {
                    let m = c.get(0).expect("whole match");
                    (r.rewrite)(&text, m.start(), c).unwrap_or_else(|| m.as_str().to_owned())
                })
                .into_owned();
        }
        out
    }

    pub fn deep(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.string(s)),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.deep(v)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| (self.string(k), self.deep(v)))
                    .collect::<Map<_, _>>(),
            ),
            other => other.clone(),
        }
    }
}
