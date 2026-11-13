//! JavaScript value semantics the 6.2.0 handlers rely on: truthiness, template
//! coercion (`${x}`, `undefined` for a missing value), `JSON.stringify(x, null, 2)`
//! and the encoded-query facts the normalizers report.

use serde_json::{Map, Value};

use crate::logic::js::{json_stringify_str, number_to_string, to_js_string};

/// JS truthiness of an optional (absent = `undefined`) value.
pub(super) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Template-literal coercion: `undefined` when absent.
pub(super) fn tpl(v: Option<&Value>) -> String {
    v.map_or_else(|| "undefined".to_owned(), to_js_string)
}

/// `obj.key` on a JSON value (`undefined` unless `obj` is an object with `key`).
pub(super) fn prop<'a>(obj: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    obj.and_then(Value::as_object).and_then(|m| m.get(key))
}

/// `a || b` on optional values.
pub(super) fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) { a } else { b }
}

/// Query-string coercion as `getRecords` applies it: falsy values are not sent.
pub(super) fn param(v: Option<&Value>) -> Option<String> {
    truthy(v).then(|| tpl(v))
}

/// Inserts `key` unless `value` is `undefined` (JSON.stringify drops those).
pub(super) fn put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.to_owned(), v.clone());
    }
}

/// `JSON.stringify(value, null, 2)`; `None` (undefined) renders `undefined` as a
/// template literal would.
pub(super) fn pretty(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "undefined".to_owned();
    };
    let mut out = String::new();
    push(&mut out, value, 0);
    out
}

fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    out.extend(std::iter::repeat_n("  ", depth));
}

fn array_index(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    if b.is_empty()
        || b.len() > 10
        || !b.iter().all(u8::is_ascii_digit)
        || (b.len() > 1 && b[0] == b'0')
    {
        return None;
    }
    key.parse::<u32>().ok().filter(|i| *i != u32::MAX)
}

fn push(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_to_string(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => out.push_str(&json_stringify_str(s)),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                push(out, item, depth + 1);
            }
            indent(out, depth);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            let mut indexed: Vec<(u32, &String, &Value)> = map
                .iter()
                .filter_map(|(k, v)| array_index(k).map(|i| (i, k, v)))
                .collect();
            indexed.sort_unstable_by_key(|(i, _, _)| *i);
            let members = indexed
                .iter()
                .map(|(_, k, v)| (*k, *v))
                .chain(map.iter().filter(|(k, _)| array_index(k).is_none()));
            out.push('{');
            for (i, (k, v)) in members.enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                out.push_str(&json_stringify_str(k));
                out.push_str(": ");
                push(out, v, depth + 1);
            }
            indent(out, depth);
            out.push('}');
        }
    }
}

/// `results.length` in a template (`undefined` for a non-array/non-string).
pub(super) fn length(v: &Value) -> String {
    match v {
        Value::Array(a) => a.len().to_string(),
        Value::String(s) => crate::logic::js::utf16_len(s).to_string(),
        _ => "undefined".to_owned(),
    }
}

/// Facts an encoded query exposes to policy: referenced fields, ordering fields
/// and `javascript:` operands (whitespace-normalized, prefix stripped).
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct QueryFacts {
    pub predicate: Vec<String>,
    pub ordering: Vec<String>,
    pub operands: Vec<String>,
}

impl QueryFacts {
    /// Scans the query as sent and, when it holds `%` escapes, its decoded
    /// form too, so an encoded `javascript%3A` operand cannot slip past the guard.
    pub(super) fn add(&mut self, query: &str) {
        self.scan(query);
        if query.contains('%') {
            let decoded = percent_decode(query);
            if decoded != query {
                self.scan(&decoded);
            }
        }
    }

    fn scan(&mut self, query: &str) {
        for seg in query.split('^') {
            if let Some(field) = seg
                .strip_prefix("ORDERBYDESC")
                .or_else(|| seg.strip_prefix("ORDERBY"))
                .or_else(|| seg.strip_prefix("GROUPBY"))
            {
                push_unique(&mut self.ordering, field_prefix(field));
                continue;
            }
            let cond = seg
                .strip_prefix("NQ")
                .or_else(|| seg.strip_prefix("OR"))
                .unwrap_or(seg);
            let end = cond
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(cond.len());
            push_unique(&mut self.predicate, &cond[..end]);
            let lower = seg.to_ascii_lowercase();
            if let Some(at) = lower.find("javascript:") {
                let operand = seg[at + "javascript:".len()..]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                self.operands.push(operand);
            }
        }
    }
}

/// Copied from lane A's `crud::normalize`; the lead dedupes on merge.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The leading field name (`[A-Za-z0-9_.]*`) of a condition or ordering term.
fn field_prefix(s: &str) -> &str {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    &s[..end]
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && !v.iter().any(|x| x == s) {
        v.push(s.to_owned());
    }
}

/// `sysparm_order_by` / comma-separated field list.
pub(super) fn field_list(v: Option<&Value>) -> Vec<String> {
    param(v)
        .map(|s| {
            s.split(',')
                .map(|f| f.trim().trim_start_matches('-').to_owned())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pretty_matches_json_stringify_indent_2() {
        let v = json!({"b": [1, {"x": 1.5}], "2": [], "1": {}, "s": "q\"u"});
        assert_eq!(
            pretty(Some(&v)),
            "{\n  \"1\": {},\n  \"2\": [],\n  \"b\": [\n    1,\n    {\n      \"x\": 1.5\n    }\n  ],\n  \"s\": \"q\\\"u\"\n}"
        );
        assert_eq!(pretty(None), "undefined");
        assert_eq!(pretty(Some(&json!(2.0))), "2");
    }

    #[test]
    fn truthiness_and_templates() {
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!([]))));
        assert_eq!(tpl(None), "undefined");
        assert_eq!(tpl(Some(&json!(3))), "3");
        assert_eq!(param(Some(&json!(0))), None);
    }

    #[test]
    fn query_facts() {
        let mut f = QueryFacts::default();
        f.add("active=true^ORpriority=1^assigned_to=javascript:gs.getUserID()^ORDERBYDESCnumber");
        assert_eq!(f.predicate, ["active", "priority", "assigned_to"]);
        assert_eq!(f.ordering, ["number"]);
        assert_eq!(f.operands, ["gs.getUserID()"]);
    }

    #[test]
    fn query_facts_decode_escapes_and_trim_ordering() {
        let mut f = QueryFacts::default();
        f.add("caller_id=javascript%3Ags.getSession()^GROUPBYstate^ORDERBYnumber DESC");
        assert_eq!(f.operands, ["gs.getSession()"]);
        assert_eq!(f.ordering, ["state", "number"]);
    }
}
