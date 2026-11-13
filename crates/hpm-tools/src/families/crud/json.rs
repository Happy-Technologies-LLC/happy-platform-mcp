//! `JSON.stringify(value, null, 2)` and the small JavaScript value rules the
//! 6.2.0 handlers rely on. A JavaScript `undefined` is an absent key
//! (`Option::None`); JSON `null` is a present value.

use serde_json::{Map, Value};

use crate::logic::js;

/// A canonical array index key (JavaScript own-property order puts these first).
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    key.parse::<u32>().ok().filter(|i| *i != u32::MAX)
}

/// Object members in JavaScript property order.
fn members(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut indexed: Vec<(u32, &String, &Value)> = map
        .iter()
        .filter_map(|(k, v)| array_index(k).map(|i| (i, k, v)))
        .collect();
    indexed.sort_unstable_by_key(|(i, _, _)| *i);
    indexed
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(map.iter().filter(|(k, _)| array_index(k).is_none()))
        .collect()
}

fn push_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn push(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_indent(out, depth + 1);
                push(out, item, depth + 1);
            }
            push_indent(out, depth);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in members(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_indent(out, depth + 1);
                out.push_str(&js::json_stringify_str(key));
                out.push_str(": ");
                push(out, item, depth + 1);
            }
            push_indent(out, depth);
            out.push('}');
        }
        scalar => out.push_str(&js::json_stringify(scalar)),
    }
}

/// `JSON.stringify(value, null, 2)`.
pub fn pretty(value: &Value) -> String {
    let mut out = String::new();
    push(&mut out, value, 0);
    out
}

/// JavaScript truthiness.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `String(value)`, `undefined` for an absent value.
pub fn text(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), js::to_js_string)
}

/// `a || b`.
pub fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) { a } else { b }
}

/// `obj?.key` on an untyped value (`undefined` unless `obj` is an object).
pub fn prop<'a>(obj: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    obj.and_then(|o| o.as_object()).and_then(|m| m.get(key))
}

/// Destructuring default: `value = default` applies only to `undefined`.
pub fn arg_or(args: &Value, key: &str, default: Value) -> Value {
    args.get(key).cloned().unwrap_or(default)
}

/// Assigns `key` unless the value is `undefined` (stringify omits it anyway).
pub fn set(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    match value {
        Some(v) => {
            map.insert(key.to_owned(), v.clone());
        }
        None => {
            map.shift_remove(key);
        }
    }
}

/// `x.length` for the value a Table API call returned, as interpolated text.
/// `Err` carries the JavaScript `TypeError` message for `null`/`undefined`.
pub fn length_text(value: &Value) -> Result<String, String> {
    match value {
        Value::Array(items) => Ok(items.len().to_string()),
        Value::String(s) => Ok(js::utf16_len(s).to_string()),
        Value::Null => Err("Cannot read properties of undefined (reading 'length')".into()),
        _ => Ok("undefined".into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn pretty_matches_json_stringify_with_two_space_indent() {
        let v = json!({"b": [1, 2.5, {"x": null}], "a": {}, "c": [], "10": true, "2": "s\n"});
        assert_eq!(
            pretty(&v),
            "{\n  \"2\": \"s\\n\",\n  \"10\": true,\n  \"b\": [\n    1,\n    2.5,\n    {\n      \"x\": null\n    }\n  ],\n  \"a\": {},\n  \"c\": []\n}"
        );
        assert_eq!(pretty(&json!(1.0)), "1");
    }

    #[test]
    fn truthiness_and_text_follow_javascript() {
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!([]))));
        assert_eq!(text(None), "undefined");
        assert_eq!(text(Some(&json!(25))), "25");
        assert_eq!(text(Some(&Value::Null)), "null");
        assert_eq!(
            or(Some(&json!("")), Some(&json!("x"))).cloned(),
            Some(json!("x"))
        );
    }

    #[test]
    fn length_text_of_table_api_results() {
        assert_eq!(length_text(&json!([1, 2])).as_deref(), Ok("2"));
        assert_eq!(length_text(&json!({})).as_deref(), Ok("undefined"));
        assert!(length_text(&Value::Null).is_err());
    }
}
