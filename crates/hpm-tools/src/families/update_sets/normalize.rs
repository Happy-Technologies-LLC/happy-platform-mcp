//! Pure normalizers for the lane C tools (D-05 rows). Writes go through
//! [`Operation::on_table`] so execute-class tables (`wf_*`, `sys_update_xml`)
//! escalate before policy sees them; the update-set pointer move is the one
//! D-05 rule-6 exception (an `update_set` pointer change stays an Update).

use hpm_types::operation::{Action, Operation, OperationSet, Subject};
use hpm_types::tool::{NormalizeInput, ToolError};
use serde_json::Value;

use super::js_text::truthy_opt;

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
    set(vec![
        on("SN-List-Update-Sets", Action::Read, "sys_update_set", i)
            .at("query")
            .returned_fields(returned),
    ])
}

pub(super) fn set_current_application(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        on("SN-Set-Current-Application", Action::Read, "sys_scope", i).at("app_sys_id"),
        op("SN-Set-Current-Application", Action::ConfigWrite, i).at("app_sys_id"),
    ])
}

pub(super) fn execute_background_script(
    _: &Value,
    i: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    set(vec![
        op("SN-Execute-Background-Script", Action::Execute, i).at("script"),
    ])
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
    set(vec![
        on(T, Action::Read, table, i)
            .at("record_sys_ids")
            .returned_fields(fields(&["sys_id", "name", "type"])),
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
