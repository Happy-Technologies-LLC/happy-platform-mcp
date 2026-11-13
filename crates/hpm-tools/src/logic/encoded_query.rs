//! Shared ServiceNow encoded-query helpers for every tool family: the value
//! guard for arguments interpolated into a query (owner decision), and the
//! analyser that reports predicate/ordering fields and `javascript:` operands
//! to the central guard (RFC §3.2). Moved here from lane A's `crud::normalize`
//! (lane B held a copy) so there is one implementation.

use hpm_types::tool::ToolError;

/// Fields, ordering and script operands found in a ServiceNow encoded query.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct QueryInfo {
    pub predicate_fields: Vec<String>,
    pub ordering_fields: Vec<String>,
    /// Text after each `javascript:` (case-insensitive), whitespace-collapsed.
    pub script_operands: Vec<String>,
}

const SCRIPT_PREFIX: &str = "javascript:";

/// Predicate field reported for whole-record search and cross-table operators
/// (`123TEXTQUERY321`, `IR_*`, `JOIN`, `RLQUERY`, `SUBQUERY`): all fields.
pub const OPAQUE_FIELD: &str = "*";

/// A value a tool interpolates into an encoded query (`field=<value>`). Refuses
/// `^` (would start another condition such as `^OR`/`^NQ`), CR/LF and every
/// other control character, so one argument can only ever be one operand.
pub fn query_value<'a>(arg_name: &str, value: &'a str) -> Result<&'a str, ToolError> {
    if value.contains('^') {
        return Err(ToolError::InvalidParams(format!(
            "{arg_name} must not contain '^' (it would add encoded-query conditions)"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ToolError::InvalidParams(format!(
            "{arg_name} must not contain line breaks or other control characters"
        )));
    }
    Ok(value)
}

/// `%XX` decoding (invalid escapes kept verbatim; invalid UTF-8 replaced).
pub fn percent_decode(s: &str) -> String {
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

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    (0..=h.len().saturating_sub(n.len()))
        .find(|&i| h.len() >= n.len() && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn field_prefix(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    &s[..end]
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !s.is_empty() && !v.iter().any(|x| x == s) {
        v.push(s.to_owned());
    }
}

fn analyze_into(query: &str, info: &mut QueryInfo) {
    for cond in query.split('^') {
        if let Some(at) = find_ci(cond, SCRIPT_PREFIX) {
            let operand = cond[at + SCRIPT_PREFIX.len()..]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            info.script_operands.push(operand);
        }
        // Whole-record search and cross-table operators read fields no
        // predicate names: report them as the all-fields wildcard.
        let opaque = [
            "123TEXTQUERY321",
            "IR_",
            "JOIN",
            "RLQUERY",
            "ENDRLQUERY",
            "SUBQUERY",
        ]
        .iter()
        .any(|p| {
            cond.strip_prefix("NQ")
                .or_else(|| cond.strip_prefix("OR"))
                .unwrap_or(cond)
                .get(..p.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(p))
        });
        if opaque {
            push_unique(&mut info.predicate_fields, OPAQUE_FIELD);
            continue;
        }
        let ordering = ["ORDERBYDESC", "ORDERBY", "GROUPBY"]
            .iter()
            .find_map(|p| cond.strip_prefix(p));
        if let Some(field) = ordering {
            push_unique(&mut info.ordering_fields, field_prefix(field));
            continue;
        }
        let cond = cond
            .strip_prefix("NQ")
            .or_else(|| cond.strip_prefix("OR"))
            .unwrap_or(cond);
        push_unique(&mut info.predicate_fields, field_prefix(cond));
    }
}

/// Analyses the query as sent and, when it holds `%` escapes, its decoded form
/// too, so an encoded `javascript%3A` operand cannot slip past the guard.
pub fn analyze_query(query: &str) -> QueryInfo {
    let mut info = QueryInfo::default();
    analyze_into(query, &mut info);
    if query.contains('%') {
        let decoded = percent_decode(query);
        if decoded != query {
            analyze_into(&decoded, &mut info);
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_value_accepts_plain_values_and_refuses_injection() {
        assert_eq!(query_value("number", "INC0010001").ok(), Some("INC0010001"));
        assert_eq!(query_value("q", "a b=c").ok(), Some("a b=c"));
        for bad in ["x^ORactive=true", "x\nactive=true", "x\r", "x\u{0}", "x\t"] {
            let Err(ToolError::InvalidParams(m)) = query_value("number", bad) else {
                panic!("{bad:?} accepted");
            };
            assert!(m.starts_with("number must not contain"), "{m}");
        }
    }

    #[test]
    fn percent_decode_keeps_invalid_escapes() {
        assert_eq!(percent_decode("a%3Ab%zz%4"), "a:b%zz%4");
    }

    #[test]
    fn query_operands_fields_and_ordering_are_extracted() {
        let info = analyze_query(
            "active=true^ORpriority=1^assigned_to=JavaScript:gs.getUserID()^NQstate!=7^ORDERBYDESCsys_created_on^caller_id.name=x",
        );
        assert_eq!(
            info.predicate_fields,
            [
                "active",
                "priority",
                "assigned_to",
                "state",
                "caller_id.name"
            ]
        );
        assert_eq!(info.ordering_fields, ["sys_created_on"]);
        assert_eq!(info.script_operands, ["gs.getUserID()"]);
    }

    #[test]
    fn encoded_and_spaced_script_operands_are_still_found() {
        let info = analyze_query("sys_id=javascript%3Anew  GlideRecord('x')");
        assert_eq!(info.script_operands, ["new GlideRecord('x')"]);
        let info = analyze_query("a=javascript:gs.daysAgo(7)javascript:gs.setProperty('a','b')");
        assert_eq!(
            info.script_operands,
            ["gs.daysAgo(7)javascript:gs.setProperty('a','b')"]
        );
    }

    #[test]
    fn opaque_operators_report_all_fields() {
        let info = analyze_query("123TEXTQUERY321=foo^ORJOINincident.x=y");
        assert_eq!(info.predicate_fields, [OPAQUE_FIELD]);
    }
}
