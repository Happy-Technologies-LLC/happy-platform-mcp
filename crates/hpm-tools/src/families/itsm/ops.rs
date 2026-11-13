//! Incident / change / problem convenience operations (6.2.0
//! `mcp-server-consolidated.js` 2815-3272): look the record up by number, then
//! one PUT.

use hpm_types::operation::{Action, Operation};
use hpm_types::tool::NormalizeInput;
use serde_json::{Map, Value, json};

use super::js_out::{or, prop, tpl, truthy};
use super::{Call, Fail, Kind, Sn, first, is_sys_id, iso_now, query, read, write};

struct Target {
    table: &'static str,
    /// Subject of the not-found error.
    missing: &'static str,
    /// Heading in the result text.
    label: &'static str,
    /// Noun in the closing sentence.
    noun: &'static str,
    number_arg: &'static str,
}

const INCIDENT: Target = Target {
    table: "incident",
    missing: "Incident",
    label: "Incident",
    noun: "incident",
    number_arg: "incident_number",
};
const CHANGE: Target = Target {
    table: "change_request",
    missing: "Change request",
    label: "Change Request",
    noun: "change request",
    number_arg: "change_number",
};
const PROBLEM: Target = Target {
    table: "problem",
    missing: "Problem",
    label: "Problem",
    noun: "problem",
    number_arg: "problem_number",
};

fn target(kind: Kind) -> &'static Target {
    match kind {
        Kind::AddChangeComment | Kind::AssignChange | Kind::ApproveChange => &CHANGE,
        Kind::AddProblemComment | Kind::CloseProblem => &PROBLEM,
        _ => &INCIDENT,
    }
}

fn lookup_query(number: &str) -> String {
    format!("number={number}")
}

fn user_query(user: &str) -> String {
    format!("name={user}^ORuser_name={user}")
}

fn group_query(group: &str) -> String {
    format!("name={group}")
}

/// Assignment arguments that need a lookup (not already a sys_id).
fn needs_user(args: &Value) -> Option<String> {
    let user = tpl(args.get("assigned_to"));
    (!is_sys_id(&user)).then_some(user)
}

fn needs_group(args: &Value) -> Option<String> {
    let group = args.get("assignment_group");
    let text = tpl(group);
    (truthy(group) && !is_sys_id(&text)).then_some(text)
}

/// The update body, given resolved assignment ids where they apply.
fn update_data(kind: Kind, args: &Value, assigned: Option<&Value>, group: Option<&Value>) -> Value {
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<&Value>| {
        if let Some(v) = v {
            m.insert(k.to_owned(), v.clone());
        }
    };
    let a = |k: &str| args.get(k);
    match kind {
        Kind::AddComment | Kind::AddChangeComment | Kind::AddProblemComment => {
            put("comments", a("comment"));
        }
        Kind::AddWorkNotes => put("work_notes", a("work_notes")),
        Kind::AssignIncident | Kind::AssignChange => {
            put("assigned_to", assigned);
            if truthy(group) {
                put("assignment_group", group);
            }
        }
        Kind::ResolveIncident | Kind::CloseIncident => {
            let (state, notes, code) = if kind == Kind::ResolveIncident {
                (6, "resolution_notes", "resolution_code")
            } else {
                (7, "close_notes", "close_code")
            };
            put("state", Some(&json!(state)));
            put("close_notes", a(notes));
            if truthy(a(code)) {
                put("close_code", a(code));
            }
        }
        Kind::ApproveChange => {
            put("approval", Some(&json!("approved")));
            if truthy(a("approval_comments")) {
                put("comments", a("approval_comments"));
            }
        }
        _ => {
            put("state", Some(&json!(3)));
            put("resolution_notes", a("resolution_notes"));
            if truthy(a("resolution_code")) {
                put("resolution_code", a("resolution_code"));
            }
        }
    }
    Value::Object(m)
}

pub(super) fn normalize(kind: Kind, args: &Value, input: &NormalizeInput<'_>) -> Vec<Operation> {
    let t = target(kind);
    let number = tpl(args.get(t.number_arg));
    let mut ops = vec![read(
        kind,
        t.table,
        &[&lookup_query(&number)],
        vec!["sys_id".into()],
        input,
    )];
    let assigns = matches!(kind, Kind::AssignIncident | Kind::AssignChange);
    if assigns {
        if let Some(user) = needs_user(args) {
            ops.push(read(
                kind,
                "sys_user",
                &[&user_query(&user)],
                vec!["sys_id".into()],
                input,
            ));
        }
        if let Some(group) = needs_group(args) {
            ops.push(read(
                kind,
                "sys_user_group",
                &[&group_query(&group)],
                vec!["sys_id".into()],
                input,
            ));
        }
    }
    // Field names only: assignment values are resolved sys_ids at run time.
    let resolved = Value::String("sys_id".to_owned());
    let group = truthy(args.get("assignment_group")).then_some(&resolved);
    let data = update_data(kind, args, Some(&resolved), group);
    ops.push(write(kind, Action::Update, Some(t.table), &data, input).at(""));
    ops
}

pub(super) async fn run<S: Sn>(
    kind: Kind,
    call: &mut Call<'_, S>,
    args: &Value,
) -> Result<String, Fail> {
    let t = target(kind);
    let number_v = args.get(t.number_arg);
    let number = tpl(number_v);
    let list = call
        .records(t.table, query(lookup_query(&number), "1"))
        .await?;
    let record = first(&list, || format!("{} {number} not found", t.missing))?;
    let sys_id = tpl(prop(Some(&record), "sys_id"));

    let mut assigned: Option<Value> = None;
    let mut group: Option<Value> = None;
    if matches!(kind, Kind::AssignIncident | Kind::AssignChange) {
        assigned = args.get("assigned_to").cloned();
        if let Some(user) = needs_user(args) {
            let users = call
                .records("sys_user", query(user_query(&user), "1"))
                .await?;
            let u = first(&users, || format!("User \"{user}\" not found"))?;
            assigned = prop(Some(&u), "sys_id").cloned();
        }
        group = args.get("assignment_group").cloned();
        if let Some(name) = needs_group(args) {
            let groups = call
                .records("sys_user_group", query(group_query(&name), "1"))
                .await?;
            let g = first(&groups, || format!("Group \"{name}\" not found"))?;
            group = prop(Some(&g), "sys_id").cloned();
        }
    }

    let data = update_data(kind, args, assigned.as_ref(), group.as_ref());
    let result = call.mutate(call.sn.update(t.table, &sys_id, &data)).await?;
    let now = iso_now();
    let a = |k: &str| tpl(args.get(k));
    let opt = |label: &str, k: &str| {
        if truthy(args.get(k)) {
            format!("{label}: {}", a(k))
        } else {
            String::new()
        }
    };
    let head = format!("{}: {number}\nsys_id: {sys_id}", t.label);
    Ok(match kind {
        Kind::AddComment | Kind::AddChangeComment | Kind::AddProblemComment => format!(
            "✅ Comment added to {number}\n\n{head}\nComment: {}\nUpdated: {now}\n\nThe comment has been successfully added to the {}.",
            a("comment"),
            t.noun
        ),
        Kind::AddWorkNotes => format!(
            "✅ Work notes added to {number}\n\n{head}\nWork Notes: {}\nUpdated: {now}\n\nThe work notes have been successfully added to the incident.",
            a("work_notes")
        ),
        Kind::AssignIncident | Kind::AssignChange => {
            let shown = |field: &str, id: Option<&Value>| {
                tpl(or(prop(prop(Some(&result), field), "display_value"), id))
            };
            let group_line = if truthy(group.as_ref()) {
                format!(
                    "Assignment Group: {}",
                    shown("assignment_group", group.as_ref())
                )
            } else {
                String::new()
            };
            format!(
                "✅ {number} assigned successfully\n\n{head}\nAssigned To: {}\n{group_line}\nUpdated: {now}\n\nThe {} has been assigned successfully.",
                shown("assigned_to", assigned.as_ref()),
                t.noun
            )
        }
        Kind::ResolveIncident => format!(
            "✅ {number} resolved successfully\n\n{head}\nState: Resolved (6)\nResolution Notes: {}\n{}\nUpdated: {now}\n\nThe incident has been resolved successfully.",
            a("resolution_notes"),
            opt("Resolution Code", "resolution_code")
        ),
        Kind::CloseIncident => format!(
            "✅ {number} closed successfully\n\n{head}\nState: Closed (7)\nClose Notes: {}\n{}\nUpdated: {now}\n\nThe incident has been closed successfully.",
            a("close_notes"),
            opt("Close Code", "close_code")
        ),
        Kind::ApproveChange => format!(
            "✅ {number} approved successfully\n\n{head}\nApproval: approved\n{}\nUpdated: {now}\n\nThe change request has been approved successfully.",
            opt("Comments", "approval_comments")
        ),
        _ => format!(
            "✅ {number} closed successfully\n\n{head}\nState: Resolved/Closed (3)\nResolution Notes: {}\n{}\nUpdated: {now}\n\nThe problem has been closed successfully.",
            a("resolution_notes"),
            opt("Resolution Code", "resolution_code")
        ),
    })
}

#[cfg(test)]
mod tests {
    use hpm_types::tool::{MutationOutcome, ResultBody, ToolError};
    use serde_json::json;

    use super::super::fake::Fake;
    use super::super::run as run_tool;
    use super::*;

    const INC: &str = "2fc8aa1ec6d67779a3ab923ac2cc7238";
    const USER: &str = "8d1cba9bbb9a19d3a8ab22c62b5c2df1";

    fn text(done: &hpm_types::tool::HandlerCompletion) -> String {
        match &done.result {
            Ok(ResultBody::ToolResult { text, .. }) => text.clone(),
            other => panic!("not a tool result: {other:?}"),
        }
    }

    fn error(done: &hpm_types::tool::HandlerCompletion) -> String {
        match &done.result {
            Err(ToolError::Message(m)) => format!("Error: {m}"),
            Err(ToolError::Upstream { message, .. }) => format!("Error: {message}"),
            other => panic!("not an error: {other:?}"),
        }
    }

    /// Blanks the `Updated:` timestamp.
    fn stable(t: &str) -> String {
        t.lines()
            .map(|l| {
                if l.starts_with("Updated: ") {
                    "Updated: T"
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn add_comment_looks_up_by_number_then_updates() {
        let comment = "Test with \"quotes\" and \\backslash & ampersand";
        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!({}));
        let done = run_tool(
            Kind::AddComment,
            &sn,
            &json!({"incident_number": "INC0010001", "comment": comment}),
        )
        .await;
        assert_eq!(
            sn.calls(),
            [
                "GET incident query=number=INC0010001 limit=1".to_owned(),
                format!(
                    r#"PUT incident/{INC} {{"comments":"Test with \"quotes\" and \\backslash & ampersand"}}"#
                ),
            ]
        );
        assert_eq!(done.outcome, MutationOutcome::Committed);
        assert_eq!(
            stable(&text(&done)),
            format!(
                "✅ Comment added to INC0010001\n\nIncident: INC0010001\nsys_id: {INC}\nComment: {comment}\nUpdated: T\n\nThe comment has been successfully added to the incident."
            )
        );
    }

    #[tokio::test]
    async fn not_found_errors_per_table_and_no_mutation() {
        for (kind, args, want) in [
            (
                Kind::AddComment,
                json!({"incident_number": "INC9999999", "comment": "c"}),
                "Error: Incident INC9999999 not found",
            ),
            (
                Kind::AddWorkNotes,
                json!({"incident_number": "INC1", "work_notes": "w"}),
                "Error: Incident INC1 not found",
            ),
            (
                Kind::ApproveChange,
                json!({"change_number": "CHG1"}),
                "Error: Change request CHG1 not found",
            ),
            (
                Kind::CloseProblem,
                json!({"problem_number": "PRB1", "resolution_notes": "n"}),
                "Error: Problem PRB1 not found",
            ),
        ] {
            let sn = Fake::new().ok(json!([]));
            let done = run_tool(kind, &sn, &args).await;
            assert_eq!(error(&done), want);
            assert_eq!(done.outcome, MutationOutcome::Failed);
            assert_eq!(sn.calls().len(), 1);
        }
    }

    #[tokio::test]
    async fn assign_resolves_user_by_name_and_skips_lookup_for_sys_id() {
        let sn = Fake::new()
            .ok(json!([{"sys_id": INC}]))
            .ok(json!([{"sys_id": USER}]))
            .ok(json!({"assigned_to": {"value": USER, "display_value": "John Doe"}}));
        let done = run_tool(
            Kind::AssignIncident,
            &sn,
            &json!({"incident_number": "INC1", "assigned_to": "john.doe"}),
        )
        .await;
        assert_eq!(
            sn.calls()[1],
            "GET sys_user query=name=john.doe^ORuser_name=john.doe limit=1"
        );
        assert_eq!(
            sn.calls()[2],
            format!(r#"PUT incident/{INC} {{"assigned_to":"{USER}"}}"#)
        );
        assert_eq!(
            stable(&text(&done)),
            format!(
                "✅ INC1 assigned successfully\n\nIncident: INC1\nsys_id: {INC}\nAssigned To: John Doe\n\nUpdated: T\n\nThe incident has been assigned successfully."
            )
        );

        let upper = USER.to_ascii_uppercase();
        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!({}));
        run_tool(
            Kind::AssignChange,
            &sn,
            &json!({"change_number": "CHG1", "assigned_to": upper}),
        )
        .await;
        assert_eq!(sn.calls().len(), 2, "a sys_id (any case) needs no lookup");
    }

    #[tokio::test]
    async fn assign_user_or_group_not_found() {
        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!([]));
        let done = run_tool(
            Kind::AssignIncident,
            &sn,
            &json!({"incident_number": "INC1", "assigned_to": "nobody"}),
        )
        .await;
        assert_eq!(error(&done), "Error: User \"nobody\" not found");

        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!([]));
        let done = run_tool(
            Kind::AssignChange,
            &sn,
            &json!({"change_number": "CHG1", "assigned_to": USER, "assignment_group": "Ghosts"}),
        )
        .await;
        assert_eq!(error(&done), "Error: Group \"Ghosts\" not found");
        assert_eq!(done.outcome, MutationOutcome::Failed);
    }

    #[tokio::test]
    async fn resolve_and_close_bodies_and_optional_code_lines() {
        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!({}));
        let done = run_tool(
            Kind::ResolveIncident,
            &sn,
            &json!({"incident_number": "INC1", "resolution_notes": "Fixed", "resolution_code": "Solved (Work Around)"}),
        )
        .await;
        assert_eq!(
            sn.calls()[1],
            format!(
                r#"PUT incident/{INC} {{"state":6,"close_notes":"Fixed","close_code":"Solved (Work Around)"}}"#
            )
        );
        assert!(text(&done).contains(
            "State: Resolved (6)\nResolution Notes: Fixed\nResolution Code: Solved (Work Around)\n"
        ));

        let sn = Fake::new().ok(json!([{"sys_id": INC}])).ok(json!({}));
        let done = run_tool(
            Kind::CloseIncident,
            &sn,
            &json!({"incident_number": "INC1", "close_notes": "Done"}),
        )
        .await;
        assert_eq!(
            sn.calls()[1],
            format!(r#"PUT incident/{INC} {{"state":7,"close_notes":"Done"}}"#)
        );
        assert!(text(&done).contains("State: Closed (7)\nClose Notes: Done\n\nUpdated: "));
    }

    #[tokio::test]
    async fn api_errors_render_client_text_and_set_outcome() {
        let sn = Fake::new().http(403, "403 User Not Authorized ACL exception");
        let done = run_tool(
            Kind::AddComment,
            &sn,
            &json!({"incident_number": "INC1", "comment": "c"}),
        )
        .await;
        assert_eq!(error(&done), "Error: 403 User Not Authorized ACL exception");
        assert_eq!(done.outcome, MutationOutcome::Failed);

        let sn = Fake::new()
            .ok(json!([{"sys_id": INC}]))
            .http(500, "500 boom");
        let done = run_tool(
            Kind::CloseProblem,
            &sn,
            &json!({"problem_number": "P", "resolution_notes": "n"}),
        )
        .await;
        assert_eq!(done.outcome, MutationOutcome::Failed);

        let sn = Fake::new()
            .ok(json!([{"sys_id": INC}]))
            .transport("socket hang up");
        let done = run_tool(
            Kind::CloseProblem,
            &sn,
            &json!({"problem_number": "P", "resolution_notes": "n"}),
        )
        .await;
        assert_eq!(done.outcome, MutationOutcome::Unknown);
    }

    #[tokio::test]
    async fn cancelled_call_sends_nothing() {
        let sn = Fake::new().cancelled();
        let done = run_tool(
            Kind::AddComment,
            &sn,
            &json!({"incident_number": "INC1", "comment": "c"}),
        )
        .await;
        assert!(matches!(done.result, Err(ToolError::Cancelled)));
        assert_eq!(done.outcome, MutationOutcome::NotSent);
        assert!(sn.calls().is_empty());
    }

    #[test]
    fn normalizer_covers_lookups_and_update() {
        let snap = Default::default();
        let input = NormalizeInput::new(None, &snap);
        let ops = normalize(
            Kind::AssignIncident,
            &json!({"incident_number": "INC1", "assigned_to": "javascript:gs.setProperty()", "assignment_group": "G"}),
            &input,
        );
        let tables: Vec<_> = ops
            .iter()
            .map(|o| o.table.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            tables,
            ["incident", "sys_user", "sys_user_group", "incident"]
        );
        assert_eq!(
            ops[1].query_script_operands,
            ["gs.setProperty()", "gs.setProperty()"]
        );
        assert!(ops[3].action.is_mutation());
        assert_eq!(ops[3].written_fields, ["assigned_to", "assignment_group"]);
    }
}
