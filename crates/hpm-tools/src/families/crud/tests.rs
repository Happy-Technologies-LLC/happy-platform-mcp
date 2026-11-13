//! Handler branches the goldens do not cover (most lane A tools have one or two
//! goldens): error texts, batch partial outcomes, progress, schema fallbacks.

use std::sync::Mutex;

use hpm_core::client::{ClientError, RecordParams, RecordQuery};
use hpm_types::BoxFut;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::{Value, json};

use super::handlers::{self, Api, Done, Progress};

/// Scripted fake: each call pops the next answer and records `method table`.
#[derive(Default)]
struct Fake {
    answers: Mutex<Vec<Result<Value, ClientError>>>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new(answers: Vec<Result<Value, ClientError>>) -> Self {
        let mut answers = answers;
        answers.reverse();
        Self {
            answers: Mutex::new(answers),
            calls: Mutex::default(),
        }
    }
    fn next(&self, call: String) -> BoxFut<'_, Result<Value, ClientError>> {
        self.calls.lock().expect("lock").push(call);
        let answer = self
            .answers
            .lock()
            .expect("lock")
            .pop()
            .unwrap_or_else(|| Ok(json!([])));
        Box::pin(async move { answer })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }
}

impl Api for Fake {
    fn get_records<'a>(
        &'a self,
        table: &'a str,
        q: &'a RecordQuery,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        self.next(format!(
            "GET {table} q={} l={} f={} o={} ob={}",
            q.query.as_deref().unwrap_or("-"),
            q.limit.as_deref().unwrap_or("-"),
            q.fields.as_deref().unwrap_or("-"),
            q.offset.as_deref().unwrap_or("-"),
            q.order_by.as_deref().unwrap_or("-"),
        ))
    }
    fn get_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        _: &'a RecordParams,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        self.next(format!("GET {table}/{sys_id}"))
    }
    fn create_record<'a>(
        &'a self,
        table: &'a str,
        data: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        self.next(format!("POST {table} {data}"))
    }
    fn update_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        _: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        self.next(format!("PUT {table}/{sys_id}"))
    }
}

fn http(status: u16, message: &str) -> ClientError {
    ClientError::plain(message).with_status(Some(status))
}

fn ok_text(done: &Done) -> &str {
    &done.result.as_ref().expect("ok").text
}

fn err_text(done: &Done) -> String {
    match done.result.as_ref().expect_err("err") {
        ToolError::Message(m) => format!("Error: {m}"),
        ToolError::Upstream { message, .. } => format!("Error: {message}"),
        ToolError::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

const ID1: &str = "0123456789abcdef0123456789abcdef";
const ID2: &str = "fedcba9876543210fedcba9876543210";

type Emitted = Mutex<Vec<(f64, Option<f64>, String)>>;

/// Runs `batch_create` (`create`) or `batch_update`, recording progress.
async fn batch(
    create: bool,
    fake: &Fake,
    args: &Value,
    cancelled: bool,
) -> (Done, Vec<(f64, Option<f64>, String)>) {
    let seen: Emitted = Mutex::default();
    let emit = |p: f64, t: Option<f64>, m: String| -> BoxFut<'_, ()> {
        seen.lock().expect("lock").push((p, t, m));
        Box::pin(async {})
    };
    let mut progress = Progress::new(&emit);
    let is_cancelled = move || cancelled;
    let done = if create {
        handlers::batch_create(fake, args, &mut progress, &is_cancelled).await
    } else {
        handlers::batch_update(fake, args, &mut progress, &is_cancelled).await
    };
    let seen = seen.lock().expect("lock").clone();
    (done, seen)
}

#[tokio::test]
async fn query_table_sends_only_truthy_params_and_counts_results() {
    let fake = Fake::new(vec![Ok(json!([{"a": "1"}]))]);
    let done = handlers::query_table(
        &fake,
        &json!({"table_name": "incident", "offset": 0, "limit": 2.5}),
    )
    .await;
    assert_eq!(fake.calls(), ["GET incident q=- l=2.5 f=- o=- ob=-"]);
    assert_eq!(
        ok_text(&done),
        "Found 1 records in incident:\n[\n  {\n    \"a\": \"1\"\n  }\n]"
    );
    assert_eq!(done.outcome, MutationOutcome::NotAMutation);
}

#[tokio::test]
async fn upstream_errors_keep_the_6_2_0_text() {
    let fake = Fake::new(vec![Err(http(
        403,
        "403 User Not Authorized ACL exception",
    ))]);
    let done = handlers::query_table(&fake, &json!({"table_name": "incident"})).await;
    assert_eq!(
        err_text(&done),
        "Error: 403 User Not Authorized ACL exception"
    );
}

#[tokio::test]
async fn create_and_update_name_the_record_by_key_field() {
    let fake = Fake::new(vec![Ok(json!({"sys_id": "s1", "number": "INC1"}))]);
    let done = handlers::create_record(&fake, &json!({"table_name": "incident", "data": {}})).await;
    assert!(ok_text(&done).starts_with("Created Incident successfully: INC1\n"));
    assert_eq!(done.outcome, MutationOutcome::Committed);

    let fake = Fake::new(vec![Ok(json!({"sys_id": "s1"}))]);
    let done = handlers::create_record(&fake, &json!({"table_name": "u_x", "data": {}})).await;
    assert!(ok_text(&done).starts_with("Created u_x successfully: s1\n"));

    let fake = Fake::new(vec![Ok(json!({}))]);
    let done = handlers::update_record(
        &fake,
        &json!({"table_name": "u_x", "sys_id": "abc", "data": {}}),
    )
    .await;
    assert_eq!(ok_text(&done), "Updated u_x abc successfully\n{}");

    let fake = Fake::new(vec![Err(http(400, "400 bad"))]);
    let done = handlers::update_record(
        &fake,
        &json!({"table_name": "u_x", "sys_id": "abc", "data": {}}),
    )
    .await;
    assert_eq!(done.outcome, MutationOutcome::Failed);
    assert_eq!(err_text(&done), "Error: 400 bad");

    let fake = Fake::new(vec![Err(ClientError::plain("socket hang up"))]);
    let done = handlers::create_record(&fake, &json!({"table_name": "u_x", "data": {}})).await;
    assert_eq!(done.outcome, MutationOutcome::Unknown);
    let fake = Fake::new(vec![Err(ClientError::stale())]);
    let done = handlers::create_record(&fake, &json!({"table_name": "u_x", "data": {}})).await;
    assert_eq!(done.outcome, MutationOutcome::NotSent);
    assert_eq!(
        err_text(&done),
        "Error: Instance changed during credential resolution"
    );
}

#[tokio::test]
async fn get_table_schema_falls_back_to_the_api_then_to_guidance() {
    let fake = Fake::new(vec![
        Ok(json!([{"label": "X"}])),
        Ok(json!([{"element": "u_a", "internal_type": {"value": "string"}}])),
    ]);
    let done = handlers::get_table_schema(&fake, &json!({"table_name": "u_x"})).await;
    let text = ok_text(&done);
    assert!(text.starts_with("Schema for u_x (fetched from ServiceNow API):\n{\n  \"table_name\": \"u_x\",\n  \"label\": \"X\""));
    assert!(text.contains("\"source\": \"live_api\""));

    let fake = Fake::new(vec![Err(http(403, "403 denied"))]);
    let done = handlers::get_table_schema(&fake, &json!({"table_name": "u_x"})).await;
    let reply = done.result.as_ref().expect("guidance is not an error");
    assert!(reply.explicit_false);
    assert_eq!(
        reply.text,
        "No schema metadata found for table \"u_x\" in local cache, and API lookup failed: Failed to discover schema for u_x: 403 denied. The table may not exist or you may not have permissions. Use SN-Query-Table to attempt to query it."
    );

    let fake = Fake::new(vec![]);
    let done = handlers::get_table_schema(&fake, &json!({"table_name": "incident"})).await;
    assert!(ok_text(&done).contains("\"source\": \"local_cache\""));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn list_available_tables_filters_known_categories_only() {
    let all = handlers::list_available_tables(&json!({}));
    assert!(ok_text(&all).starts_with("Available ServiceNow tables (94 total):\n"));
    let unknown = handlers::list_available_tables(&json!({"category": "nope"}));
    assert_eq!(ok_text(&unknown), ok_text(&all));
    let cmdb = handlers::list_available_tables(&json!({"category": "cmdb"}));
    assert!(!ok_text(&cmdb).contains("\"incident\""));
}

#[tokio::test]
async fn discover_and_explain_wrap_errors() {
    let fake = Fake::new(vec![Ok(json!([])), Err(http(500, "500 boom"))]);
    let done = handlers::discover_table_schema(&fake, &json!({"table_name": "t"})).await;
    assert_eq!(
        err_text(&done),
        "Error: Failed to discover schema for t: 500 boom"
    );

    let fake = Fake::new(vec![Ok(json!([]))]);
    let done = handlers::explain_field(&fake, &json!({"table": "t", "field": "f"})).await;
    assert_eq!(
        err_text(&done),
        "Error: Failed to explain field: Field f not found in table t"
    );
}

#[tokio::test]
async fn discover_builds_type_codes_relationships_and_constraints() {
    let fake = Fake::new(vec![
        Ok(json!([{"label": "T", "super_class": {"value": "task"}}])),
        Ok(json!([
            {"element": "type", "column_label": "Type", "internal_type": "integer", "mandatory": "true", "default_value": "1"},
            {"element": "ref", "internal_type": {"value": "reference"}, "reference": {"value": "sys_user"}}
        ])),
        Ok(json!([{"value": "2", "label": "B"}, {"value": "1", "label": "A"}])),
    ]);
    let args = json!({"table_name": "t", "include_type_codes": true, "include_relationships": true, "include_field_constraints": true, "include_choice_tables": true});
    let done = handlers::discover_table_schema(&fake, &args).await;
    let text = ok_text(&done);
    let body: Value = serde_json::from_str(text.split_once('\n').expect("nl").1).expect("json");
    assert_eq!(body["super_class"], "task");
    assert_eq!(body["fields"][0]["type_codes"], json!({"1": "A", "2": "B"}));
    assert_eq!(body["fields"][0]["default_value"], "1");
    assert_eq!(body["fields"][0]["mandatory"], true);
    assert_eq!(
        body["relationships"]["ref"],
        json!({"type": "reference", "table": "sys_user"})
    );
    // JS key order: integer-like keys first, ascending.
    assert!(text.contains("\"type_codes\": {\n        \"1\": \"A\",\n        \"2\": \"B\""));
    let keys: Vec<&String> = body.as_object().expect("obj").keys().collect();
    assert_eq!(
        keys,
        [
            "table",
            "label",
            "fields",
            "super_class",
            "choice_tables",
            "relationships"
        ]
    );
}

#[tokio::test]
async fn explain_field_reports_known_issues() {
    let fake = Fake::new(vec![Ok(
        json!([{"column_label": "Var", "internal_type": "string"}]),
    )]);
    let args = json!({"table": "catalog_ui_policy_action", "field": "catalog_variable", "include_examples": false});
    let done = handlers::explain_field(&fake, &args).await;
    let text = ok_text(&done);
    assert!(text.contains("Must include IO: prefix"));
    assert!(text.contains("\"special_format\": \"IO:<variable_sys_id>\""));
    assert!(!text.contains("\"example\""));
}

#[tokio::test]
async fn natural_language_search_reports_parse_failures_and_input_errors() {
    let fake = Fake::new(vec![]);
    let done =
        handlers::natural_language_search(&fake, &json!({"query": "zzz", "show_patterns": false}))
            .await;
    assert!(ok_text(&done).starts_with("❌ Unable to parse query: \"zzz\"\n\n"));
    assert!(ok_text(&done).ends_with("Unmatched text: \"zzz\"\n\n"));
    assert!(fake.calls().is_empty());

    let done = handlers::natural_language_search(&fake, &json!({"query": "x".repeat(2049)})).await;
    assert_eq!(
        err_text(&done),
        "Error: Natural language query must be at most 2048 characters (received 2049)"
    );

    let fake = Fake::new(vec![Ok(json!([]))]);
    let done = handlers::natural_language_search(
        &fake,
        &json!({"query": "P1 incidents", "table": "problem"}),
    )
    .await;
    assert!(ok_text(&done).contains("**Target Table:** problem"));
    assert!(ok_text(&done).ends_with("## No records found matching the query.\n\nTry adjusting your search criteria or use SN-Query-Table for more control."));
    assert_eq!(fake.calls(), ["GET problem q=priority=1 l=25 f=- o=- ob=-"]);
}

#[tokio::test]
async fn batch_create_resolves_refs_and_reports_progress() {
    let fake = Fake::new(vec![Ok(json!({"sys_id": ID1})), Ok(json!({"sys_id": ID2}))]);
    let args = json!({"operations": [
        {"table": "incident", "data": {"a": "x"}, "save_as": "p"},
        {"table": "incident", "data": {"parent": "${p}"}}
    ]});
    let (done, seen) = batch(true, &fake, &args, false).await;
    assert_eq!(done.outcome, MutationOutcome::Committed);
    let reply = done.result.as_ref().expect("ok");
    assert!(reply.explicit_false);
    assert!(reply.text.contains(&format!(
        "\"p\": \"{ID1}\",\n    \"operation_1\": \"{ID2}\""
    )));
    let messages: Vec<&str> = seen.iter().map(|s| s.2.as_str()).collect();
    assert_eq!(
        messages,
        [
            "Creating record 1/2 (50%): incident",
            "Creating record 2/2 (100%): incident",
            "Complete: All 2 records created successfully"
        ]
    );
    assert_eq!(seen[2].0, 3.0);
    assert_eq!(seen[2].1, None);
}

#[tokio::test]
async fn batch_create_transaction_stops_without_rollback_and_is_partial() {
    let fake = Fake::new(vec![
        Ok(json!({"sys_id": ID1})),
        Err(http(403, "403 Insert Operation Failed: ACL")),
    ]);
    let args = json!({"operations": [
        {"table": "incident", "data": {}}, {"table": "problem", "data": {}}, {"table": "problem", "data": {}}
    ], "progress": false});
    let (done, seen) = batch(true, &fake, &args, false).await;
    assert!(seen.is_empty());
    assert_eq!(done.outcome, MutationOutcome::PartiallyCommitted);
    let outcomes: Vec<_> = done.ops.iter().map(|o| o.outcome).collect();
    assert_eq!(
        outcomes,
        [
            MutationOutcome::Committed,
            MutationOutcome::Failed,
            MutationOutcome::NotSent
        ]
    );
    assert_eq!(
        err_text(&done),
        "Error: Batch create failed at operation 1: 403 Insert Operation Failed: ACL"
    );
}

#[tokio::test]
async fn batch_create_without_transaction_reports_failed_body() {
    let fake = Fake::new(vec![Err(http(400, "400 nope")), Ok(json!({"sys_id": ID1}))]);
    let args = json!({"operations": [{"table": "a", "data": {}}, {"table": "b", "data": {}}], "transaction": false, "progress": false});
    let (done, _) = batch(true, &fake, &args, false).await;
    let text = err_text(&done);
    assert!(
        text.starts_with("Batch create failed:\n{\n  \"success\": false,\n  \"created_count\": 1,")
    );
    assert!(text.contains(
        "\"operation_index\": 0,\n      \"table\": \"a\",\n      \"error\": \"400 nope\""
    ));
    assert_eq!(done.outcome, MutationOutcome::PartiallyCommitted);
}

#[tokio::test]
async fn batch_validation_errors_send_nothing() {
    let fake = Fake::new(vec![]);
    let args = json!({"operations": [{"table": "bad table", "data": {}}]});
    let (done, _) = batch(true, &fake, &args, false).await;
    assert_eq!(done.outcome, MutationOutcome::NotSent);
    assert!(err_text(&done).starts_with("Error: operations[0].table must be a table name"));

    let fake = Fake::new(vec![]);
    let args = json!({"updates": [{"table": "bad table", "sys_id": ID1, "data": {}}]});
    let (done, _) = batch(false, &fake, &args, false).await;
    assert_eq!(done.outcome, MutationOutcome::NotSent);
    assert!(err_text(&done).starts_with("Error: updates[0].table must be a table name"));
}

#[tokio::test]
async fn batch_update_continues_or_stops_on_error() {
    let updates = json!([
        {"table": "incident", "sys_id": ID1, "data": {"state": "2"}},
        {"table": "incident", "sys_id": ID2, "data": {"state": "3"}},
        {"table": "incident", "sys_id": ID1, "data": {"state": "4"}}
    ]);
    let answers = || {
        vec![
            Ok(json!({})),
            Err(http(404, "404 No Record found")),
            Ok(json!({})),
        ]
    };

    let fake = Fake::new(answers());
    let args = json!({"updates": updates, "stop_on_error": true});
    let (done, seen) = batch(false, &fake, &args, false).await;
    assert_eq!(done.outcome, MutationOutcome::PartiallyCommitted);
    assert!(err_text(&done).starts_with(
        "Batch update completed with errors:\n{\n  \"success\": false,\n  \"updated_count\": 1,"
    ));
    let messages: Vec<&str> = seen.iter().map(|s| s.2.as_str()).collect();
    assert_eq!(
        messages,
        [
            "Updating record 1/3 (33%): incident",
            "Failed 2/3: incident - 404 No Record found",
            "Complete: 1/3 records updated (1 failed)"
        ]
    );

    let fake = Fake::new(answers());
    let args = json!({"updates": updates});
    let (done, _) = batch(false, &fake, &args, false).await;
    assert!(err_text(&done).contains("\"updated_count\": 2,"));
    let outcomes: Vec<_> = done.ops.iter().map(|o| o.outcome).collect();
    assert_eq!(
        outcomes,
        [
            MutationOutcome::Committed,
            MutationOutcome::Failed,
            MutationOutcome::Committed
        ]
    );
}

#[tokio::test]
async fn batch_stops_before_the_next_write_once_cancelled() {
    let fake = Fake::new(vec![Ok(json!({}))]);
    let args = json!({"updates": [{"table": "incident", "sys_id": ID1, "data": {}}]});
    let (done, _) = batch(false, &fake, &args, true).await;
    assert!(matches!(done.result, Err(ToolError::Cancelled)));
    assert_eq!(done.outcome, MutationOutcome::NotSent);
}

#[test]
fn every_lane_a_tool_registers_with_its_d05_class() {
    use std::sync::Arc;

    use hpm_core::client::ClientDeps;
    use hpm_core::config::ConfigManager;
    use hpm_core::credential::provider::InstanceClients;
    use hpm_core::keychain::LazySecretStore;

    use crate::families::FamilyDeps;
    use crate::registration::ToolRegistry;

    let config = Arc::new(ConfigManager::from_process_env());
    let deps = ClientDeps::production(Arc::new(LazySecretStore::os())).expect("client deps");
    let store = deps.credential_store.clone();
    let clients = Arc::new(InstanceClients::from_config(config.clone(), deps));
    let family = FamilyDeps::new(clients, config, store);
    let mut registry = ToolRegistry::new().expect("registry");
    super::register(&mut registry, &family).expect("register");
    assert_eq!(registry.tools().len(), 11);
    assert!(registry.tools().iter().all(|t| t.needs_instance));
}
