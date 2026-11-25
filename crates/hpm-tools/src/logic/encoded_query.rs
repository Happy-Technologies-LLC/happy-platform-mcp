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
/// `^` (would start another condition such as `^OR`/`^NQ`), CR/LF, the
/// Unicode line/paragraph separators and every other control character, so
/// one argument can only ever be one operand.
pub fn query_value<'a>(arg_name: &str, value: &'a str) -> Result<&'a str, ToolError> {
    if value.contains('^') {
        return Err(ToolError::InvalidParams(format!(
            "{arg_name} must not contain '^' (it would add encoded-query conditions)"
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
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

/// Operator words that may directly follow a field name in an encoded query
/// (`descriptionLIKEfoo`, `priorityIN1,2`), including the spaced `NOT IN` and
/// `NOT LIKE` forms. Case-sensitive.
const OPERATOR_WORDS: &[&str] = &[
    "GT_OR_EQUALS_FIELD",
    "LT_OR_EQUALS_FIELD",
    "CHANGESFROM",
    "ISNOTEMPTY",
    "STARTSWITH",
    "INSTANCEOF",
    "VALCHANGES",
    "CHANGESTO",
    "ENDSWITH",
    "GT_FIELD",
    "LT_FIELD",
    "NSAMEAS",
    "BETWEEN",
    "ANYTHING",
    "DYNAMIC",
    "NOT LIKE",
    "NOT IN",
    "NOTLIKE",
    "ISEMPTY",
    "SAMEAS",
    "LIKE",
    "IN",
];

/// The exact field name leading a condition: the `[a-z0-9_.]` run, followed by
/// end of condition, an operator character (any non-alphanumeric character:
/// `= ! > < ^`, space, ...) or a known operator word. `None` when the run is
/// followed by something else (an uppercase run starting no operator, e.g.
/// `u_Foo=1`): the name cannot be told apart from an operator, so the caller
/// reports [`OPAQUE_FIELD`].
fn field_name(s: &str) -> Option<&str> {
    let end = s
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    let rest = &s[end..];
    let ok = match rest.chars().next() {
        None => true,
        Some(c) if !c.is_ascii_alphanumeric() => true,
        Some(_) => OPERATOR_WORDS.iter().any(|w| rest.starts_with(w)),
    };
    ok.then_some(&s[..end])
}

/// Drops the leading whitespace and control characters ServiceNow trims from
/// the start of a condition (and after an `NQ`/`OR`/`ORDERBY`/`GROUPBY`
/// prefix). Any Unicode whitespace counts, which is wider than ServiceNow's
/// own trim: that only makes the analysis see more, never less.
fn trim_cond(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || c.is_control())
}

/// Records the field leading `s` (already trimmed). Nothing for an empty
/// piece; a non-empty piece whose field cannot be read (`=1`, `%20x`,
/// `u_Foo=1`) is recorded as [`OPAQUE_FIELD`], never dropped.
fn push_field(v: &mut Vec<String>, s: &str) {
    if s.is_empty() {
        return;
    }
    let name = field_name(s).filter(|n| !n.is_empty());
    push_unique(v, name.unwrap_or(OPAQUE_FIELD));
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
        // predicate names: report them as the all-fields wildcard. They are
        // matched anywhere in the condition (case-insensitive), since a
        // sub-query is an operator after a field (`caller_idINSUBQUERYsys_id`);
        // a field name that merely contains one only over-reports (fail-safe).
        let opaque = [
            "123TEXTQUERY321",
            "IR_",
            "JOIN",
            "RLQUERY",
            "ENDRLQUERY",
            "SUBQUERY",
            "ENDSUBQUERY",
        ]
        .iter()
        .any(|p| find_ci(cond, p).is_some());
        if opaque {
            push_unique(&mut info.predicate_fields, OPAQUE_FIELD);
            continue;
        }
        let cond = trim_cond(cond);
        let ordering = ["ORDERBYDESC", "ORDERBY", "GROUPBY"]
            .iter()
            .find_map(|p| cond.strip_prefix(p));
        if let Some(field) = ordering {
            push_field(&mut info.ordering_fields, trim_cond(field));
            continue;
        }
        let cond = cond
            .strip_prefix("NQ")
            .or_else(|| cond.strip_prefix("OR"))
            .unwrap_or(cond);
        push_field(&mut info.predicate_fields, trim_cond(cond));
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
        for bad in [
            "x^ORactive=true",
            "x\nactive=true",
            "x\r",
            "x\u{0}",
            "x\t",
            "x\u{2028}active=true",
            "x\u{2029}",
        ] {
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

    /// Review W3 M5: a sub-query is an operator after the field, so the
    /// opaque check matches anywhere in a condition.
    #[test]
    fn sub_queries_and_mid_condition_operators_report_all_fields() {
        let info = analyze_query("caller_idINSUBQUERYsys_id^active=true^ENDSUBQUERY");
        assert!(info.predicate_fields.iter().any(|f| f == OPAQUE_FIELD));
        assert!(!info.predicate_fields.iter().any(|f| f == "ENDSUBQUERY"));
        for q in ["sys_idIR_AND_QUERYx", "ORcaller_idrlquerysys_user"] {
            assert_eq!(analyze_query(q).predicate_fields, [OPAQUE_FIELD], "{q}");
        }
    }

    /// SEAM-T-08: operator-suffixed names resolve to the exact field.
    #[test]
    fn operator_suffixed_names_give_exact_fields() {
        for (q, field) in [
            ("descriptionLIKEx", "description"),
            ("short_descriptionSTARTSWITHabc", "short_description"),
            ("short_descriptionENDSWITHabc", "short_description"),
            ("priorityIN1,2", "priority"),
            ("priorityNOT IN1,2", "priority"),
            ("short_descriptionNOT LIKEfoo", "short_description"),
            ("short_descriptionNOTLIKEfoo", "short_description"),
            ("assigned_toISEMPTY", "assigned_to"),
            ("caller_id.nameISNOTEMPTY", "caller_id.name"),
            ("priorityBETWEEN1@3", "priority"),
            ("a_bGT_FIELDc_d", "a_b"),
            ("a_bGT_OR_EQUALS_FIELDc", "a_b"),
            ("priority>=2", "priority"),
            ("priority!=2", "priority"),
            ("priority=1", "priority"),
            ("priority", "priority"),
        ] {
            assert_eq!(analyze_query(q).predicate_fields, [field], "{q}");
        }
        let info = analyze_query("ORDERBYDESCpriority^GROUPBYstate^ORsys_class_nameIN1,2");
        assert_eq!(info.ordering_fields, ["priority", "state"]);
        assert_eq!(info.predicate_fields, ["sys_class_name"]);
    }

    /// SEAM-T-08: an uppercase run that starts no known operator cannot be
    /// told apart from one, so it is unanalyzable.
    #[test]
    fn unknown_uppercase_suffix_is_opaque() {
        for q in ["u_Foo=1", "u_fooBAR=1", "ORu_Foo=1", "UPPER=1"] {
            assert_eq!(analyze_query(q).predicate_fields, [OPAQUE_FIELD], "{q}");
        }
        assert_eq!(
            analyze_query("ORDERBYu_Foo").ordering_fields,
            [OPAQUE_FIELD]
        );
    }

    fn has_field(info: &QueryInfo, field: &str) -> bool {
        info.predicate_fields
            .iter()
            .chain(&info.ordering_fields)
            .any(|f| f == field || f == OPAQUE_FIELD)
    }

    /// ServiceNow trims leading whitespace, so a condition that starts with
    /// it still filters: the field must not vanish from the analysis.
    #[test]
    fn leading_whitespace_conditions_keep_their_field() {
        for q in [
            " descriptionSTARTSWITHa",
            "active=true^ descriptionSTARTSWITHa",
            "\tdescriptionLIKEa",
            "%20descriptionSTARTSWITHa",
            "%09descriptionLIKEa",
            "\r\n descriptionLIKEa",
            "\u{a0}descriptionLIKEa",
            "\u{2003}descriptionLIKEa",
            "OR descriptionLIKEa",
            "NQ\tdescriptionLIKEa",
            "x=1^OR %20 descriptionLIKEa",
        ] {
            let info = analyze_query(q);
            assert!(
                info.predicate_fields
                    .iter()
                    .any(|f| f == "description" || f == OPAQUE_FIELD),
                "{q:?} -> {info:?}"
            );
        }
        for q in [
            "ORDERBY description",
            "ORDERBYDESC\tdescription",
            "GROUPBY description",
            " ORDERBY description",
            "%20ORDERBYdescription",
            "%20ORDERBY%20description",
        ] {
            let info = analyze_query(q);
            assert!(
                info.ordering_fields
                    .iter()
                    .any(|f| f == "description" || f == OPAQUE_FIELD),
                "{q:?} -> {info:?}"
            );
        }
        // The decoded form is analysed exactly, so these are not merely opaque.
        assert_eq!(
            analyze_query("%20descriptionSTARTSWITHa").predicate_fields,
            [OPAQUE_FIELD, "description"]
        );
        assert_eq!(
            analyze_query(" descriptionSTARTSWITHa").predicate_fields,
            ["description"]
        );
        assert_eq!(
            analyze_query("active=true^ descriptionSTARTSWITHa").predicate_fields,
            ["active", "description"]
        );
        assert_eq!(
            analyze_query("ORDERBY description").ordering_fields,
            ["description"]
        );
    }

    /// A non-empty condition with no readable field is opaque, not dropped.
    #[test]
    fn non_empty_condition_without_field_prefix_is_opaque() {
        for q in ["=1", "!=x", "%zz", "OR=1", "NQ>1", "^ =1"] {
            assert_eq!(analyze_query(q).predicate_fields, [OPAQUE_FIELD], "{q}");
        }
        assert_eq!(analyze_query("ORDERBY=1").ordering_fields, [OPAQUE_FIELD]);
    }

    /// Empty pieces still contribute nothing.
    #[test]
    fn empty_conditions_are_ignored() {
        let info = analyze_query("a=1^^b=2");
        assert_eq!(info.predicate_fields, ["a", "b"]);
        assert!(info.ordering_fields.is_empty());
        for q in [
            "",
            "^",
            "^^",
            "OR",
            "ORDERBY",
            "a=1^OR^b=2",
            "  ",
            "a=1^ ^b=2",
        ] {
            let info = analyze_query(q);
            assert!(
                info.predicate_fields.iter().all(|f| f != OPAQUE_FIELD),
                "{q:?} -> {info:?}"
            );
        }
        assert!(analyze_query("").predicate_fields.is_empty());
        assert!(analyze_query("ORDERBY").ordering_fields.is_empty());
    }

    /// Property: prepending whitespace to any condition, at the start of the
    /// query, after `^`, or after an `OR`/`NQ`/`ORDERBY`/`GROUPBY` prefix,
    /// never removes a field from the analysis.
    #[test]
    fn prepending_whitespace_never_removes_a_field() {
        let fields = ["description", "a", "u_x.y", "sys_id", "u_Foo"];
        let ops = ["=1", "LIKEx", "STARTSWITHa", "!=b", "IN1,2", "ISEMPTY", ""];
        let prefixes = ["", "OR", "NQ", "ORDERBY", "ORDERBYDESC", "GROUPBY"];
        let ws = [
            " ", "  ", "\t", "\r", "\n", "\r\n", "%20", "%09", "%0A", "\u{a0}", "\u{2003}",
            "\u{0b}", "\u{1}",
        ];
        let mut conds = Vec::new();
        for p in prefixes {
            for f in fields {
                for o in ops {
                    conds.push(format!("{p}{f}{o}"));
                }
            }
        }
        conds.push("123TEXTQUERY321=foo".to_owned());
        conds.push("javascript:gs.x()".to_owned());
        conds.push("caller_idINSUBQUERYsys_id".to_owned());
        for c in &conds {
            let base = analyze_query(c);
            for w in ws {
                let variants = [
                    format!("{w}{c}"),
                    format!("active=true^{w}{c}"),
                    format!("{w}{c}^{w}b=2"),
                ];
                for q in variants {
                    let got = analyze_query(&q);
                    for f in &base.predicate_fields {
                        assert!(
                            has_field(&got, f),
                            "{q:?} lost predicate field {f:?}: {got:?}"
                        );
                    }
                    for f in &base.ordering_fields {
                        assert!(
                            has_field(&got, f),
                            "{q:?} lost ordering field {f:?}: {got:?}"
                        );
                    }
                    assert!(
                        got.script_operands.len() >= base.script_operands.len(),
                        "{q:?} lost a script operand"
                    );
                }
            }
            // Whitespace between a prefix and its field.
            for p in ["OR", "NQ", "ORDERBY", "ORDERBYDESC", "GROUPBY"] {
                if let Some(rest) = c.strip_prefix(p) {
                    for w in ws {
                        let q = format!("{p}{w}{rest}");
                        let got = analyze_query(&q);
                        for f in base.predicate_fields.iter().chain(&base.ordering_fields) {
                            assert!(has_field(&got, f), "{q:?} lost {f:?}: {got:?}");
                        }
                    }
                }
            }
        }
    }
}
