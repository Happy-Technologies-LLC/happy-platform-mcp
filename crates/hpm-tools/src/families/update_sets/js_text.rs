//! JavaScript value-to-text semantics the 6.2.0 handlers rely on: template
//! interpolation of possibly-`undefined` properties, `||` truthiness and
//! `JSON.stringify(value, null, 2)`.

use serde_json::{Map, Value};

use crate::logic::js;

/// JavaScript truthiness of a present JSON value.
pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Truthiness of a possibly-`undefined` property.
pub(super) fn truthy_opt(value: Option<&Value>) -> bool {
    value.is_some_and(truthy)
}

/// `${value}` for a possibly-`undefined` property.
pub(super) fn text(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), js::to_js_string)
}

/// `value || fallback`.
pub(super) fn or(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(v) if truthy(v) => v.clone(),
        _ => fallback,
    }
}

/// Inserts `key` only when the property is defined (`JSON.stringify` drops
/// `undefined` members; `null` is kept).
pub(super) fn put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.to_owned(), v.clone());
    }
}

/// `string.substring(0, max)` in UTF-16 code units (a split surrogate pair is
/// dropped whole).
pub(super) fn utf16_prefix(value: &str, max: usize) -> &str {
    let mut units = 0;
    for (index, c) in value.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &value[..index];
        }
    }
    value
}

/// `string.substring(0, max)` plus `...` when the string is longer.
pub(super) fn preview(value: &str, max: usize) -> String {
    let head = utf16_prefix(value, max);
    if js::utf16_len_exceeds(value, max) {
        format!("{head}...")
    } else {
        head.to_owned()
    }
}

/// A canonical array index key (ordered first by `JSON.stringify`/`Object.keys`).
pub(super) fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    key.parse::<u32>().ok().filter(|index| *index != u32::MAX)
}

/// `JSON.stringify(value, null, 2)`.
pub(super) fn pretty(value: &Value) -> String {
    let mut out = String::new();
    push_pretty(&mut out, value, 0);
    out
}

/// `JSON.stringify(value, null, 2)` interpolated: `undefined` when absent.
pub(super) fn pretty_opt(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), pretty)
}

fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn push_pretty(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                push_pretty(out, item, depth + 1);
            }
            indent(out, depth);
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
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
            for (position, (key, item)) in members.enumerate() {
                if position > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                out.push_str(&js::json_stringify_str(key));
                out.push_str(": ");
                push_pretty(out, item, depth + 1);
            }
            indent(out, depth);
            out.push('}');
        }
        other => out.push_str(&js::json_stringify(other)),
    }
}

/// `value.replace(/\s+/g, ' ').trim()`.
pub(super) fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_space = false;
    for c in value.chars() {
        if js::is_js_whitespace(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    js::trim(&out).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pretty_matches_json_stringify_indent_2() {
        let value = json!({"b": [1, {"c": null}], "a": {}, "1": "x", "e": [], "f": 1.5});
        assert_eq!(
            pretty(&value),
            "{\n  \"1\": \"x\",\n  \"b\": [\n    1,\n    {\n      \"c\": null\n    }\n  ],\n  \"a\": {},\n  \"e\": [],\n  \"f\": 1.5\n}"
        );
        assert_eq!(pretty_opt(None), "undefined");
    }

    #[test]
    fn truthiness_and_text_follow_javascript() {
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!([])));
        assert_eq!(text(None), "undefined");
        assert_eq!(text(Some(&json!(null))), "null");
        assert_eq!(or(Some(&json!(0)), json!(100)), json!(100));
        assert_eq!(or(Some(&json!(7)), json!(100)), json!(7));
    }

    #[test]
    fn preview_counts_utf16_units() {
        assert_eq!(preview("abc", 2), "ab...");
        assert_eq!(preview("ab", 2), "ab");
        assert_eq!(preview("a😀b", 2), "a...");
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
    }
}
