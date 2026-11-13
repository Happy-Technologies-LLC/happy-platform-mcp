//! Pure normalizers for the lane A tools: every table the call reads or writes,
//! with written fields, written script values (D-05 rule 5, escalated centrally),
//! encoded-query predicate/ordering fields and `javascript:` query operands
//! (rejected centrally unless allowlisted, RFC §3.2).
//!
//! Writes go through [`Operation::on_table`], so a write to an execute-class,
//! admin or unknown table escalates (table rule) before policy sees it.

use hpm_types::operation::{Action, Operation, OperationSet, Subject, is_script_value};
use hpm_types::tool::{NormalizeInput, ToolError};
use serde_json::Value;

use crate::logic::bulk_limits::{validate_batch_create_operations, validate_batch_updates};
use crate::logic::nl::parse_natural_language_value;

/// Fields, ordering and script operands found in a ServiceNow encoded query.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct QueryInfo {
    pub predicate_fields: Vec<String>,
    pub ordering_fields: Vec<String>,
    /// Text after each `javascript:` (case-insensitive), whitespace-collapsed.
    pub script_operands: Vec<String>,
}

const SCRIPT_PREFIX: &str = "javascript:";

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

fn csv(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_str)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn order_field(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_str)
        .map(|s| s.trim_start_matches('-').trim())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_owned()])
        .unwrap_or_default()
}

fn contains_script(value: &Value) -> bool {
    match value {
        Value::String(s) => is_script_value(s),
        Value::Array(items) => items.iter().any(contains_script),
        Value::Object(map) => map.values().any(contains_script),
        _ => false,
    }
}

fn written(data: Option<&Value>) -> (Vec<String>, Vec<String>) {
    let Some(map) = data.and_then(Value::as_object) else {
        return (Vec::new(), Vec::new());
    };
    let fields = map.keys().cloned().collect();
    let scripts = map
        .iter()
        .filter(|(_, v)| contains_script(v))
        .map(|(k, _)| k.clone())
        .collect();
    (fields, scripts)
}

fn table_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidParams(format!("{key} must be a string")))
}

fn base(tool: &'static str, action: Action, input: &NormalizeInput<'_>) -> Operation {
    Operation::new(Subject::Tool(tool), action)
        .instance_id(input.instance.map(|i| i.instance_id.clone()))
}

fn one(op: Operation) -> Result<OperationSet, ToolError> {
    Ok(OperationSet(vec![op]))
}

fn read_query(
    tool: &'static str,
    table: &str,
    query: Option<&str>,
    fields: Option<&Value>,
    order_by: Option<&Value>,
    input: &NormalizeInput<'_>,
) -> Operation {
    let info = query.map(analyze_query).unwrap_or_default();
    let mut ordering = info.ordering_fields;
    for f in order_field(order_by) {
        push_unique(&mut ordering, &f);
    }
    base(tool, Action::Read, input)
        .on_table(table, input.classification)
        .returned_fields(csv(fields))
        .predicate_fields(info.predicate_fields)
        .ordering_fields(ordering)
        .query_script_operands(info.script_operands)
}

pub fn query_table(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let table = table_arg(args, "table_name")?;
    one(read_query(
        "SN-Query-Table",
        table,
        args.get("query").and_then(Value::as_str),
        args.get("fields"),
        args.get("order_by"),
        input,
    ))
}

pub fn natural_language_search(
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let table = match args.get("table") {
        None => "incident",
        Some(v) => v
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("table must be a string".into()))?,
    };
    let encoded = parse_natural_language_value(args.get("query"), Some(table))
        .ok()
        .map(|r| r.encoded_query)
        .filter(|q| !q.is_empty());
    one(read_query(
        "SN-Natural-Language-Search",
        table,
        encoded.as_deref(),
        args.get("fields"),
        args.get("order_by"),
        input,
    ))
}

pub fn get_record(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let table = table_arg(args, "table_name")?;
    one(base("SN-Get-Record", Action::Read, input)
        .on_table(table, input.classification)
        .returned_fields(csv(args.get("fields"))))
}

fn write_one(
    tool: &'static str,
    action: Action,
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let table = table_arg(args, "table_name")?;
    let (fields, scripts) = written(args.get("data"));
    one(base(tool, action, input)
        .on_table(table, input.classification)
        .written_fields(fields)
        .written_script_fields(scripts))
}

pub fn create_record(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    write_one("SN-Create-Record", Action::Create, args, input)
}

pub fn update_record(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    write_one("SN-Update-Record", Action::Update, args, input)
}

fn metadata(
    tool: &'static str,
    key: &str,
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let table = table_arg(args, key)?;
    one(base(tool, Action::Metadata, input).on_table(table, input.classification))
}

pub fn get_table_schema(
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    metadata("SN-Get-Table-Schema", "table_name", args, input)
}

pub fn discover_table_schema(
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    metadata("SN-Discover-Table-Schema", "table_name", args, input)
}

pub fn explain_field(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    metadata("SN-Explain-Field", "table", args, input)
}

pub fn list_available_tables(
    _args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    one(base("SN-List-Available-Tables", Action::Metadata, input))
}

/// One op per element at `/<key>/<i>`. An input the bulk validator rejects gets
/// one table-less placeholder mutation instead: the handler re-validates and
/// answers the 6.2.0 validation text before any request, so nothing is written.
fn batch(
    tool: &'static str,
    key: &str,
    action: Action,
    valid: bool,
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let items = args.get(key).and_then(Value::as_array);
    let Some(items) = items.filter(|_| valid) else {
        return one(base(tool, action, input));
    };
    let mut ops = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (fields, scripts) = written(item.get("data"));
        ops.push(
            base(tool, action, input)
                .on_table(table, input.classification)
                .at(format!("/{key}/{i}"))
                .written_fields(fields)
                .written_script_fields(scripts),
        );
    }
    Ok(OperationSet(ops))
}

pub fn batch_create(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let valid = validate_batch_create_operations(args.get("operations")).is_ok();
    batch(
        "SN-Batch-Create",
        "operations",
        Action::Create,
        valid,
        args,
        input,
    )
}

pub fn batch_update(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let valid = validate_batch_updates(args.get("updates")).is_ok();
    batch(
        "SN-Batch-Update",
        "updates",
        Action::Update,
        valid,
        args,
        input,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use hpm_types::operation::{ClassificationSnapshot, TableClass};
    use serde_json::json;

    use super::*;

    fn snapshot() -> ClassificationSnapshot {
        let mut s = ClassificationSnapshot::default();
        s.known_tables = ["incident", "problem", "sys_script_include", "sys_user"]
            .iter()
            .map(|t| (*t).to_owned())
            .collect::<HashSet<_>>();
        s.complete = true;
        s
    }

    fn run(
        f: fn(&Value, &NormalizeInput<'_>) -> Result<OperationSet, ToolError>,
        args: Value,
    ) -> OperationSet {
        let snap = snapshot();
        f(&args, &NormalizeInput::new(None, &snap)).expect("normalizes")
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
    fn query_table_carries_operands_for_the_central_guard() {
        let set = run(
            query_table,
            json!({"table_name": "incident", "query": "short_description=javascript:gs.setProperty('a','b')", "order_by": "-priority", "fields": "number, state"}),
        );
        let op = &set.0[0];
        assert_eq!(op.action, Action::Read);
        assert_eq!(op.query_script_operands, ["gs.setProperty('a','b')"]);
        assert_eq!(op.ordering_fields, ["priority"]);
        assert_eq!(op.returned_fields, ["number", "state"]);
    }

    #[test]
    fn natural_language_operands_come_from_the_parsed_query() {
        let set = run(
            natural_language_search,
            json!({"query": "incidents assigned to me"}),
        );
        assert_eq!(set.0[0].table.as_deref(), Some("incident"));
        assert_eq!(set.0[0].query_script_operands, ["gs.getUserID()"]);
    }

    #[test]
    fn writes_escalate_by_table_and_flag_script_values() {
        let set = run(
            create_record,
            json!({"table_name": "sys_script_include", "data": {"script": "x"}}),
        );
        assert_eq!(set.0[0].action, Action::Execute);
        let set = run(
            update_record,
            json!({"table_name": "sys_user", "sys_id": "a", "data": {"roles": "admin"}}),
        );
        assert_eq!(set.0[0].action, Action::AdminWrite);
        let set = run(
            update_record,
            json!({"table_name": "u_unknown", "sys_id": "a", "data": {}}),
        );
        assert_eq!(
            (set.0[0].action, set.0[0].class),
            (Action::Execute, TableClass::Unknown)
        );
        let mut set = run(
            create_record,
            json!({"table_name": "incident", "data": {"short_description": "ok", "u_q": {"x": "javascript:gs.log(1)"}}}),
        );
        assert_eq!(set.0[0].written_script_fields, ["u_q"]);
        set.escalate_written_scripts();
        assert_eq!(set.0[0].action, Action::Execute);
    }

    #[test]
    fn batches_classify_every_element_at_its_location() {
        let set = run(
            batch_create,
            json!({"operations": [
                {"table": "incident", "data": {"a": "1"}},
                {"table": "sys_user", "data": {"b": "${x}"}}
            ]}),
        );
        assert_eq!(set.0.len(), 2);
        assert_eq!(set.0[1].arg_location, "/operations/1");
        assert_eq!(set.0[1].action, Action::AdminWrite);
        let set = run(
            batch_update,
            json!({"updates": [{"table": "problem", "sys_id": "0123456789abcdef0123456789abcdef", "data": {"state": "2"}}]}),
        );
        assert_eq!(set.0[0].action, Action::Update);
        assert_eq!(set.0[0].arg_location, "/updates/0");
    }

    #[test]
    fn invalid_batches_get_one_placeholder_mutation() {
        let set = run(
            batch_create,
            json!({"operations": [{"table": "bad table"}]}),
        );
        assert_eq!(set.0.len(), 1);
        assert!(set.0[0].action.is_mutation());
        assert!(set.0[0].table.is_none());
    }
}
