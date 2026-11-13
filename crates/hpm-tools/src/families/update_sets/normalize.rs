//! Pure normalizers for the lane C tools (D-05 rows). Writes go through
//! [`Operation::on_table`] so execute-class tables (`wf_*`, `sys_update_xml`)
//! escalate before policy sees them; the update-set pointer move is the one
//! D-05 rule-6 exception (an `update_set` pointer change stays an Update).

use hpm_types::operation::{Action, Operation, OperationSet, Subject};
use hpm_types::tool::{NormalizeInput, ToolError};
use serde_json::Value;

use super::js_text::truthy_opt;
use crate::logic::encoded_query::analyze_query;

fn op(tool: &'static str, action: Action, input: &NormalizeInput<'_>) -> Operation {
    Operation::new(Subject::Tool(tool), action)
        .instance_id(input.instance.map(|i| i.instance_id.clone()))
}

fn on(tool: &'static str, action: Action, table: &str, input: &NormalizeInput<'_>) -> Operation {
    op(tool, action, input).on_table(table, input.classification)
}

fn fields(list: &[&str]) -> Vec<String> {
    list.iter().map(|f| (*f).to_owned()).collect()
}

fn set(ops: Vec<Operation>) -> Result<OperationSet, ToolError> {
    Ok(OperationSet(ops))
}

pub(super) fn set_update_set(_: &Value, i: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    set(vec![
        on("SN-Set-Update-Set", Action::Read, "sys_update_set", i).at("update_set_sys_id"),
        op("SN-Set-Update-Set", Action::ConfigWrite, i).at("update_set_sys_id"),
    ])
}

pub(super) fn get_current_update_set(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        on(
            "SN-Get-Current-Update-Set",
            Action::Read,
            "sys_update_set",
            i,
        )
        .returned_fields(fields(&["name", "sys_id"])),
    ])
}

pub(super) fn list_update_sets(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let returned = args
        .get("fields")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
        .map(|f| f.split(',').map(|s| s.trim().to_owned()).collect())
        .unwrap_or_default();
    let info = args
        .get("query")
        .and_then(Value::as_str)
        .map(analyze_query)
        .unwrap_or_default();
    let mut ordering = info.ordering_fields;
    // `order_by` is a field name, `-` prefixed for descending.
    if let Some(order) = args
        .get("order_by")
        .and_then(Value::as_str)
        .map(|o| o.trim_start_matches('-'))
        .filter(|o| !o.is_empty() && !ordering.iter().any(|f| f == o))
    {
        ordering.push(order.to_owned());
    }
    set(vec![
        on("SN-List-Update-Sets", Action::Read, "sys_update_set", i)
            .at("query")
            .returned_fields(returned)
            .predicate_fields(info.predicate_fields)
            .ordering_fields(ordering)
            .query_script_operands(info.script_operands),
    ])
}

pub(super) fn set_current_application(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    // Core verifies "global" against sys_scope and any other id against sys_app.
    let table = if args.get("app_sys_id").and_then(Value::as_str) == Some("global") {
        "sys_scope"
    } else {
        "sys_app"
    };
    set(vec![
        on("SN-Set-Current-Application", Action::Read, table, i).at("app_sys_id"),
        op("SN-Set-Current-Application", Action::ConfigWrite, i).at("app_sys_id"),
    ])
}

pub(super) fn execute_background_script(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Execute-Background-Script";
    let mut ops = vec![op(T, Action::Execute, i).at("script")];
    if truthy_opt(args.get("scope")) {
        // Scope resolution; a scoped script never falls back to sys_trigger.
        ops.push(on(T, Action::Read, "sys_scope", i).at("scope"));
    } else {
        // sys_trigger fallback (wait:false, or the processor did not run it).
        ops.push(on(T, Action::Create, "sys_trigger", i).at("script"));
        ops.push(on(T, Action::Update, "sys_trigger", i).at("script"));
    }
    ops.push(
        on(T, Action::Read, "sys_script_execution_history", i)
            .returned_fields(fields(&["rollback_context"])),
    );
    ops.push(
        on(T, Action::Read, "syslog", i)
            .predicate_fields(fields(&["message", "source", "sys_created_on"]))
            .returned_fields(fields(&["message", "sys_created_on"])),
    );
    set(ops)
}

pub(super) fn create_fix_script(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        op("SN-Create-Fix-Script", Action::Create, i).at("script_content"),
    ])
}

pub(super) fn inspect_update_set(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Inspect-Update-Set";
    set(vec![
        on(T, Action::Read, "sys_update_set", i)
            .at("update_set")
            .returned_fields(fields(&["sys_id", "name", "state", "description"])),
        on(T, Action::Read, "sys_update_xml", i)
            .at("update_set")
            .predicate_fields(fields(&["update_set"]))
            .returned_fields(fields(&["type", "name", "target_name"])),
    ])
}

pub(super) fn create_workflow(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Create-Workflow";
    let mut ops = vec![
        on(T, Action::Create, "wf_workflow", i).written_fields(fields(&["name", "description"])),
        on(T, Action::Create, "wf_workflow_version", i).written_fields(fields(&[
            "name",
            "table",
            "description",
            "condition",
        ])),
        on(T, Action::Create, "wf_activity", i)
            .at("activities")
            .written_fields(fields(&["name", "input"]))
            .written_script_fields(fields(&["input"])),
    ];
    let transitions = args.get("transitions").and_then(Value::as_array);
    if transitions.is_some_and(|t| t.iter().any(|t| truthy_opt(t.get("condition")))) {
        ops.push(
            on(T, Action::Create, "wf_condition", i)
                .at("transitions")
                .written_fields(fields(&["condition"])),
        );
    }
    if transitions.is_some_and(|t| !t.is_empty()) {
        ops.push(on(T, Action::Create, "wf_transition", i).at("transitions"));
    }
    if truthy_opt(args.get("publish")) {
        ops.push(
            on(T, Action::Update, "wf_workflow_version", i)
                .at("publish")
                .written_fields(fields(&["start", "published"])),
        );
    }
    set(ops)
}

pub(super) fn create_activity(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        on("SN-Create-Activity", Action::Create, "wf_activity", i)
            .written_fields(fields(&["name", "workflow_version", "input", "x", "y"]))
            .written_script_fields(fields(&["input"])),
    ])
}

pub(super) fn create_transition(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Create-Transition";
    let mut ops = Vec::new();
    if truthy_opt(args.get("condition_script")) {
        ops.push(
            on(T, Action::Create, "wf_condition", i)
                .at("condition_script")
                .written_fields(fields(&["activity", "name", "condition"])),
        );
    }
    ops.push(
        on(T, Action::Create, "wf_transition", i).written_fields(fields(&[
            "from",
            "to",
            "condition",
            "order",
        ])),
    );
    set(ops)
}

pub(super) fn publish_workflow(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        on(
            "SN-Publish-Workflow",
            Action::Update,
            "wf_workflow_version",
            i,
        )
        .at("version_sys_id")
        .written_fields(fields(&["start", "published"])),
    ])
}

pub(super) fn move_records_to_update_set(
    args: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Move-Records-To-Update-Set";
    let table = args
        .get("table")
        .and_then(Value::as_str)
        .unwrap_or("sys_update_xml");
    let class = i.classification.classify(table);
    // D-05 rule 6: only the `update_set` pointer of sys_update_xml rows moves;
    // the payload is untouched, so the write stays an Update.
    let write = if table == "sys_update_xml" {
        op(T, Action::Update, i).table(table, class)
    } else {
        on(T, Action::Update, table, i)
    };
    let mut read = on(T, Action::Read, table, i)
        .at("record_sys_ids")
        .returned_fields(fields(&["sys_id", "name", "type"]));
    if truthy_opt(args.get("time_range")) {
        let mut predicates = fields(&["sys_created_on"]);
        if truthy_opt(args.get("source_update_set")) {
            predicates.push("update_set.name".to_owned());
        }
        read = read.at("time_range").predicate_fields(predicates);
    }
    set(vec![
        read,
        write
            .at("update_set_id")
            .written_fields(fields(&["update_set"])),
    ])
}

pub(super) fn clone_update_set(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    const T: &str = "SN-Clone-Update-Set";
    set(vec![
        on(T, Action::Read, "sys_update_set", i).at("source_update_set_id"),
        on(T, Action::Read, "sys_update_xml", i)
            .at("source_update_set_id")
            .predicate_fields(fields(&["update_set"])),
        on(T, Action::Create, "sys_update_set", i)
            .at("new_name")
            .written_fields(fields(&["name", "description", "application", "state"])),
        on(T, Action::Create, "sys_update_xml", i).written_fields(fields(&[
            "update_set",
            "name",
            "type",
            "target_name",
            "payload",
            "category",
        ])),
    ])
}

#[cfg(test)]
mod tests {
    use hpm_types::operation::ClassificationSnapshot;
    use serde_json::json;

    use super::*;

    #[test]
    fn list_update_sets_reports_query_fields_and_script_operands() {
        let snapshot = ClassificationSnapshot::default();
        let input = NormalizeInput::new(None, &snapshot);
        let ops = list_update_sets(
            &json!({"query": "state=complete^sys_id=javascript:gs.getUserID()", "order_by": "-sys_created_on"}),
            &input,
        )
        .expect("ops");
        let op = &ops.0[0];
        assert!(op.predicate_fields.iter().any(|f| f == "state"));
        assert_eq!(op.ordering_fields, vec!["sys_created_on".to_owned()]);
        assert_eq!(op.query_script_operands.len(), 1);
    }
}
