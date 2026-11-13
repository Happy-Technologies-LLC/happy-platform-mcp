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

use crate::logic::bulk_limits::{
    resolve_batch_placeholders, validate_batch_create_operations, validate_batch_updates,
};
use crate::logic::encoded_query::analyze_query;
use crate::logic::nl::parse_natural_language_value;

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

/// Table and field names the metadata tools interpolate into encoded queries:
/// anything else could add `^OR`/`^NQ` predicates or a `javascript:` operand.
fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$'))
}

/// The 7.0 error for a metadata identifier argument that is not a plain name
/// (checked by the normalizer and again by the handler).
pub fn identifier_error(args: &Value, key: &str) -> Option<String> {
    let ok = args
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(is_identifier);
    (!ok).then(|| format!("{key} must contain only letters, digits, '_', '.' or '$'"))
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
        if !ordering.contains(&f) {
            ordering.push(f);
        }
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
    keys: &[&str],
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let table = table_arg(args, key)?;
    for k in keys {
        if let Some(e) = identifier_error(args, k) {
            return Err(ToolError::InvalidParams(e));
        }
    }
    one(base(tool, Action::Metadata, input).on_table(table, input.classification))
}

pub fn get_table_schema(
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    metadata(
        "SN-Get-Table-Schema",
        "table_name",
        &["table_name"],
        args,
        input,
    )
}

pub fn discover_table_schema(
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    metadata(
        "SN-Discover-Table-Schema",
        "table_name",
        &["table_name"],
        args,
        input,
    )
}

pub fn explain_field(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    metadata(
        "SN-Explain-Field",
        "table",
        &["table", "field"],
        args,
        input,
    )
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
    data: Option<Vec<Value>>,
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let items = args.get(key).and_then(Value::as_array);
    let (Some(items), Some(data)) = (items, data) else {
        return one(base(tool, action, input));
    };
    let mut ops = Vec::with_capacity(items.len());
    for (i, (item, data)) in items.iter().zip(&data).enumerate() {
        let table = item
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (fields, scripts) = written(Some(data));
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

/// A stand-in sys_id for a placeholder ref: what the wire carries instead.
const REF_STAND_IN: &str = "00000000000000000000000000000000";

/// Data of each operation with every well-formed `${name}` ref to an earlier
/// operation replaced as the handler would, so refs are not script values.
fn batch_create_data(args: &Value) -> Option<Vec<Value>> {
    let serialized = validate_batch_create_operations(args.get("operations")).ok()?;
    let ops = args.get("operations")?.as_array()?;
    let mut earlier = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(ops.len());
    for (i, (op, s)) in ops.iter().zip(&serialized).enumerate() {
        out.push(serde_json::from_str(&resolve_batch_placeholders(s, &earlier)).ok()?);
        earlier.insert(batch_ref_name(op, i), REF_STAND_IN.to_owned());
    }
    Some(out)
}

/// The name an operation's sys_id is saved under (`save_as` or `operation_<i>`).
pub fn batch_ref_name(op: &Value, i: usize) -> String {
    match op.get("save_as") {
        Some(v) if crate::families::crud::json::truthy(Some(v)) => {
            crate::logic::js::to_js_string(v)
        }
        _ => format!("operation_{i}"),
    }
}

pub fn batch_create(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let data = batch_create_data(args);
    batch(
        "SN-Batch-Create",
        "operations",
        Action::Create,
        data,
        args,
        input,
    )
}

pub fn batch_update(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let data = validate_batch_updates(args.get("updates"))
        .ok()
        .and_then(|()| {
            let items = args.get("updates")?.as_array()?;
            Some(
                items
                    .iter()
                    .map(|u| u.get("data").cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        });
    batch(
        "SN-Batch-Update",
        "updates",
        Action::Update,
        data,
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

    #[test]
    fn metadata_names_must_be_identifiers() {
        let snap = snapshot();
        let input = NormalizeInput::new(None, &snap);
        type Norm = fn(&Value, &NormalizeInput<'_>) -> Result<OperationSet, ToolError>;
        let cases: [(Norm, Value); 4] = [
            (
                explain_field,
                json!({"table": "incident", "field": "x^ORelement=javascript:gs.log(1)"}),
            ),
            (
                explain_field,
                json!({"table": "incident^NQname=sys_user", "field": "state"}),
            ),
            (
                discover_table_schema,
                json!({"table_name": "incident^NQname=javascript:x"}),
            ),
            (get_table_schema, json!({"table_name": "incident^ORname=x"})),
        ];
        for (f, args) in cases {
            assert!(matches!(f(&args, &input), Err(ToolError::InvalidParams(_))));
        }
        let ok = run(
            explain_field,
            json!({"table": "u_x$1", "field": "caller_id.name"}),
        );
        assert_eq!(ok.0.len(), 1);
    }

    #[test]
    fn batch_refs_to_earlier_operations_are_not_script_values() {
        let set = run(
            batch_create,
            json!({"operations": [
                {"table": "incident", "data": {}, "save_as": "p"},
                {"table": "incident", "data": {"parent": "${p}"}},
                {"table": "incident", "data": {"x": "${operation_1}"}},
                {"table": "incident", "data": {"y": "${later}"}}
            ]}),
        );
        assert!(set.0[1].written_script_fields.is_empty());
        assert!(set.0[2].written_script_fields.is_empty());
        assert_eq!(set.0[3].written_script_fields, ["y"]);
    }
}
