//! Port of tests/natural-language.test.js (pure parser parts).

use hpm_tools::logic::nl::{
    MAX_NATURAL_LANGUAGE_QUERY_LENGTH, MatchedPattern, NaturalLanguageError, NaturalLanguageResult,
    SUPPORTED_PATTERNS, parse_natural_language, parse_natural_language_value,
    supported_patterns_json, test_parser,
};
use serde_json::json;
use std::time::{Duration, Instant};

const MAX_QUERY_LENGTH: usize = 2048;
const PROMPT_COMPLETION: Duration = Duration::from_millis(50);

fn parse(query: &str) -> NaturalLanguageResult {
    parse_natural_language(query, "incident").expect("valid query")
}

fn parsed_in(query: &str, table: &str) -> (String, String) {
    let result = parse_natural_language(query, table).expect("valid query");
    (result.encoded_query, result.unmatched_text)
}

fn parsed(query: &str) -> (String, String) {
    parsed_in(query, "incident")
}

fn pair(encoded: &str, unmatched: &str) -> (String, String) {
    (encoded.to_owned(), unmatched.to_owned())
}

/// Repeat `unit` after `prefix` and append `suffix`, without exceeding `length`.
fn filled_to(prefix: &str, unit: &str, suffix: &str, length: usize) -> String {
    let repeats = (length - prefix.len() - suffix.len()) / unit.len();
    format!("{prefix}{}{suffix}", unit.repeat(repeats))
}

fn filled(prefix: &str, unit: &str, suffix: &str) -> String {
    filled_to(prefix, unit, suffix, MAX_QUERY_LENGTH)
}

// --- input validation ---

#[test]
fn exports_the_2048_character_input_limit() {
    assert_eq!(MAX_NATURAL_LANGUAGE_QUERY_LENGTH, MAX_QUERY_LENGTH);
}

#[test]
fn rejects_non_string_queries_with_a_type_error() {
    for query in [
        None,
        Some(json!(null)),
        Some(json!(42)),
        Some(json!(["P1"])),
        Some(json!({ "query": "P1" })),
    ] {
        let error = parse_natural_language_value(query.as_ref(), None).expect_err("non-string");
        assert_eq!(error, NaturalLanguageError::NotAString);
        assert_eq!(error.to_string(), "Natural language query must be a string");
    }
}

#[test]
fn rejects_a_query_one_character_over_the_limit_without_echoing_it() {
    let query = format!("P1 {}", "x".repeat(MAX_QUERY_LENGTH - 2));
    assert_eq!(query.len(), MAX_QUERY_LENGTH + 1);
    let error = parse_natural_language(&query, "incident").expect_err("too long");
    assert_eq!(error, NaturalLanguageError::TooLong { received: 2049 });
    assert_eq!(
        error.to_string(),
        "Natural language query must be at most 2048 characters (received 2049)"
    );
    assert!(!error.to_string().contains("xxxx"));
}

#[test]
fn rejects_an_over_limit_pathological_query_promptly() {
    let query = format!("with{}!", " ".repeat(100_000));
    let started = Instant::now();
    assert!(matches!(
        parse_natural_language(&query, "incident"),
        Err(NaturalLanguageError::TooLong { .. })
    ));
    assert!(started.elapsed() < PROMPT_COMPLETION);
}

#[test]
fn measures_length_in_utf16_code_units() {
    let query = "\u{1F600}".repeat(1025);
    assert_eq!(
        parse_natural_language(&query, "incident"),
        Err(NaturalLanguageError::TooLong { received: 2050 })
    );
    assert!(parse_natural_language(&"é".repeat(2048), "incident").is_ok());
}

#[test]
fn accepts_a_query_of_exactly_the_maximum_length() {
    let query = filled("P1 incidents about SAP", " ", "");
    assert_eq!(query.len(), MAX_QUERY_LENGTH);
    assert_eq!(
        parsed(&query),
        pair(
            "priority=1^short_descriptionLIKESAP^ORdescriptionLIKESAP",
            "incidents"
        )
    );
}

#[test]
fn returns_guidance_for_an_empty_query() {
    assert_eq!(
        parse(""),
        NaturalLanguageResult {
            encoded_query: String::new(),
            matched_patterns: vec![],
            unmatched_text: String::new(),
            suggestions: vec!["Please provide a valid query string".to_owned()],
        }
    );
}

// --- documented semantics ---

const DOCUMENTED_EXAMPLES: &[(&str, &str, &str)] = &[
    ("find all P1 incidents", "priority=1", "find all incidents"),
    (
        "show recent problems assigned to me",
        "assigned_to=javascript:gs.getUserID()^sys_created_on>javascript:gs.daysAgo(7)",
        "show problems",
    ),
    (
        "high priority changes created last week",
        "priority=2",
        "changes created last week",
    ),
    (
        "open incidents about SAP",
        "state=1^ORstate=2^ORstate=3^short_descriptionLIKESAP^ORdescriptionLIKESAP",
        "incidents",
    ),
    (
        "unassigned P2 incidents",
        "assigned_toISEMPTY^priority=2",
        "incidents",
    ),
    (
        "unassigned P2 incidents created today",
        "assigned_toISEMPTY^created_on>javascript:gs.daysAgoStart(0)^priority=2",
        "incidents",
    ),
    (
        "incidents created today with high priority",
        "created_on>javascript:gs.daysAgoStart(0)^priority=2",
        "incidents",
    ),
    (
        "closed problems assigned to John Smith",
        "assigned_to.nameLIKEJohn Smith^state=7",
        "problems",
    ),
    (
        "critical incidents opened in the last 30 days",
        "sys_created_on>javascript:gs.daysAgo(30)",
        "critical incidents",
    ),
    (
        "show me my active tickets",
        "active=true",
        "show me my tickets",
    ),
    (
        "new incidents with high impact and high urgency",
        "impact=1^urgency=1^sys_created_on>javascript:gs.daysAgo(7)",
        "incidents",
    ),
    (
        "incidents containing database error",
        "short_descriptionLIKEdatabase error^ORdescriptionLIKEdatabase error",
        "incidents",
    ),
    (
        "P1 incidents assigned to Network Team",
        "assigned_to.nameLIKENetwork Team^priority=1",
        "incidents",
    ),
    (
        "incidents created after January 1",
        "created_on>January 1",
        "incidents",
    ),
    (
        "resolved incidents about authentication",
        "state=6^short_descriptionLIKEauthentication^ORdescriptionLIKEauthentication",
        "incidents",
    ),
    (
        "high priority incidents assigned to me",
        "assigned_to=javascript:gs.getUserID()^priority=2",
        "incidents",
    ),
    (
        "recent problems about database",
        "sys_created_on>javascript:gs.daysAgo(7)^short_descriptionLIKEdatabase^ORdescriptionLIKEdatabase",
        "problems",
    ),
    ("P1 incidents", "priority=1", "incidents"),
    (
        "recent problems",
        "sys_created_on>javascript:gs.daysAgo(7)",
        "problems",
    ),
    ("high priority", "priority=2", ""),
    ("P1", "priority=1", ""),
    ("priority 2", "priority=2", ""),
    ("critical priority", "priority=1", ""),
    (
        "assigned to me",
        "assigned_to=javascript:gs.getUserID()",
        "",
    ),
    ("unassigned", "assigned_toISEMPTY", ""),
    (
        "assigned to John Smith",
        "assigned_to.nameLIKEJohn Smith",
        "",
    ),
    ("my incidents", "assigned_to=javascript:gs.getUserID()", ""),
    ("open", "state=1^ORstate=2^ORstate=3", ""),
    ("active", "active=true", ""),
    ("in progress", "state=2", ""),
    ("closed", "state=7", ""),
    ("resolved", "state=6", ""),
    (
        "created today",
        "created_on>javascript:gs.daysAgoStart(0)",
        "",
    ),
    ("recent", "sys_created_on>javascript:gs.daysAgo(7)", ""),
    (
        "opened yesterday",
        "sys_created_on>javascript:gs.daysAgoStart(1)",
        "",
    ),
    (
        "about SAP",
        "short_descriptionLIKESAP^ORdescriptionLIKESAP",
        "",
    ),
    (
        "containing error",
        "short_descriptionLIKEerror^ORdescriptionLIKEerror",
        "",
    ),
    (
        "description contains authentication",
        "descriptionLIKEauthentication",
        "",
    ),
    ("high impact", "impact=1", ""),
    ("medium impact", "impact=2", ""),
    ("low impact", "impact=3", ""),
    ("high urgency", "urgency=1", ""),
    ("medium urgency", "urgency=2", ""),
    ("low urgency", "urgency=3", ""),
    ("number is INC0012345", "number=INC0012345", ""),
    ("caller is John Smith", "caller_id.nameLIKEJohn Smith", ""),
    ("category is Software", "categoryLIKESoftware", ""),
    (
        "assignment group is Network Team",
        "assignment_group.nameLIKENetwork Team",
        "",
    ),
    (
        "high priority and assigned to me",
        "assigned_to=javascript:gs.getUserID()^priority=2",
        "",
    ),
    (
        "recent and unassigned",
        "assigned_toISEMPTY^sys_created_on>javascript:gs.daysAgo(7)",
        "",
    ),
    ("P1 or P2 incidents", "priority=1", "P2 incidents"),
];

#[test]
fn documented_examples_parse_as_in_javascript() {
    for (query, encoded, unmatched) in DOCUMENTED_EXAMPLES {
        assert_eq!(parsed(query), pair(encoded, unmatched), "{query}");
    }
}

#[test]
fn every_supported_pattern_and_test_parser_example_is_covered() {
    let covered: Vec<&str> = DOCUMENTED_EXAMPLES
        .iter()
        .map(|(query, _, _)| *query)
        .collect();
    let unparsable = ["new", "last 7 days", "updated last week"];
    for example in SUPPORTED_PATTERNS.iter().flat_map(|entry| entry.examples) {
        assert!(
            covered.contains(example) || unparsable.contains(example),
            "{example}"
        );
    }
    for (query, result) in test_parser("incident") {
        assert!(covered.contains(&query), "{query}");
        assert!(result.is_ok());
    }
    let json = supported_patterns_json();
    assert_eq!(json["priority"]["examples"][1], "P1");
    assert_eq!(
        json["assignmentGroup"]["encodedQuery"],
        "assignment_group.nameLIKENetwork Team"
    );
    let keys: Vec<&String> = json.as_object().expect("object").keys().collect();
    assert_eq!(keys.first().map(|k| k.as_str()), Some("priority"));
    assert_eq!(keys.last().map(|k| k.as_str()), Some("combining"));
}

#[test]
fn reports_matched_text_and_conditions_in_evaluation_order() {
    let matched = |pattern, matched: &str, condition: &str| MatchedPattern {
        pattern,
        matched: matched.to_owned(),
        condition: condition.to_owned(),
    };
    let result = parse("unassigned P2 incidents created today");
    assert_eq!(
        result,
        NaturalLanguageResult {
            encoded_query: "assigned_toISEMPTY^created_on>javascript:gs.daysAgoStart(0)^priority=2".to_owned(),
            matched_patterns: vec![
                matched("unassigned", "unassigned", "assigned_toISEMPTY"),
                matched("relative-day", "created today", "created_on>javascript:gs.daysAgoStart(0)"),
                matched("priority-code", "P2", "priority=2"),
            ],
            unmatched_text: "incidents".to_owned(),
            suggestions: vec![
                "Unrecognized: \"incidents\"".to_owned(),
                "Try using encoded query format: field=value^field2=value2".to_owned(),
                "Supported patterns: priority (P1-P5), state (new/open/closed), assigned to me/unassigned, recent, dates".to_owned(),
            ],
        }
    );
    // JSON shape and key order match the JavaScript object literal.
    assert_eq!(
        serde_json::to_string(&result.to_json()).expect("json"),
        concat!(
            r#"{"encodedQuery":"assigned_toISEMPTY^created_on>javascript:gs.daysAgoStart(0)^priority=2","#,
            r#""matchedPatterns":[{"pattern":"unassigned","matched":"unassigned","condition":"assigned_toISEMPTY"},"#,
            r#"{"pattern":"relative-day","matched":"created today","condition":"created_on>javascript:gs.daysAgoStart(0)"},"#,
            r#"{"pattern":"priority-code","matched":"P2","condition":"priority=2"}],"#,
            r#""unmatchedText":"incidents","suggestions":["Unrecognized: \"incidents\"","#,
            r#""Try using encoded query format: field=value^field2=value2","#,
            r#""Supported patterns: priority (P1-P5), state (new/open/closed), assigned to me/unassigned, recent, dates"]}"#
        )
    );
}

#[test]
fn parses_relative_ranges_explicit_dates_and_priority_words() {
    assert_eq!(
        parsed("updated in the last 2 weeks"),
        pair("updated_on>javascript:gs.daysAgo(14)", "")
    );
    assert_eq!(
        parsed("closed last 3 months"),
        pair("closed_on>javascript:gs.daysAgo(90)", "")
    );
    assert_eq!(
        parsed("opened before March 5, 2024"),
        pair("sys_created_on<March 5, 2024", "")
    );
    assert_eq!(
        parsed("created after June 30 2025 P3"),
        pair("created_on>June 30 2025^priority=3", "")
    );
    assert_eq!(parsed("priority planning"), pair("priority=5", ""));
    assert_eq!(
        parsed("PRIORITY 4 recently created"),
        pair("priority=4^sys_created_on>javascript:gs.daysAgo(7)", "")
    );
    assert_eq!(parsed("number = CHG0000042"), pair("number=CHG0000042", ""));
}

#[test]
fn formats_huge_relative_ranges_like_javascript_numbers() {
    // Rust-only parity check of parseInt + Number#toString.
    assert_eq!(
        parsed("created last 999999999999999999999 days").0,
        "created_on>javascript:gs.daysAgo(1e+21)"
    );
    assert_eq!(
        parsed("created last 100000000000000000000 weeks").0,
        "created_on>javascript:gs.daysAgo(700000000000000000000)"
    );
    assert_eq!(
        parsed(&format!("created last {} months", "9".repeat(400))).0,
        "created_on>javascript:gs.daysAgo(Infinity)"
    );
}

#[test]
fn maps_states_per_table_and_falls_back_to_incident_states() {
    assert_eq!(
        parsed_in("open", "problem").0,
        "state=1^ORstate=2^ORstate=3^ORstate=4"
    );
    assert_eq!(parsed_in("open", "change_request").0, "state<0");
    assert_eq!(parsed_in("closed", "change_request").0, "state=3");
    assert_eq!(parsed_in("on hold", "cmdb_ci").0, "state=3");
    assert_eq!(parsed_in("closed", "__proto__").0, "state=7");
    assert_eq!(parsed_in("resolved", "change_request").0, "state=resolved");
}

#[test]
fn ends_names_and_search_terms_only_at_whole_connector_words() {
    assert_eq!(
        parsed("assigned to John Andrews").0,
        "assigned_to.nameLIKEJohn Andrews"
    );
    assert_eq!(
        parsed("caller is Ann Orr and P1").0,
        "priority=1^caller_id.nameLIKEAnn Orr"
    );
    assert_eq!(
        parsed("about order incidents").0,
        "short_descriptionLIKEorder incidents^ORdescriptionLIKEorder incidents"
    );
    assert_eq!(
        parsed("description contains \"disk full\" and P2").0,
        "priority=2^descriptionLIKEdisk full"
    );
    assert_eq!(
        parsed("includes 'VPN' created today").0,
        "created_on>javascript:gs.daysAgoStart(0)^short_descriptionLIKEVPN^ORdescriptionLIKEVPN"
    );
}

#[test]
fn does_not_turn_connectors_or_empty_text_into_search_terms() {
    assert_eq!(parsed("incidents with and").0, "");
    assert_eq!(parsed("category is or"), pair("", "category is or"));
    assert_eq!(parsed("P1 about '").0, "priority=1");
}

#[test]
fn cannot_inject_conditions_or_ordering_through_a_description_value() {
    let result = parse("description contains foo^active=false^ORDERBYnumber");
    assert_eq!(result.encoded_query, "active=true");
    let patterns: Vec<&str> = result.matched_patterns.iter().map(|m| m.pattern).collect();
    assert_eq!(patterns, ["state"]);
    assert!(!result.encoded_query.contains("ORDERBY"));
    assert!(!result.encoded_query.contains("active=false"));
}

#[test]
fn cannot_inject_ordering_through_a_line_break_in_a_description_value() {
    assert_eq!(parsed("description contains x\nORDERBYDESCsys_id").0, "");
    assert_eq!(
        parsed("description contains x\r\nORDERBYDESCsys_id and P1").0,
        "priority=1"
    );
    assert_eq!(
        parsed("description contains disk full\n").0,
        "descriptionLIKEdisk full"
    );
}

fn captured_values(m: &MatchedPattern) -> Vec<String> {
    let separators: &[&str] = match m.pattern {
        "assigned-to-name" => &["assigned_to.nameLIKE"],
        "caller" => &["caller_id.nameLIKE"],
        "category" => &["categoryLIKE"],
        "assignment-group" => &["assignment_group.nameLIKE"],
        "description-contains" => &["descriptionLIKE"],
        "content" => &["short_descriptionLIKE", "^ORdescriptionLIKE"],
        "before-date" => &["<"],
        "after-date" => &[">"],
        _ => return vec![],
    };
    if m.pattern == "content" {
        return m.condition[separators[0].len()..]
            .split(separators[1])
            .map(str::to_owned)
            .collect();
    }
    let at = m.condition.find(separators[0]).expect("separator");
    vec![m.condition[at + separators[0].len()..].to_owned()]
}

fn forbidden(value: &str) -> bool {
    value.chars().any(
        |c| matches!(c, '^' | '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}'),
    )
}

#[test]
fn never_captures_carets_line_breaks_or_control_characters_in_any_value() {
    let prefixes = [
        "assigned to John",
        "caller is John",
        "category = Software",
        "assignment group is Network",
        "description contains foo",
        "description contains \"foo",
        "about SAP",
        "containing \"SAP",
        "with SAP",
        "includes SAP",
        "created before January",
        "opened after June 30,",
    ];
    let unsafe_chars = [
        "^", "\n", "\r", "\r\n", "\t", "\u{0}", "\u{7}", "\u{1b}", "\u{7f}", "\u{85}", "\u{2028}",
        "\u{2029}",
    ];
    let tails = [
        " Smith",
        "1",
        " 2024",
        "ORDERBYnumber",
        " active=false",
        "\" and P1",
        "",
    ];
    let mut captured_count = 0;
    for prefix in prefixes {
        for character in unsafe_chars {
            for tail in tails {
                for query in [
                    format!("{prefix}{character}{tail}"),
                    format!("{prefix} {character} {tail}"),
                    format!("{prefix}{character}"),
                ] {
                    for m in parse(&query).matched_patterns {
                        for value in captured_values(&m) {
                            captured_count += 1;
                            assert!(!forbidden(&value), "{query:?} captured {value:?}");
                        }
                    }
                }
            }
        }
    }
    assert!(captured_count > 100, "{captured_count}");
    assert_eq!(
        parsed("assigned to John\nand P1").0,
        "assigned_to.nameLIKEJohn^priority=1"
    );
    assert_eq!(
        parsed("created before January 1\n").0,
        "created_on<January 1"
    );
    assert_eq!(parsed("created before January\n1").0, "");
}

#[test]
fn joins_conditions_with_or_when_unmatched_text_contains_or() {
    assert_eq!(
        parsed("critical priority or unassigned"),
        pair("assigned_toISEMPTY^ORpriority=1", "")
    );
}

#[test]
fn passes_encoded_queries_through_unchanged_when_no_pattern_matches() {
    assert_eq!(
        parse("category=software^impact=1"),
        NaturalLanguageResult {
            encoded_query: "category=software^impact=1".to_owned(),
            matched_patterns: vec![],
            unmatched_text: String::new(),
            suggestions: vec!["Using query as-is (appears to be encoded query format)".to_owned()],
        }
    );
}

#[test]
fn returns_the_original_text_when_nothing_matches() {
    let result = parse("show me everything");
    assert_eq!(result.encoded_query, "");
    assert_eq!(result.unmatched_text, "show me everything");
    assert_eq!(
        result.suggestions[0],
        "No patterns matched. Supported patterns:"
    );
    assert_eq!(result.suggestions.len(), 7);
}

// --- bounded runtime ---

#[test]
fn adversarial_queries_complete_promptly() {
    let and_spaces = format!("about{}", " ".repeat(15));
    let queries: Vec<(&str, String)> = vec![
        (
            "content keyword followed by a whitespace run",
            format!("with{}!", " ".repeat(MAX_QUERY_LENGTH - 5)),
        ),
        (
            "assignee keyword followed by a whitespace run",
            format!("assigned to a{}!", " ".repeat(MAX_QUERY_LENGTH - 14)),
        ),
        ("repeated content keywords", filled("", "with ", "")),
        (
            "repeated content keywords and spaces",
            filled("", &and_spaces, "!"),
        ),
        ("alternating and/or chain", filled("", "and or ", "")),
        (
            "caller value made of connectors",
            filled("caller is ", "and ", "1"),
        ),
        (
            "long assignee name ending in a digit",
            filled("assigned to ", "a ", "1"),
        ),
        (
            "repeated field prefixes",
            filled("", "assignment group is ", ""),
        ),
        (
            "unterminated quoted content",
            filled("about \"", "x ", "\"!"),
        ),
        (
            "description followed by whitespace and a quote",
            filled("description contains ", " ", "x\""),
        ),
        ("only double quotes", "\"".repeat(MAX_QUERY_LENGTH)),
        ("only single quotes", "'".repeat(MAX_QUERY_LENGTH)),
        ("only spaces", " ".repeat(MAX_QUERY_LENGTH)),
        (
            "mixed whitespace content keywords",
            filled("", "with\t\n ", ""),
        ),
        (
            "repeated quoted content keywords",
            filled("", "includes \" ", "!"),
        ),
        (
            "repeated date prefixes",
            filled("", "created before January ", ""),
        ),
        (
            "repeated relative ranges missing a unit",
            filled("", "updated in the last 7 ", ""),
        ),
        ("repeated priority codes", filled("", "p1 ", "")),
        ("one long word", filled("about ", "a", "")),
        (
            "repeated connectors after content keyword",
            filled("about ", "or ", "!"),
        ),
    ];
    // Warm up so the measurement reflects steady-state cost.
    parse("open incidents about SAP assigned to John Smith created today");
    for (label, query) in queries {
        assert!(query.len() <= MAX_QUERY_LENGTH, "{label}");
        assert!(query.len() > MAX_QUERY_LENGTH - 25, "{label}");
        let started = Instant::now();
        let result = parse_natural_language(&query, "incident");
        let elapsed = started.elapsed();
        assert!(result.is_ok(), "{label}");
        assert!(elapsed < PROMPT_COMPLETION, "{label}: {elapsed:?}");
    }
}

#[test]
fn still_parses_meaningful_content_inside_a_maximum_length_adversarial_query() {
    let query = filled("assigned to Jane Doe and P1 about", " ", "SAP");
    assert_eq!(query.len(), MAX_QUERY_LENGTH);
    let started = Instant::now();
    let result = parsed(&query);
    assert!(started.elapsed() < PROMPT_COMPLETION);
    assert_eq!(
        result,
        pair(
            "assigned_to.nameLIKEJane Doe^priority=1^short_descriptionLIKESAP^ORdescriptionLIKESAP",
            ""
        )
    );
}

#[test]
fn whitespace_only_adversarial_query_matches_nothing() {
    // Parser-level half of the MCP test "answers a maximum-length adversarial
    // query promptly without calling ServiceNow": nothing to send.
    let result = parse(&format!("with{}!", " ".repeat(MAX_QUERY_LENGTH - 5)));
    assert_eq!(result.encoded_query, "");
    assert!(result.matched_patterns.is_empty());
}
