//! JavaScript semantics the ported modules depend on: UTF-16 string length,
//! the `\s` whitespace class, `Number.prototype.toString`, `JSON.stringify`,
//! `String(value)` and `Date.prototype.toISOString`.

use serde_json::Value;
use std::fmt::Write as _;

/// `string.length`: the number of UTF-16 code units.
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// True when `value.length > max`, without counting a short string's units.
pub fn utf16_len_exceeds(value: &str, max: usize) -> bool {
    // UTF-16 length never exceeds UTF-8 length.
    value.len() > max && utf16_len(value) > max
}

/// JavaScript `\s`: WhiteSpace and LineTerminator code points.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `Number.prototype.toString()` (radix 10) for any double.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    // `{:e}` yields the shortest round-tripping digits, as ECMAScript requires.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        let _ = write!(out, "e{}{}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    out
}

fn number_value(number: &serde_json::Number) -> f64 {
    // JSON numbers reach JavaScript as doubles; integers beyond 2^53 round the same way.
    number.as_f64().unwrap_or(f64::NAN)
}

fn push_json_string(out: &mut String, value: &str) {
    // serde_json escapes exactly like JSON.stringify for well-formed strings:
    // `"`, `\`, \b \f \n \r \t, other C0 controls as lowercase \u00XX.
    match serde_json::to_string(value) {
        Ok(quoted) => out.push_str(&quoted),
        Err(_) => out.push_str("\"\""),
    }
}

fn push_json(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_to_string(number_value(number))),
        Value::String(text) => push_json_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json_string(out, key);
                out.push(':');
                push_json(out, item);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(value)` without indentation. Object members are emitted in
/// `serde_json::Map` iteration order.
pub fn json_stringify(value: &Value) -> String {
    let mut out = String::new();
    push_json(&mut out, value);
    out
}

/// `JSON.stringify(string)`.
pub fn json_stringify_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    push_json_string(&mut out, value);
    out
}

fn push_js_string(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_to_string(number_value(number))),
        Value::String(text) => out.push_str(text),
        Value::Array(items) => {
            // Array.prototype.join(','): null/undefined elements become ''.
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if !item.is_null() {
                    push_js_string(out, item);
                }
            }
        }
        Value::Object(_) => out.push_str("[object Object]"),
    }
}

/// `String(value)` for a JSON value.
pub fn to_js_string(value: &Value) -> String {
    let mut out = String::new();
    push_js_string(&mut out, value);
    out
}

/// Largest absolute millisecond offset a JavaScript `Date` accepts.
pub const MAX_DATE_MILLIS: i64 = 8_640_000_000_000_000;

/// `Date.prototype.toISOString` failed: the time value is outside the `Date` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Invalid time value")]
pub struct InvalidTimeValue;

/// `new Date(millis).toISOString()`.
pub fn to_iso_string(millis: i64) -> Result<String, InvalidTimeValue> {
    if !(-MAX_DATE_MILLIS..=MAX_DATE_MILLIS).contains(&millis) {
        return Err(InvalidTimeValue);
    }
    const DAY: i64 = 86_400_000;
    let days = millis.div_euclid(DAY);
    let in_day = millis.rem_euclid(DAY);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    let mut out = String::with_capacity(27);
    if (0..=9999).contains(&year) {
        let _ = write!(out, "{year:04}");
    } else {
        let _ = write!(
            out,
            "{}{:06}",
            if year < 0 { '-' } else { '+' },
            year.abs()
        );
    }
    let _ = write!(
        out,
        "-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    );
    Ok(out)
}

/// `Date.now()`.
pub fn now_millis() -> i64 {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_to_string_matches_ecmascript() {
        let cases = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-1.5, "-1.5"),
            (42.0, "42"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1.5e300, "1.5e+300"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (f64::INFINITY, "Infinity"),
            (f64::NAN, "NaN"),
            (700.0, "700"),
        ];
        for (value, expected) in cases {
            assert_eq!(number_to_string(value), expected, "{value}");
        }
    }

    #[test]
    fn json_stringify_matches_javascript() {
        assert_eq!(
            json_stringify(&json!({"a": [1.0, null, true, "x\"\n\u{1}"], "b": 2.5})),
            r#"{"a":[1,null,true,"x\"\n\u0001"],"b":2.5}"#
        );
        assert_eq!(json_stringify_str("\u{2028}\u{7f}"), "\"\u{2028}\u{7f}\"");
    }

    #[test]
    fn string_coercion_matches_javascript() {
        assert_eq!(to_js_string(&json!(null)), "null");
        assert_eq!(to_js_string(&json!([1, null, [2, "a"], {}])), "1,,2,a,[object Object]");
        assert_eq!(to_js_string(&json!(1.0)), "1");
    }

    #[test]
    fn iso_string_matches_javascript() {
        assert_eq!(to_iso_string(0), Ok("1970-01-01T00:00:00.000Z".to_owned()));
        assert_eq!(
            to_iso_string(1_791_248_523_004),
            Ok("2026-10-06T01:02:03.004Z".to_owned())
        );
        assert_eq!(to_iso_string(-1), Ok("1969-12-31T23:59:59.999Z".to_owned()));
        assert_eq!(
            to_iso_string(MAX_DATE_MILLIS),
            Ok("+275760-09-13T00:00:00.000Z".to_owned())
        );
        assert_eq!(
            to_iso_string(-62_198_755_200_000),
            Ok("-000001-01-01T00:00:00.000Z".to_owned())
        );
        assert_eq!(to_iso_string(MAX_DATE_MILLIS + 1), Err(InvalidTimeValue));
    }

    #[test]
    fn utf16_length_counts_code_units() {
        assert_eq!(utf16_len("a\u{1F600}"), 3);
        assert!(utf16_len_exceeds("\u{1F600}", 1));
        assert!(!utf16_len_exceeds("é", 1));
    }
}
