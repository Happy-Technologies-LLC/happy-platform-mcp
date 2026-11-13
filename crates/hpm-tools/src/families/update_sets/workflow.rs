//! SN-Create-Workflow, SN-Create-Activity, SN-Create-Transition,
//! SN-Publish-Workflow (6.2.0 `mcp-server-consolidated.js` 2624-2752,
//! `servicenow-client.js` 2640-2880).

use std::collections::HashMap;

use hpm_core::client::{ClientError, PinnedClient};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::{Map, Value, json};

use super::common::{
    Done, Progress, after_failure, done, error_for, failed_outcome, ok_text, pinned,
};
use super::js_text::{or, put, text, truthy, truthy_opt};
use crate::families::FamilyDeps;
use crate::logic::bulk_limits::validate_workflow_spec;

/// 6.2.0 handler crash after `createCompleteWorkflow` resolves: its result has
/// no `activities` array, so `result.activities.length` throws.
pub(super) const CREATE_WORKFLOW_CRASH: &str =
    "Error: Cannot read properties of undefined (reading 'length')";

/// `createActivity(activityData)` fields; `None` is `undefined`.
#[derive(Default)]
struct ActivityData {
    name: Option<Value>,
    workflow_version_sys_id: Option<Value>,
    activity_definition_sys_id: Option<Value>,
    x: Option<Value>,
    y: Option<Value>,
    width: Option<Value>,
    height: Option<Value>,
    script: Option<Value>,
    vars: Option<Value>,
    stage_sys_id: Option<Value>,
    parent_sys_id: Option<Value>,
    timeout: Option<Value>,
}

fn activity_body(a: &ActivityData) -> Value {
    let mut m = Map::new();
    put(&mut m, "name", a.name.as_ref());
    put(
        &mut m,
        "workflow_version",
        a.workflow_version_sys_id.as_ref(),
    );
    m.insert(
        "activity_definition".into(),
        or(a.activity_definition_sys_id.as_ref(), json!("")),
    );
    m.insert("x".into(), or(a.x.as_ref(), json!(100)));
    m.insert("y".into(), or(a.y.as_ref(), json!(100)));
    m.insert("width".into(), or(a.width.as_ref(), json!(150)));
    m.insert("height".into(), or(a.height.as_ref(), json!(80)));
    m.insert("input".into(), or(a.script.as_ref(), json!("")));
    m.insert("vars".into(), or(a.vars.as_ref(), json!("")));
    m.insert("stage".into(), or(a.stage_sys_id.as_ref(), json!("")));
    m.insert("parent".into(), or(a.parent_sys_id.as_ref(), json!("")));
    m.insert(
        "timeout".into(),
        or(a.timeout.as_ref(), json!("0 00:00:00")),
    );
    Value::Object(m)
}

/// `createCondition(conditionData)` fields.
struct ConditionData<'a> {
    activity_sys_id: Option<&'a Value>,
    name: Value,
    description: Option<&'a Value>,
    condition: Option<&'a Value>,
    order: Option<&'a Value>,
    else_flag: Option<&'a Value>,
}

fn condition_body(c: &ConditionData<'_>) -> Value {
    let mut m = Map::new();
    put(&mut m, "activity", c.activity_sys_id);
    m.insert("name".into(), c.name.clone());
    m.insert("short_description".into(), or(c.description, json!("")));
    m.insert("condition".into(), or(c.condition, json!("")));
    m.insert("order".into(), or(c.order, json!(1)));
    m.insert("else_flag".into(), or(c.else_flag, json!(false)));
    m.insert("event".into(), json!(false));
    m.insert("event_name".into(), json!(""));
    m.insert("condition_type".into(), json!("standard"));
    Value::Object(m)
}

fn transition_body(
    from: Option<&Value>,
    to: Option<&Value>,
    condition_sys_id: Option<&Value>,
    order: Option<&Value>,
) -> Value {
    let mut m = Map::new();
    put(&mut m, "from", from);
    put(&mut m, "to", to);
    m.insert("condition".into(), or(condition_sys_id, json!("")));
    m.insert("order".into(), or(order, json!(1)));
    Value::Object(m)
}

fn publish_body(start: Option<&Value>) -> Value {
    let mut m = Map::new();
    put(&mut m, "start", start);
    m.insert("published".into(), json!(true));
    Value::Object(m)
}

/// `record.sys_id` of a created record (`undefined` when absent).
fn sys_id_of(record: &Value) -> Option<Value> {
    record.get("sys_id").cloned()
}

/// Sequential writes on one pinned client, counting what committed.
struct Writer<'a> {
    client: &'a PinnedClient,
    ctx: &'a RequestContext,
    committed: usize,
}

/// A write sequence stopped: the client error, or `None` when cancelled first.
type Stop = Option<ClientError>;

impl Writer<'_> {
    async fn create(&mut self, table: &str, body: &Value) -> Result<Value, Stop> {
        if !self.ctx.is_live() {
            return Err(None);
        }
        let record = self.client.create_record(table, body).await?;
        self.committed += 1;
        Ok(record)
    }

    async fn update(&mut self, table: &str, sys_id: &str, body: &Value) -> Result<Value, Stop> {
        if !self.ctx.is_live() {
            return Err(None);
        }
        let record = self.client.update_record(table, sys_id, body).await?;
        self.committed += 1;
        Ok(record)
    }

    /// Completion for a stopped sequence; `wrap` adds the 6.2.0 message prefix.
    fn stopped(&self, stop: Stop, wrap: impl FnOnce(String) -> String) -> Done {
        let outcome = after_failure(self.committed, stop.as_ref());
        match stop {
            Some(e) => done(outcome, Err(error_for(outcome, wrap(e.message)))),
            None => done(outcome, Err(ToolError::Cancelled)),
        }
    }
}

/// Activity reference → created sys_id (`resolveActivity`).
fn resolve_activity(
    reference: Option<&Value>,
    references: &HashMap<String, usize>,
    created: &[Value],
) -> Option<Value> {
    match reference? {
        Value::Number(n) => n
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| created.get(i).cloned()),
        Value::String(s) => match references.get(s) {
            Some(&i) => created.get(i).cloned(),
            None => Some(Value::String(s.clone())),
        },
        other => Some(other.clone()),
    }
}

pub(super) fn create_workflow<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let mut spec = Map::new();
        for key in [
            "name",
            "description",
            "table",
            "condition",
            "activities",
            "transitions",
        ] {
            put(&mut spec, key, args.get(key));
        }
        spec.insert(
            "publish".into(),
            args.get("publish").cloned().unwrap_or(json!(false)),
        );
        let spec = Value::Object(spec);
        let references = match validate_workflow_spec(Some(&spec)) {
            Ok(r) => r,
            Err(e) => {
                return done(MutationOutcome::NotSent, Err(ToolError::Message(e.message)));
            }
        };
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let mut progress = Progress::new(ctx, args.get("progress").is_none_or(truthy));
        let mut w = Writer {
            client: &client,
            ctx,
            committed: 0,
        };
        match build_workflow(&mut w, &mut progress, &spec, &references).await {
            // The 6.2.0 handler then throws on `result.activities.length`.
            Ok(()) => done(
                MutationOutcome::Committed,
                Err(ToolError::Text(CREATE_WORKFLOW_CRASH.to_owned())),
            ),
            Err(stop) => w.stopped(stop, |m| format!("Failed to create workflow: {m}")),
        }
    })
}

/// `createCompleteWorkflow` steps 1-5.
async fn build_workflow(
    w: &mut Writer<'_>,
    progress: &mut Progress<'_>,
    spec: &Value,
    references: &HashMap<String, usize>,
) -> Result<(), Stop> {
    let get = |k: &str| spec.get(k);
    progress
        .notify("Creating workflow base".into(), None, None)
        .await;
    let mut workflow = Map::new();
    put(&mut workflow, "name", get("name"));
    workflow.insert("description".into(), or(get("description"), json!("")));
    workflow.insert("template".into(), json!(false));
    workflow.insert("access".into(), json!("public"));
    let workflow = w.create("wf_workflow", &Value::Object(workflow)).await?;

    progress
        .notify("Creating workflow version".into(), None, None)
        .await;
    let mut version = Map::new();
    put(&mut version, "name", get("name"));
    put(&mut version, "workflow", workflow.get("sys_id"));
    put(&mut version, "table", get("table"));
    version.insert("description".into(), or(get("description"), json!("")));
    version.insert("active".into(), json!(true));
    version.insert("published".into(), json!(false));
    version.insert("condition".into(), or(get("condition"), json!("")));
    version.insert("order".into(), json!(100));
    version.insert("run_multiple".into(), json!(false));
    version.insert("after_business_rules".into(), json!(true));
    version.insert("expected_time".into(), json!(""));
    version.insert("condition_type".into(), json!(""));
    let version = w
        .create("wf_workflow_version", &Value::Object(version))
        .await?;
    let version_id = sys_id_of(&version);

    let empty = Vec::new();
    let activities = get("activities")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let total_activities = activities.len();
    let mut created: Vec<Value> = Vec::with_capacity(total_activities);
    for (i, act) in activities.iter().enumerate() {
        let field = |k: &str| act.get(k).cloned();
        progress
            .notify(
                format!(
                    "Creating activity {}/{total_activities}: {}",
                    i + 1,
                    text(act.get("name"))
                ),
                Some(i + 1),
                Some(total_activities),
            )
            .await;
        let data = ActivityData {
            name: field("name"),
            workflow_version_sys_id: version_id.clone(),
            activity_definition_sys_id: field("activity_type"),
            x: Some(field("x").unwrap_or_else(|| json!(100 + i * 150))),
            y: Some(field("y").unwrap_or_else(|| json!(100))),
            width: field("width"),
            height: field("height"),
            script: field("script"),
            vars: field("vars"),
            stage_sys_id: field("stage"),
            parent_sys_id: field("parent"),
            timeout: field("timeout"),
        };
        let activity = w.create("wf_activity", &activity_body(&data)).await?;
        created.push(sys_id_of(&activity).unwrap_or(Value::Null));
    }

    let transitions = get("transitions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let total_transitions = transitions.len();
    for (i, trans) in transitions.iter().enumerate() {
        progress
            .notify(
                format!("Creating transition {}/{total_transitions}", i + 1),
                Some(i + 1),
                Some(total_transitions),
            )
            .await;
        let from = resolve_activity(trans.get("from"), references, &created);
        let to = resolve_activity(trans.get("to"), references, &created);
        let mut condition_id = trans.get("condition_sys_id").cloned();
        if truthy_opt(trans.get("condition")) && !truthy_opt(condition_id.as_ref()) {
            let condition = w
                .create(
                    "wf_condition",
                    &condition_body(&ConditionData {
                        activity_sys_id: from.as_ref(),
                        name: or(trans.get("condition_name"), json!("Condition")),
                        description: trans.get("condition_description"),
                        condition: trans.get("condition"),
                        order: trans.get("order"),
                        else_flag: trans.get("else_flag"),
                    }),
                )
                .await?;
            condition_id = sys_id_of(&condition);
        }
        w.create(
            "wf_transition",
            &transition_body(
                from.as_ref(),
                to.as_ref(),
                condition_id.as_ref(),
                trans.get("order"),
            ),
        )
        .await?;
    }

    if truthy_opt(get("publish")) && total_activities > 0 {
        progress
            .notify("Publishing workflow".into(), None, None)
            .await;
        w.update(
            "wf_workflow_version",
            &text(version_id.as_ref()),
            &publish_body(created.first()),
        )
        .await?;
    }
    progress
        .notify(
            format!(
                "Complete: Workflow created with {total_activities} activities and {total_transitions} transitions"
            ),
            None,
            None,
        )
        .await;
    Ok(())
}

pub(super) fn create_activity<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let field = |k: &str| args.get(k).cloned();
        let data = ActivityData {
            name: field("name"),
            workflow_version_sys_id: field("workflow_version_sys_id"),
            activity_definition_sys_id: field("activity_definition_sys_id"),
            x: field("x"),
            y: field("y"),
            script: field("script"),
            ..ActivityData::default()
        };
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        match client
            .create_record("wf_activity", &activity_body(&data))
            .await
        {
            Ok(record) => done(
                MutationOutcome::Committed,
                ok_text(format!(
                    "✅ Activity created successfully!\n\nActivity: {}\nActivity sys_id: {}\n\nYou can now:\n- Create transitions to/from this activity using SN-Create-Transition\n- Add this activity to workflow canvas in ServiceNow UI",
                    text(args.get("name")),
                    text(record.get("sys_id"))
                )),
            ),
            Err(e) => {
                let outcome = failed_outcome(&e);
                done(outcome, Err(error_for(outcome, e.message)))
            }
        }
    })
}

pub(super) fn create_transition<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let from = args.get("from_activity_sys_id");
        let to = args.get("to_activity_sys_id");
        let condition_script = args.get("condition_script");
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let mut w = Writer {
            client: &client,
            ctx,
            committed: 0,
        };
        let mut condition_id: Option<Value> = None;
        if truthy_opt(condition_script) {
            let body = condition_body(&ConditionData {
                activity_sys_id: from,
                name: json!("Transition Condition"),
                description: None,
                condition: condition_script,
                order: None,
                else_flag: None,
            });
            match w.create("wf_condition", &body).await {
                Ok(c) => condition_id = sys_id_of(&c),
                Err(stop) => return w.stopped(stop, |m| m),
            }
        }
        let body = transition_body(from, to, condition_id.as_ref(), args.get("order"));
        let transition = match w.create("wf_transition", &body).await {
            Ok(t) => t,
            Err(stop) => return w.stopped(stop, |m| m),
        };
        let condition_line = match condition_id.as_ref().filter(|c| truthy(c)) {
            Some(c) => format!("Condition sys_id: {}", text(Some(c))),
            None => "No condition (always transitions)".to_owned(),
        };
        done(
            MutationOutcome::Committed,
            ok_text(format!(
                "✅ Transition created successfully!\n\nTransition sys_id: {}\nFrom activity: {}\nTo activity: {}\n{condition_line}\n\nThe workflow will now transition from the source activity to the target activity{}.",
                text(transition.get("sys_id")),
                text(from),
                text(to),
                if truthy_opt(condition_script) {
                    " when the condition is met"
                } else {
                    ""
                }
            )),
        )
    })
}

pub(super) fn publish_workflow<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let version = text(args.get("version_sys_id"));
        let start = args.get("start_activity_sys_id");
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        match client
            .update_record("wf_workflow_version", &version, &publish_body(start))
            .await
        {
            Ok(_) => done(
                MutationOutcome::Committed,
                ok_text(format!(
                    "✅ Workflow published successfully!\n\nVersion sys_id: {version}\nStart activity: {}\nStatus: Published\n\nThe workflow is now active and will trigger based on its configured conditions.",
                    text(start)
                )),
            ),
            Err(e) => {
                let outcome = failed_outcome(&e);
                done(outcome, Err(error_for(outcome, e.message)))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_defaults_follow_javascript_or() {
        let body = activity_body(&ActivityData {
            name: Some(json!("A")),
            x: Some(json!(0)),
            ..ActivityData::default()
        });
        assert_eq!(
            body,
            json!({
                "name": "A", "activity_definition": "", "x": 100, "y": 100,
                "width": 150, "height": 80, "input": "", "vars": "", "stage": "",
                "parent": "", "timeout": "0 00:00:00"
            })
        );
    }

    #[test]
    fn transition_and_condition_defaults() {
        assert_eq!(
            transition_body(Some(&json!("f")), None, None, Some(&json!(0))),
            json!({"from": "f", "condition": "", "order": 1})
        );
        let c = condition_body(&ConditionData {
            activity_sys_id: None,
            name: json!("Condition"),
            description: None,
            condition: Some(&json!("x")),
            order: Some(&json!(3)),
            else_flag: None,
        });
        assert_eq!(c["order"], json!(3));
        assert_eq!(c["else_flag"], json!(false));
        assert!(c.get("activity").is_none());
    }

    #[test]
    fn references_resolve_by_index_name_or_literal_sys_id() {
        let created = vec![json!("s0"), json!("s1")];
        let refs = HashMap::from([("Start".to_owned(), 0)]);
        assert_eq!(
            resolve_activity(Some(&json!(1)), &refs, &created),
            Some(json!("s1"))
        );
        assert_eq!(
            resolve_activity(Some(&json!("Start")), &refs, &created),
            Some(json!("s0"))
        );
        assert_eq!(
            resolve_activity(Some(&json!("abc")), &refs, &created),
            Some(json!("abc"))
        );
        assert_eq!(resolve_activity(None, &refs, &created), None);
    }
}
