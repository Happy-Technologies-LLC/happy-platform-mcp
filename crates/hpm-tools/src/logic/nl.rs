//! Natural language to ServiceNow encoded query parser. Ports
//! `src/natural-language.js`.
//!
//! The input is validated (at most [`MAX_NATURAL_LANGUAGE_QUERY_LENGTH`]
//! UTF-16 code units) before any parsing. It is then tokenized in a single
//! linear pass, and every supported pattern is matched over that token list
//! with fixed lookahead or a single forward scan, so parsing time grows
//! linearly with the bounded input. The parser performs no lookups: assignee,
//! caller and group names become `LIKE` conditions resolved by ServiceNow.

use super::js;
use serde_json::{Value, json};

/// Maximum accepted natural language query length, in UTF-16 code units.
pub const MAX_NATURAL_LANGUAGE_QUERY_LENGTH: usize = 2048;

/// Table used when the caller names none.
pub const DEFAULT_TABLE: &str = "incident";

/// Table-specific state conditions; unknown tables use the incident mapping.
fn state_condition(table: &str, state: &str) -> Option<&'static str> {
    match table {
        "change_request" => match state {
            "new" => Some("state=-5"),
            "assess" => Some("state=-4"),
            "authorize" => Some("state=-3"),
            "scheduled" => Some("state=-2"),
            "implement" => Some("state=-1"),
            "review" => Some("state=0"),
            "closed" => Some("state=3"),
            "canceled" => Some("state=4"),
            // Negative states are open
            "open" => Some("state<0"),
            "active" => Some("active=true"),
            _ => None,
        },
        "problem" => match state {
            "new" => Some("state=1"),
            "assessed" => Some("state=2"),
            "root cause analysis" => Some("state=3"),
            "fix in progress" => Some("state=4"),
            "resolved" => Some("state=6"),
            "closed" => Some("state=7"),
            "open" => Some("state=1^ORstate=2^ORstate=3^ORstate=4"),
            "active" => Some("active=true"),
            _ => None,
        },
        _ => match state {
            "new" => Some("state=1"),
            "in progress" => Some("state=2"),
            "on hold" => Some("state=3"),
            "resolved" => Some("state=6"),
            "closed" => Some("state=7"),
            "canceled" => Some("state=8"),
            // New, In Progress, or On Hold
            "open" => Some("state=1^ORstate=2^ORstate=3"),
            "active" => Some("active=true"),
            _ => None,
        },
    }
}

/// Priority mappings (consistent across tables). `word` is lowercase.
fn priority_mapping(word: &str) -> Option<&'static str> {
    match word {
        "critical" | "1" => Some("1"),
        "high" | "2" => Some("2"),
        "moderate" | "3" => Some("3"),
        "low" | "4" => Some("4"),
        "planning" | "5" => Some("5"),
        _ => None,
    }
}

/// Impact and urgency mappings. `word` is lowercase.
fn severity_mapping(word: &str) -> &'static str {
    match word {
        "high" => "1",
        "medium" => "2",
        _ => "3",
    }
}

const DATE_FIELDS: &[&str] = &["created", "opened", "updated", "modified", "closed"];
const PRIORITY_LEVELS: &[&str] = &["critical", "high", "moderate", "low", "planning"];
const SEVERITY_LEVELS: &[&str] = &["high", "medium", "low"];
const MY_RECORD_WORDS: &[&str] = &["incidents", "problems", "changes", "tickets"];
const RELATIVE_DAYS: &[&str] = &["today", "yesterday"];
const CONNECTORS: &[&str] = &["and", "or"];
const RANGE_UNITS: &[&str] = &["day", "days", "week", "weeks", "month", "months"];
const SINGLE_WORD_STATES: &[&str] = &["new", "open", "active", "resolved", "closed", "canceled"];
const CONTENT_KEYWORDS: &[&str] = &["about", "containing", "with", "include", "includes"];
const DESCRIPTION_VERBS: &[&str] = &["contains", "includes"];
const NUMBER_OPERATORS: &[&str] = &["is", "equal", "equals"];
const FILLER_WORDS: &[&str] = &["and", "or", "with", "in", "the", "a", "an"];

const ASSIGNEE_TERMINATORS: &[&str] = &["and", "or", "with", "created", "opened", "updated"];
const FIELD_VALUE_TERMINATORS: &[&str] = &["and", "or", "with"];
const CONTENT_TERMINATORS: &[&str] = &["and", "or", "in", "with", "created", "opened", "assigned"];
const DESCRIPTION_TERMINATORS: &[&str] = &["and", "or"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Space,
    Symbol,
}

/// A token borrows its text from the query (or is the static " " left by a
/// removed match). Word tokens are ASCII, so case-insensitive comparison
/// stands in for JavaScript's `lower` field.
#[derive(Debug, Clone, Copy)]
struct Token<'q> {
    kind: Kind,
    text: &'q str,
    /// Whitespace or symbol containing a character no value may carry.
    unsafe_value: bool,
}

/// `[A-Za-z0-9_]`, matching regex `\w`.
fn is_word_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Characters that may never appear in a captured value: the encoded query
/// separator "^", C0/C1 control characters (including CR, LF and tab) and the
/// Unicode line/paragraph separators.
fn is_unsafe_value_character(c: char) -> bool {
    matches!(c, '^' | '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}' | '\u{2028}' | '\u{2029}')
}

/// Split the query into maximal word runs, maximal whitespace runs, and single
/// other characters. One linear pass.
fn tokenize(query: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut chars = query.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        if is_word_character(c) {
            while let Some(&(at, next)) = chars.peek() {
                if !is_word_character(next) {
                    break;
                }
                end = at + next.len_utf8();
                chars.next();
            }
            tokens.push(Token {
                kind: Kind::Word,
                text: &query[start..end],
                unsafe_value: false,
            });
        } else if js::is_js_whitespace(c) {
            let mut unsafe_value = is_unsafe_value_character(c);
            while let Some(&(at, next)) = chars.peek() {
                if !js::is_js_whitespace(next) {
                    break;
                }
                unsafe_value |= is_unsafe_value_character(next);
                end = at + next.len_utf8();
                chars.next();
            }
            tokens.push(Token {
                kind: Kind::Space,
                text: &query[start..end],
                unsafe_value,
            });
        } else {
            tokens.push(Token {
                kind: Kind::Symbol,
                text: &query[start..end],
                unsafe_value: is_unsafe_value_character(c),
            });
        }
    }
    tokens
}

type Tokens<'q> = [Token<'q>];

fn at<'a, 'q>(tokens: &'a Tokens<'q>, index: usize) -> Option<&'a Token<'q>> {
    tokens.get(index)
}

fn is_word(token: Option<&Token<'_>>, word: &str) -> bool {
    token.is_some_and(|t| t.kind == Kind::Word && t.text.eq_ignore_ascii_case(word))
}

fn is_word_in(token: Option<&Token<'_>>, words: &[&str]) -> bool {
    token.is_some_and(|t| {
        t.kind == Kind::Word && words.iter().any(|w| t.text.eq_ignore_ascii_case(w))
    })
}

fn is_word_like(token: Option<&Token<'_>>, pattern: fn(&str) -> bool) -> bool {
    token.is_some_and(|t| t.kind == Kind::Word && pattern(t.text))
}

fn is_space(token: Option<&Token<'_>>) -> bool {
    token.is_some_and(|t| t.kind == Kind::Space)
}

/// Whitespace that may be copied into a captured value.
fn is_value_space(token: Option<&Token<'_>>) -> bool {
    token.is_some_and(|t| t.kind == Kind::Space && !t.unsafe_value)
}

fn is_symbol(token: Option<&Token<'_>>, symbol: &str) -> bool {
    token.is_some_and(|t| t.kind == Kind::Symbol && t.text == symbol)
}

fn is_quote(token: Option<&Token<'_>>) -> bool {
    is_symbol(token, "\"") || is_symbol(token, "'")
}

/// `/^[Pp][1-5]$/`
fn priority_code(text: &str) -> bool {
    matches!(text.as_bytes(), [b'P' | b'p', b'1'..=b'5'])
}

/// `/^[A-Za-z]+$/`
fn alpha_word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphabetic())
}

/// `/^[A-Za-z0-9]+$/`
fn alphanumeric_word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `/^[0-9]+$/`
fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `/^[0-9]{1,2}$/`
fn day_of_month(text: &str) -> bool {
    (1..=2).contains(&text.len()) && digits(text)
}

/// `/^[0-9]{4}$/`
fn year(text: &str) -> bool {
    text.len() == 4 && digits(text)
}

/// `/^[A-Za-z]{3}[0-9]{7}$/`
fn record_number(text: &str) -> bool {
    text.len() == 10 && alpha_word(&text[..3]) && digits(&text[3..])
}

fn text_of(tokens: &Tokens<'_>, start: usize, end: usize) -> String {
    tokens[start..end].iter().map(|t| t.text).collect()
}

fn date_field(token: &Token<'_>) -> String {
    let lower = token.text.to_ascii_lowercase();
    if lower == "opened" {
        "sys_created_on".to_owned()
    } else {
        format!("{lower}_on")
    }
}

struct Match {
    start: usize,
    end: usize,
    condition: String,
}

/// Return the leftmost match produced by `match_at` (which inspects a fixed
/// number of tokens starting at an index).
fn find_first(tokens: &Tokens<'_>, match_at: impl Fn(usize) -> Option<Match>) -> Option<Match> {
    (0..tokens.len()).find_map(match_at)
}

struct Capture {
    start: usize,
    end: usize,
    value: String,
}

struct CaptureRules<'r> {
    /// Index of the first value token when a prefix starts at the index.
    capture_prefix: &'r dyn Fn(usize) -> Option<usize>,
    accepts: fn(&Token<'_>) -> bool,
    terminators: &'r [&'r str],
    quoted: bool,
}

/// Leftmost "<prefix> <value>" match where the value is a lazily captured run
/// of accepted tokens ending at the end of the query, or before whitespace and
/// a terminator word (consumed with the match). A value must contain a word,
/// cannot start with a logical connector ("and"/"or") and never includes an
/// unsafe token ("^", control characters, line breaks).
///
/// A failed value scan is never repeated for later prefixes whose value starts
/// inside the already rejected span, so total work is linear in the token count.
fn find_capture(tokens: &Tokens<'_>, rules: &CaptureRules<'_>) -> Option<Capture> {
    let mut rejected_through: Option<usize> = None;

    for index in 0..tokens.len() {
        let Some(value_start) = (rules.capture_prefix)(index) else {
            continue;
        };
        if rejected_through.is_some_and(|through| value_start <= through) {
            continue;
        }

        let mut has_word = false;
        let mut position = value_start;
        loop {
            let token = match at(tokens, position) {
                Some(token) if !token.unsafe_value && (rules.accepts)(token) => token,
                _ => {
                    rejected_through = Some(position);
                    break;
                }
            };
            if token.kind == Kind::Word {
                if !has_word && is_word_in(Some(token), CONNECTORS) {
                    rejected_through = Some(position);
                    break;
                }
                has_word = true;
            }
            if has_word {
                let mut next = position + 1;
                if rules.quoted && is_quote(at(tokens, next)) {
                    next += 1;
                }
                let end = if next == tokens.len() {
                    Some(next)
                } else if is_space(at(tokens, next)) {
                    if next + 1 == tokens.len() {
                        Some(next + 1)
                    } else if is_word_in(at(tokens, next + 1), rules.terminators) {
                        Some(next + 2)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(end) = end {
                    return Some(Capture {
                        start: index,
                        end,
                        value: js::trim(&text_of(tokens, value_start, position + 1)).to_owned(),
                    });
                }
            }
            position += 1;
        }
    }
    None
}

/// "<keyword...> (is|=) <value>" prefix for field value patterns.
fn field_value_prefix(tokens: &Tokens<'_>, keywords: &[&str], index: usize) -> Option<usize> {
    let mut position = index;
    for keyword in keywords {
        if position > index {
            if !is_space(at(tokens, position)) {
                return None;
            }
            position += 1;
        }
        if !is_word(at(tokens, position), keyword) {
            return None;
        }
        position += 1;
    }
    if !is_space(at(tokens, position)) {
        return None;
    }
    let operator = at(tokens, position + 1);
    if !is_word(operator, "is") && !is_symbol(operator, "=") {
        return None;
    }
    if !is_space(at(tokens, position + 2)) {
        return None;
    }
    Some(position + 3)
}

fn accepts_name(token: &Token<'_>) -> bool {
    token.kind == Kind::Space || (token.kind == Kind::Word && alpha_word(token.text))
}

fn accepts_content(token: &Token<'_>) -> bool {
    token.kind == Kind::Space || (token.kind == Kind::Word && alphanumeric_word(token.text))
}

fn accepts_unquoted(token: &Token<'_>) -> bool {
    !is_quote(Some(token))
}

fn field_value_pattern(tokens: &Tokens<'_>, keywords: &[&str], field_name: &str) -> Option<Match> {
    let capture = find_capture(
        tokens,
        &CaptureRules {
            capture_prefix: &|index| field_value_prefix(tokens, keywords, index),
            accepts: accepts_name,
            terminators: FIELD_VALUE_TERMINATORS,
            quoted: false,
        },
    )?;
    Some(Match {
        start: capture.start,
        end: capture.end,
        condition: format!("{field_name}LIKE{}", capture.value),
    })
}

fn explicit_date_pattern(tokens: &Tokens<'_>, keyword: &str, operator: &str) -> Option<Match> {
    find_first(tokens, |index| {
        let field = at(tokens, index)?;
        if !is_word_in(Some(field), DATE_FIELDS)
            || !is_space(at(tokens, index + 1))
            || !is_word(at(tokens, index + 2), keyword)
            || !is_space(at(tokens, index + 3))
            || !is_word_like(at(tokens, index + 4), alpha_word)
            || !is_value_space(at(tokens, index + 5))
            || !is_word_like(at(tokens, index + 6), day_of_month)
        {
            return None;
        }

        let mut end = index + 7;
        let mut year_start = end;
        if is_symbol(at(tokens, year_start), ",") {
            year_start += 1;
        }
        if is_value_space(at(tokens, year_start)) && is_word_like(at(tokens, year_start + 1), year)
        {
            end = year_start + 2;
        }

        Some(Match {
            start: index,
            end,
            condition: format!(
                "{}{operator}{}",
                date_field(field),
                text_of(tokens, index + 4, end)
            ),
        })
    })
}

const ASSIGNED_TO_ME: &str = "assigned_to=javascript:gs.getUserID()";
const RECENT_CONDITION: &str = "sys_created_on>javascript:gs.daysAgo(7)";

/// Supported patterns in evaluation order (highest precedence first). Each
/// pattern is applied at most once, to its leftmost match in the text left
/// unmatched by earlier patterns.
const PATTERNS: &[&str] = &[
    "number",
    "assigned-to-me",
    "unassigned",
    "assigned-to-name",
    "relative-day",
    "relative-range",
    "before-date",
    "after-date",
    "priority-level",
    "priority-code",
    "priority-value",
    "caller",
    "category",
    "assignment-group",
    "impact",
    "urgency",
    "recent",
    "state",
    "description-contains",
    "content",
];

fn find_pattern(id: &str, tokens: &Tokens<'_>, table: &str) -> Option<Match> {
    match id {
        "number" => find_first(tokens, |index| {
            let operator = at(tokens, index + 2);
            if !is_word(at(tokens, index), "number")
                || !is_space(at(tokens, index + 1))
                || !(is_word_in(operator, NUMBER_OPERATORS) || is_symbol(operator, "="))
                || !is_space(at(tokens, index + 3))
                || !is_word_like(at(tokens, index + 4), record_number)
            {
                return None;
            }
            Some(Match {
                start: index,
                end: index + 5,
                condition: format!("number={}", tokens[index + 4].text),
            })
        }),
        "assigned-to-me" => find_first(tokens, |index| {
            if is_word(at(tokens, index), "assigned")
                && is_space(at(tokens, index + 1))
                && is_word(at(tokens, index + 2), "to")
                && is_space(at(tokens, index + 3))
                && is_word(at(tokens, index + 4), "me")
            {
                return Some(Match {
                    start: index,
                    end: index + 5,
                    condition: ASSIGNED_TO_ME.to_owned(),
                });
            }
            if is_word(at(tokens, index), "my")
                && is_space(at(tokens, index + 1))
                && is_word_in(at(tokens, index + 2), MY_RECORD_WORDS)
            {
                return Some(Match {
                    start: index,
                    end: index + 3,
                    condition: ASSIGNED_TO_ME.to_owned(),
                });
            }
            None
        }),
        "unassigned" => find_first(tokens, |index| {
            is_word(at(tokens, index), "unassigned").then(|| Match {
                start: index,
                end: index + 1,
                condition: "assigned_toISEMPTY".to_owned(),
            })
        }),
        "assigned-to-name" => {
            let capture = find_capture(
                tokens,
                &CaptureRules {
                    capture_prefix: &|index| {
                        (is_word(at(tokens, index), "assigned")
                            && is_space(at(tokens, index + 1))
                            && is_word(at(tokens, index + 2), "to")
                            && is_space(at(tokens, index + 3)))
                        .then_some(index + 4)
                    },
                    accepts: accepts_name,
                    terminators: ASSIGNEE_TERMINATORS,
                    quoted: false,
                },
            )?;
            Some(Match {
                start: capture.start,
                end: capture.end,
                condition: format!("assigned_to.nameLIKE{}", capture.value),
            })
        }
        "relative-day" => find_first(tokens, |index| {
            let field = at(tokens, index)?;
            let day = at(tokens, index + 2);
            if !is_word_in(Some(field), DATE_FIELDS)
                || !is_space(at(tokens, index + 1))
                || !is_word_in(day, RELATIVE_DAYS)
            {
                return None;
            }
            let days = if is_word(day, "today") { 0 } else { 1 };
            Some(Match {
                start: index,
                end: index + 3,
                condition: format!("{}>javascript:gs.daysAgoStart({days})", date_field(field)),
            })
        }),
        "relative-range" => find_first(tokens, |index| {
            let field = at(tokens, index)?;
            if !is_word_in(Some(field), DATE_FIELDS) || !is_space(at(tokens, index + 1)) {
                return None;
            }
            let mut position = index + 2;
            if is_word(at(tokens, position), "in") && is_space(at(tokens, position + 1)) {
                position += 2;
            }
            if is_word(at(tokens, position), "the") && is_space(at(tokens, position + 1)) {
                position += 2;
            }
            let amount = at(tokens, position + 2);
            let unit = at(tokens, position + 4);
            if !is_word(at(tokens, position), "last")
                || !is_space(at(tokens, position + 1))
                || !is_word_like(amount, digits)
                || !is_space(at(tokens, position + 3))
                || !is_word_in(unit, RANGE_UNITS)
            {
                return None;
            }
            let (amount, unit) = (amount?, unit?.text.to_ascii_lowercase());

            // parseInt of a decimal digit run: the correctly rounded double.
            let mut days: f64 = amount.text.parse().unwrap_or(f64::NAN);
            if unit.starts_with("week") {
                days *= 7.0;
            }
            if unit.starts_with("month") {
                days *= 30.0;
            }

            Some(Match {
                start: index,
                end: position + 5,
                condition: format!(
                    "{}>javascript:gs.daysAgo({})",
                    date_field(field),
                    js::number_to_string(days)
                ),
            })
        }),
        "before-date" => explicit_date_pattern(tokens, "before", "<"),
        "after-date" => explicit_date_pattern(tokens, "after", ">"),
        "priority-level" => find_first(tokens, |index| {
            let level = at(tokens, index)?;
            if !is_word_in(Some(level), PRIORITY_LEVELS)
                || !is_space(at(tokens, index + 1))
                || !is_word(at(tokens, index + 2), "priority")
            {
                return None;
            }
            let value = priority_mapping(&level.text.to_ascii_lowercase())?;
            Some(Match {
                start: index,
                end: index + 3,
                condition: format!("priority={value}"),
            })
        }),
        "priority-code" => find_first(tokens, |index| {
            let token = at(tokens, index)?;
            is_word_like(Some(token), priority_code).then(|| Match {
                start: index,
                end: index + 1,
                condition: format!("priority={}", &token.text[1..2]),
            })
        }),
        "priority-value" => find_first(tokens, |index| {
            let level = at(tokens, index + 2)?;
            if !is_word(at(tokens, index), "priority")
                || !is_space(at(tokens, index + 1))
                || level.kind != Kind::Word
            {
                return None;
            }
            let value = priority_mapping(&level.text.to_ascii_lowercase())?;
            Some(Match {
                start: index,
                end: index + 3,
                condition: format!("priority={value}"),
            })
        }),
        "caller" => field_value_pattern(tokens, &["caller"], "caller_id.name"),
        "category" => field_value_pattern(tokens, &["category"], "category"),
        "assignment-group" => {
            field_value_pattern(tokens, &["assignment", "group"], "assignment_group.name")
        }
        "impact" | "urgency" => find_first(tokens, |index| {
            let level = at(tokens, index)?;
            if !is_word_in(Some(level), SEVERITY_LEVELS)
                || !is_space(at(tokens, index + 1))
                || !is_word(at(tokens, index + 2), id)
            {
                return None;
            }
            Some(Match {
                start: index,
                end: index + 3,
                condition: format!(
                    "{id}={}",
                    severity_mapping(&level.text.to_ascii_lowercase())
                ),
            })
        }),
        "recent" => find_first(tokens, |index| {
            let token = at(tokens, index);
            if is_word(token, "recent") || is_word(token, "new") {
                return Some(Match {
                    start: index,
                    end: index + 1,
                    condition: RECENT_CONDITION.to_owned(),
                });
            }
            if is_word(token, "recently")
                && is_space(at(tokens, index + 1))
                && is_word(at(tokens, index + 2), "created")
            {
                return Some(Match {
                    start: index,
                    end: index + 3,
                    condition: RECENT_CONDITION.to_owned(),
                });
            }
            None
        }),
        "state" => find_first(tokens, |index| {
            let token = at(tokens, index);
            let (state, end) = if is_word_in(token, SINGLE_WORD_STATES) {
                (token?.text.to_ascii_lowercase(), index + 1)
            } else if is_word(token, "in")
                && is_space(at(tokens, index + 1))
                && is_word(at(tokens, index + 2), "progress")
            {
                ("in progress".to_owned(), index + 3)
            } else if is_word(token, "on")
                && is_space(at(tokens, index + 1))
                && is_word(at(tokens, index + 2), "hold")
            {
                ("on hold".to_owned(), index + 3)
            } else {
                return None;
            };
            let condition = state_condition(table, &state)
                .map_or_else(|| format!("state={state}"), str::to_owned);
            Some(Match {
                start: index,
                end,
                condition,
            })
        }),
        "description-contains" => {
            let capture = find_capture(
                tokens,
                &CaptureRules {
                    capture_prefix: &|index| {
                        if !is_word(at(tokens, index), "description")
                            || !is_space(at(tokens, index + 1))
                            || !is_word_in(at(tokens, index + 2), DESCRIPTION_VERBS)
                            || !is_space(at(tokens, index + 3))
                        {
                            return None;
                        }
                        Some(if is_quote(at(tokens, index + 4)) {
                            index + 5
                        } else {
                            index + 4
                        })
                    },
                    accepts: accepts_unquoted,
                    terminators: DESCRIPTION_TERMINATORS,
                    quoted: true,
                },
            )?;
            Some(Match {
                start: capture.start,
                end: capture.end,
                condition: format!("descriptionLIKE{}", capture.value),
            })
        }
        "content" => {
            let capture = find_capture(
                tokens,
                &CaptureRules {
                    capture_prefix: &|index| {
                        if !is_word_in(at(tokens, index), CONTENT_KEYWORDS)
                            || !is_space(at(tokens, index + 1))
                        {
                            return None;
                        }
                        Some(if is_quote(at(tokens, index + 2)) {
                            index + 3
                        } else {
                            index + 2
                        })
                    },
                    accepts: accepts_content,
                    terminators: CONTENT_TERMINATORS,
                    quoted: true,
                },
            )?;
            Some(Match {
                start: capture.start,
                end: capture.end,
                condition: format!(
                    "short_descriptionLIKE{0}^ORdescriptionLIKE{0}",
                    capture.value
                ),
            })
        }
        _ => None,
    }
}

const MERGED_SPACE: Token<'static> = Token {
    kind: Kind::Space,
    text: " ",
    unsafe_value: false,
};

/// Replace tokens[start, end) with one space, merge it into neighbouring
/// whitespace and trim the token list ends.
fn remove_match(tokens: &mut Vec<Token<'_>>, start: usize, end: usize) {
    let mut from = start;
    let mut to = end;
    if from > 0 && is_space(at(tokens, from - 1)) {
        from -= 1;
    }
    if is_space(at(tokens, to)) {
        to += 1;
    }
    tokens.splice(from..to, [MERGED_SPACE]);

    if is_space(tokens.last()) {
        tokens.pop();
    }
    if is_space(tokens.first()) {
        tokens.remove(0);
    }
}

/// Remaining text with filler words removed and whitespace collapsed.
fn describe_unmatched(tokens: &Tokens<'_>) -> String {
    let mut text = String::new();
    let mut separated = false;
    for token in tokens {
        if token.kind == Kind::Space || is_word_in(Some(token), FILLER_WORDS) {
            separated = true;
            continue;
        }
        if separated && !text.is_empty() {
            text.push(' ');
        }
        text.push_str(token.text);
        separated = false;
    }
    text
}

/// One applied pattern: its id, the text it consumed and its condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPattern {
    pub pattern: &'static str,
    pub matched: String,
    pub condition: String,
}

/// `{ encodedQuery, matchedPatterns, unmatchedText, suggestions }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NaturalLanguageResult {
    pub encoded_query: String,
    pub matched_patterns: Vec<MatchedPattern>,
    pub unmatched_text: String,
    pub suggestions: Vec<String>,
}

impl NaturalLanguageResult {
    /// The JavaScript result object.
    pub fn to_json(&self) -> Value {
        json!({
            "encodedQuery": self.encoded_query,
            "matchedPatterns": self.matched_patterns.iter().map(|m| json!({
                "pattern": m.pattern,
                "matched": m.matched,
                "condition": m.condition,
            })).collect::<Vec<_>>(),
            "unmatchedText": self.unmatched_text,
            "suggestions": self.suggestions,
        })
    }
}

/// `parseNaturalLanguage` rejected its input. The messages match the
/// JavaScript `TypeError` and `RangeError`, and never echo the query.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NaturalLanguageError {
    /// JavaScript `TypeError`.
    #[error("Natural language query must be a string")]
    NotAString,
    /// JavaScript `RangeError`; `received` is the length in UTF-16 code units.
    #[error(
        "Natural language query must be at most {MAX_NATURAL_LANGUAGE_QUERY_LENGTH} characters (received {received})"
    )]
    TooLong { received: usize },
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

/// `parseNaturalLanguage(query, table)` for an untyped argument; `table`
/// `None` is the JavaScript default `'incident'`.
pub fn parse_natural_language_value(
    query: Option<&Value>,
    table: Option<&str>,
) -> Result<NaturalLanguageResult, NaturalLanguageError> {
    match query {
        Some(Value::String(query)) => parse_natural_language(query, table.unwrap_or(DEFAULT_TABLE)),
        _ => Err(NaturalLanguageError::NotAString),
    }
}

/// Parse a natural language query into a ServiceNow encoded query.
pub fn parse_natural_language(
    query: &str,
    table: &str,
) -> Result<NaturalLanguageResult, NaturalLanguageError> {
    if js::utf16_len_exceeds(query, MAX_NATURAL_LANGUAGE_QUERY_LENGTH) {
        return Err(NaturalLanguageError::TooLong {
            received: js::utf16_len(query),
        });
    }

    if query.is_empty() {
        return Ok(NaturalLanguageResult {
            encoded_query: String::new(),
            matched_patterns: Vec::new(),
            unmatched_text: String::new(),
            suggestions: owned(&["Please provide a valid query string"]),
        });
    }

    let mut tokens = tokenize(query);
    let mut conditions: Vec<String> = Vec::new();
    let mut matched_patterns = Vec::new();

    for &pattern in PATTERNS {
        if let Some(found) = find_pattern(pattern, &tokens, table) {
            matched_patterns.push(MatchedPattern {
                pattern,
                matched: text_of(&tokens, found.start, found.end),
                condition: found.condition.clone(),
            });
            conditions.push(found.condition);
            remove_match(&mut tokens, found.start, found.end);
        }
    }

    // Use OR if the remaining text contains "or", otherwise use AND (default)
    let has_or = tokens.iter().any(|token| is_word(Some(token), "or"));
    let encoded_query = conditions.join(if has_or { "^OR" } else { "^" });
    let unmatched_text = describe_unmatched(&tokens);

    // If no patterns matched, return original query (might be encoded query already)
    if conditions.is_empty() {
        if query.contains(['=', '^']) {
            return Ok(NaturalLanguageResult {
                encoded_query: query.to_owned(),
                matched_patterns: Vec::new(),
                unmatched_text: String::new(),
                suggestions: owned(&["Using query as-is (appears to be encoded query format)"]),
            });
        }
        return Ok(NaturalLanguageResult {
            encoded_query: String::new(),
            matched_patterns: Vec::new(),
            unmatched_text: query.to_owned(),
            suggestions: owned(&[
                "No patterns matched. Supported patterns:",
                "- Priority: \"high priority\", \"P1\", \"priority 2\"",
                "- Assignment: \"assigned to me\", \"unassigned\", \"assigned to John\"",
                "- State: \"new\", \"open\", \"closed\", \"in progress\"",
                "- Dates: \"created today\", \"last 7 days\", \"recent\"",
                "- Content: \"about SAP\", \"containing error\"",
                "Or use ServiceNow encoded query format: field=value^field2=value2",
            ]),
        });
    }

    // Generate suggestions for unmatched text
    let suggestions = if js::utf16_len(&unmatched_text) > 3 {
        vec![
            format!("Unrecognized: \"{unmatched_text}\""),
            "Try using encoded query format: field=value^field2=value2".to_owned(),
            "Supported patterns: priority (P1-P5), state (new/open/closed), assigned to me/unassigned, recent, dates".to_owned(),
        ]
    } else {
        owned(&["Query parsed successfully"])
    };

    Ok(NaturalLanguageResult {
        encoded_query,
        matched_patterns,
        unmatched_text,
        suggestions,
    })
}

/// `testParser` example queries.
pub const TEST_QUERIES: &[&str] = &[
    "find all P1 incidents",
    "show recent problems assigned to me",
    "high priority changes created last week",
    "open incidents about SAP",
    "unassigned P2 incidents",
    "incidents created today with high priority",
    "closed problems assigned to John Smith",
    "critical incidents opened in the last 30 days",
    "show me my active tickets",
    "new incidents with high impact and high urgency",
    "incidents containing database error",
    "P1 incidents assigned to Network Team",
    "incidents created after January 1",
    "resolved incidents about authentication",
];

/// Test the parser with the example queries.
pub fn test_parser(
    table: &str,
) -> Vec<(
    &'static str,
    Result<NaturalLanguageResult, NaturalLanguageError>,
)> {
    TEST_QUERIES
        .iter()
        .map(|query| (*query, parse_natural_language(query, table)))
        .collect()
}

/// One entry of `getSupportedPatterns()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedPattern {
    pub key: &'static str,
    pub examples: &'static [&'static str],
    pub encoded_query: &'static str,
}

/// `getSupportedPatterns()`, in JavaScript key order.
pub const SUPPORTED_PATTERNS: &[SupportedPattern] = &[
    SupportedPattern {
        key: "priority",
        examples: &["high priority", "P1", "priority 2", "critical priority"],
        encoded_query: "priority=1",
    },
    SupportedPattern {
        key: "assignment",
        examples: &[
            "assigned to me",
            "unassigned",
            "assigned to John Smith",
            "my incidents",
        ],
        encoded_query: "assigned_to=javascript:gs.getUserID() or assigned_toISEMPTY",
    },
    SupportedPattern {
        key: "state",
        examples: &["new", "open", "active", "in progress", "closed", "resolved"],
        encoded_query: "state=1 (varies by table)",
    },
    SupportedPattern {
        key: "dates",
        examples: &[
            "created today",
            "last 7 days",
            "recent",
            "opened yesterday",
            "updated last week",
        ],
        encoded_query: "sys_created_on>javascript:gs.daysAgo(7)",
    },
    SupportedPattern {
        key: "content",
        examples: &[
            "about SAP",
            "containing error",
            "description contains authentication",
        ],
        encoded_query: "short_descriptionLIKESAP",
    },
    SupportedPattern {
        key: "impact",
        examples: &["high impact", "medium impact", "low impact"],
        encoded_query: "impact=1",
    },
    SupportedPattern {
        key: "urgency",
        examples: &["high urgency", "medium urgency", "low urgency"],
        encoded_query: "urgency=1",
    },
    SupportedPattern {
        key: "number",
        examples: &["number is INC0012345"],
        encoded_query: "number=INC0012345",
    },
    SupportedPattern {
        key: "caller",
        examples: &["caller is John Smith"],
        encoded_query: "caller_id.nameLIKEJohn Smith",
    },
    SupportedPattern {
        key: "category",
        examples: &["category is Software"],
        encoded_query: "categoryLIKESoftware",
    },
    SupportedPattern {
        key: "assignmentGroup",
        examples: &["assignment group is Network Team"],
        encoded_query: "assignment_group.nameLIKENetwork Team",
    },
    SupportedPattern {
        key: "combining",
        examples: &[
            "high priority and assigned to me",
            "P1 or P2 incidents",
            "recent and unassigned",
        ],
        encoded_query: "Use \"and\" for ^ operator, \"or\" for ^OR operator",
    },
];

/// `getSupportedPatterns()` as the JavaScript object.
pub fn supported_patterns_json() -> Value {
    Value::Object(
        SUPPORTED_PATTERNS
            .iter()
            .map(|entry| {
                (
                    entry.key.to_owned(),
                    json!({ "examples": entry.examples, "encodedQuery": entry.encoded_query }),
                )
            })
            .collect(),
    )
}
