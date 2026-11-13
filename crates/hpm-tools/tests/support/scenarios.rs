//! Per-golden setup that the golden files do not record, ported from
//! `scripts/golden/scenarios/tools.mjs`: stub route overrides, instance registries,
//! keyring seeds, extra environment, and original arguments where the capture
//! normalized the recorded request.

use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Value, json};

use super::http::Described;
use super::stub_sn::{Answer, Body, Routes, State, json_answer, sys_id};

pub const FAKE_PASSWORD: &str = "golden-fake-password-4f9a1c";
pub const FAKE_USER: &str = "golden.user";

/// What a registry-mode scenario's `instances(world)` needs.
pub struct Endpoints {
    pub origin: String,
    pub port: u16,
}

#[derive(Default)]
pub struct Scenario {
    pub stub: Option<Routes>,
    /// `instances(world)`: the registry file content (`HAPPY_CONFIG_PATH`).
    pub instances: Option<fn(&Endpoints) -> Value>,
    /// Keyring seed: account → secret (service `happy-platform-mcp`).
    pub keyring: Vec<(&'static str, &'static str)>,
    /// Origin bindings for keyring accounts saved for an instance other than
    /// the stub (account → origin). Every other account is bound to the stub.
    pub bindings: Vec<(&'static str, &'static str)>,
    pub env: Vec<(&'static str, &'static str)>,
    /// `params` per step, for goldens whose recorded request was normalized.
    pub requests: Option<Vec<Value>>,
    /// Steps whose ServiceNow requests must reach the `prod` instance
    /// (`localhost:P`, `{{INSTANCE_HOST_ALT}}`). Every other step must reach the
    /// default (`127.0.0.1:P`, `{{INSTANCE_HOST}}`). The capture never recorded the
    /// host, so this is how the routing goldens prove the instance choice.
    pub prod_steps: &'static [usize],
}

fn u(n: u32) -> String {
    sys_id("arg", n)
}

fn routes(f: impl Fn(&Described, &mut State) -> Option<Answer> + Send + Sync + 'static) -> Routes {
    Arc::new(f)
}

fn error_body(message: &str, detail: &str) -> Value {
    json!({"error": {"message": message, "detail": detail}, "status": "failure"})
}

fn with_headers(mut a: Answer, headers: &[(&str, &str)]) -> Answer {
    a.headers = headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    a
}

fn history_link(id: Option<&str>) -> String {
    id.map(|id| format!("<A HREF=\"sys_script_execution_history.do?sys_id={id}&amp;sysparm_view=\">Script execution history and recovery available here</A><BR/>")).unwrap_or_default()
}

fn processor_page(scope: &str, lines: &[&str], history_id: Option<&str>) -> String {
    let pre: String = lines.iter().map(|l| format!("{l}<BR/>")).collect();
    format!(
        "<HTML><BODY><HR/><PRE>{pre}</PRE><HR/>[0:00:00.004] Script completed in scope {scope}: script<BR/>{}</BODY></HTML>",
        history_link(history_id)
    )
}

const SESSION_ID: &str = "GOLDENSESSION";
const CSRF: &str = "golden-csrf-token";

static TRIGGER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gs\.info\('([^']+):END'").expect("trigger re"));

/// `processorStub(page, processorStatus)`: `/sys.scripts.do` protocol routes.
pub fn processor_stub(page: fn() -> Option<String>, processor_status: u16) -> Routes {
    routes(move |req, state| {
        let cookie = format!("glide_session_store={SESSION_ID}; Path=/; HttpOnly");
        let m = req.method.as_str();
        let p = req.path.as_str();
        if m == "POST" && p == "/angular.do" {
            return Some(with_headers(
                json_answer(200, json!({})),
                &[("set-cookie", &cookie)],
            ));
        }
        if m == "GET" && p == "/api/now/ui/user/current_user" {
            return Some(with_headers(
                json_answer(200, json!({"result": {"user_name": "golden.user"}})),
                &[("set-cookie", &cookie)],
            ));
        }
        if m == "GET" && p == "/api/now/table/sys_user_session" {
            return Some(json_answer(
                200,
                json!({"result": [{"csrf_token": CSRF, "id": SESSION_ID}]}),
            ));
        }
        if m == "POST" && p == "/sys.scripts.do" {
            return Some(if processor_status == 200 {
                Answer {
                    status: 200,
                    body: Body::Text(page().unwrap_or_default()),
                    headers: Vec::new(),
                }
            } else {
                Answer {
                    status: processor_status,
                    body: Body::Text("forbidden".into()),
                    headers: Vec::new(),
                }
            });
        }
        if m == "PUT" && p.starts_with("/api/now/table/sys_trigger/") {
            let script = req
                .body
                .as_ref()
                .and_then(|b| b.get("script"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(c) = TRIGGER_RE.captures(script) {
                state.trigger_marker = Some(c[1].to_owned());
            }
            let id = p.rsplit('/').next().unwrap_or("");
            return Some(json_answer(200, json!({"result": {"sys_id": id}})));
        }
        if m == "GET"
            && p == "/api/now/table/syslog"
            && let Some(marker) = &state.trigger_marker
        {
            return Some(json_answer(
                200,
                json!({"result": [{"message": format!("{marker}:END{}", json!({"ok": true})), "sys_created_on": "2099-01-01 00:00:00"}]}),
            ));
        }
        if m == "GET" && p == "/api/now/table/sys_scope" {
            return Some(json_answer(
                200,
                json!({"result": [{"sys_id": sys_id("scope", 1), "scope": "x_golden_app1", "name": "Golden App 1", "active": "true"}]}),
            ));
        }
        if m == "GET" && p == "/api/now/table/sys_script_execution_history" {
            return Some(json_answer(
                200,
                json!({"result": [{"rollback_context": {"value": sys_id("rollback", 1), "link": "{{x}}"}}]}),
            ));
        }
        None
    })
}

fn atf_progress() -> String {
    sys_id("atf-progress", 1)
}

fn atf_result() -> String {
    sys_id("atf-result", 1)
}

fn atf_routes(req: &Described) -> Option<Answer> {
    let p = req.path.as_str();
    let progress = atf_progress();
    let result = atf_result();
    if req.method == "POST"
        && (p == "/api/sn_cicd/tests/run_test" || p == "/api/sn_cicd/testsuite/run")
    {
        return Some(json_answer(
            200,
            json!({"result": {"status": "0", "links": {"progress": {"id": progress, "url": format!("/api/sn_cicd/progress/{progress}")}}}}),
        ));
    }
    if p == format!("/api/sn_cicd/progress/{progress}") {
        return Some(json_answer(
            200,
            json!({"result": {"status": "2", "percent_complete": 100, "status_label": "Successful", "status_message": "Done", "links": {"results": {"id": result}}}}),
        ));
    }
    if p == format!("/api/sn_cicd/tests/test/results/{result}") {
        return Some(json_answer(
            200,
            json!({"result": {"test_name": "Golden test", "test_status": "success", "output": "all good"}}),
        ));
    }
    if p == format!("/api/sn_cicd/tests/test/results/{result}/logs") {
        return Some(json_answer(
            200,
            json!({"result": {"logs": ["INFO step 1 ok", "ERROR step 2 golden"]}}),
        ));
    }
    if p == format!("/api/sn_cicd/testsuite/results/{result}") {
        return Some(json_answer(
            200,
            json!({"result": {"test_suite_name": "Golden Suite", "test_suite_status": "success", "rolledup_test_success_count": 2, "rolledup_test_failure_count": 0, "rolledup_test_error_count": 0, "rolledup_test_skip_count": 0}}),
        ));
    }
    None
}

fn registry(e: &Endpoints) -> Value {
    json!({"version": 1, "instances": [
        {"name": "dev", "url": e.origin, "username": FAKE_USER, "password": FAKE_PASSWORD, "description": "Golden dev", "default": true},
        {"name": "prod", "url": format!("http://localhost:{}", e.port), "username": FAKE_USER, "password": FAKE_PASSWORD, "description": "Golden prod"}
    ]})
}

fn ref_registry(e: &Endpoints) -> Value {
    json!({"version": 1, "instances": [
        {"name": "dev", "url": e.origin, "username": FAKE_USER, "credentialRef": "keychain:instance/dev/password", "description": "Golden dev", "default": true}
    ]})
}

/// Scenario setup for golden id `<tool>/<case>`. Unknown ids get the defaults
/// (default routes, env credentials).
pub fn scenario(id: &str) -> Scenario {
    let mut s = Scenario::default();
    match id {
        "SN-Query-Table/http-403" => {
            s.stub = Some(routes(|req, _| {
                req.path
                    .starts_with("/api/now/table/")
                    .then(|| json_answer(403, error_body("User Not Authorized", "ACL exception")))
            }));
        }
        "SN-Get-Record/http-404" => {
            s.stub = Some(routes(|req, _| {
                (req.method == "GET").then(|| {
                    json_answer(
                        404,
                        error_body(
                            "No Record found",
                            "Record doesn't exist or ACL restricts the record retrieval",
                        ),
                    )
                })
            }));
        }
        "SN-Set-Update-Set/picker-fails-fix-script" => {
            s.stub = Some(routes(|req, _| {
                req.path
                    .starts_with("/api/now/ui/concoursepicker")
                    .then(|| json_answer(500, error_body("picker unavailable", "golden")))
            }));
        }
        "SN-Move-Records-To-Update-Set/by-time-range" => {
            s.requests = Some(vec![
                json!({"name": "SN-Move-Records-To-Update-Set", "arguments": {
                    "update_set_id": u(10), "source_update_set": "Default",
                    "time_range": {"start": "2026-01-01 00:00:00", "end": "2026-01-01 01:00:00"}
                }}),
            ]);
        }
        "SN-Execute-Background-Script/processor-success" => {
            s.stub = Some(processor_stub(
                || {
                    Some(processor_page(
                        "global",
                        &["*** Script: golden background"],
                        Some(&sys_id("hist", 1)),
                    ))
                },
                200,
            ));
        }
        "SN-Execute-Background-Script/processor-script-error" => {
            s.stub = Some(processor_stub(
                || {
                    Some(processor_page(
                        "global",
                        &["Script compilation error: Unexpected token ;"],
                        Some(&sys_id("hist", 2)),
                    ))
                },
                200,
            ));
        }
        "SN-Execute-Background-Script/processor-unauthorized-trigger-fallback" => {
            s.stub = Some(processor_stub(|| None, 403));
        }
        "SN-Execute-Background-Script/scoped-run" => {
            s.stub = Some(processor_stub(
                || {
                    Some(processor_page(
                        "x_golden_app1",
                        &["x_golden_app1: scoped"],
                        None,
                    ))
                },
                200,
            ));
        }
        "SN-Add-Work-Notes/incident-not-found" => {
            s.stub = Some(routes(|req, _| {
                (req.method == "GET" && req.path == "/api/now/table/incident")
                    .then(|| json_answer(200, json!({"result": []})))
            }));
        }
        "SN-Batch-Create/failure-rollback" => {
            s.stub = Some(routes(|req, _| {
                (req.method == "POST" && req.path == "/api/now/table/problem")
                    .then(|| json_answer(403, error_body("Insert Operation Failed: ACL", "golden")))
            }));
        }
        "SN-Batch-Update/stop-on-error" => {
            s.stub = Some(routes(|req, _| {
                ((req.method == "PATCH" || req.method == "PUT") && req.path.ends_with(&u(3)))
                    .then(|| json_answer(404, error_body("No Record found", "golden")))
            }));
        }
        "SN-Catalog-Submit/basic" => {
            s.stub = Some(routes(|req, _| {
                (req.method == "POST" && req.path.ends_with("/order_now")).then(|| {
                    json_answer(
                        200,
                        json!({"result": {"sys_id": sys_id("request", 1), "number": "REQ0010001", "request_number": "REQ0010001", "request_id": sys_id("request", 1), "table": "sc_request"}}),
                    )
                })
            }));
        }
        "SN-Catalog-Submit/http-400" => {
            s.stub = Some(routes(|req, _| {
                (req.method == "POST" && req.path.ends_with("/order_now"))
                    .then(|| json_answer(400, error_body("Mandatory variables missing", "golden")))
            }));
        }
        "SN-Run-ATF-Test/by-sys-id-wait"
        | "SN-Run-ATF-Suite/by-sys-id-wait"
        | "SN-Get-CICD-Progress/single-read"
        | "SN-Get-ATF-Results/test"
        | "SN-Get-ATF-Results/suite"
        | "SN-Get-ATF-Test-Logs/pattern-limit" => {
            s.stub = Some(routes(|req, _| atf_routes(req)));
        }
        "SN-Run-ATF-Test/by-name-no-wait" => {
            s.stub = Some(routes(|req, _| {
                if req.path == "/api/now/table/sys_atf_test" {
                    Some(json_answer(
                        200,
                        json!({"result": [{"sys_id": u(50), "name": "Golden test"}]}),
                    ))
                } else {
                    atf_routes(req)
                }
            }));
        }
        "SN-Get-CICD-Progress/http-403" => {
            s.stub = Some(routes(|req, _| {
                req.path
                    .starts_with("/api/sn_cicd")
                    .then(|| json_answer(403, error_body("User is not authorized", "golden")))
            }));
        }
        "SN-Query-Table/per-call-instance-override" => {
            s.instances = Some(registry);
            s.prod_steps = &[0];
        }
        "SN-Set-Instance/switch-then-current" => {
            s.instances = Some(registry);
            s.prod_steps = &[2];
        }
        "SN-Get-Current-Instance/registry-default"
        | "SN-Set-Instance/list"
        | "SN-Set-Instance/unknown-instance"
        | "SN-Query-Table/unknown-instance-override"
        | "SN-Register-Instance/secret-field-rejected"
        | "SN-Register-Instance/insecure-url-rejected" => {
            s.instances = Some(registry);
        }
        "SN-Register-Instance/basic-with-stored-credential" => {
            s.instances = Some(ref_registry);
            s.keyring = vec![
                ("keychain:instance/dev/password", FAKE_PASSWORD),
                ("keychain:instance/staging/password", FAKE_PASSWORD),
            ];
            // The operator saved the staging secret for the staging URL.
            s.bindings = vec![(
                "keychain:instance/staging/password",
                "https://staging.example.invalid",
            )];
        }
        "SN-Docs-Sync/local-index-enabled" => {
            s.env = vec![("HAPPY_DOCS_ENABLE_LOCAL_INDEX", "true")];
        }
        _ => {}
    }
    if s.requests.is_none() {
        s.requests = insertion_order_args(id).map(|r| vec![r]);
    }
    s
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"name": name, "arguments": arguments})
}

/// RH-1: the JS scenario's `arguments` in their original insertion order, for
/// goldens whose arguments hold multi-key objects. The golden file records the
/// keys sorted; a port that iterates an argument object (to build a body or a
/// query) must see the order the 6.2.0 capture actually sent.
fn insertion_order_args(id: &str) -> Option<Value> {
    let (tool, _) = id.split_once('/')?;
    let args = match id {
        "SN-Create-Record/basic" => json!({"table_name": "incident",
            "data": {"short_description": "golden create", "urgency": "2"}}),
        "SN-Update-Record/basic" => json!({"table_name": "incident", "sys_id": u(1),
            "data": {"state": "2", "work_notes": "golden update"}}),
        "SN-Batch-Create/basic-with-refs" => json!({"operations": [
            {"table": "incident", "data": {"short_description": "golden batch parent"}, "save_as": "parent"},
            {"table": "incident", "data": {"short_description": "golden batch child", "parent_incident": "${parent}"}}
        ], "transaction": true, "progress": true}),
        "SN-Batch-Create/failure-rollback" => json!({"operations": [
            {"table": "incident", "data": {"short_description": "golden ok"}},
            {"table": "problem", "data": {"short_description": "golden fails"}}
        ], "transaction": true}),
        "SN-Batch-Update/basic" => json!({"updates": [
            {"table": "incident", "sys_id": u(1), "data": {"state": "2"}},
            {"table": "incident", "sys_id": u(2), "data": {"state": "3"}}
        ], "stop_on_error": false, "progress": true}),
        "SN-Batch-Update/stop-on-error" => json!({"updates": [
            {"table": "incident", "sys_id": u(1), "data": {"state": "2"}},
            {"table": "incident", "sys_id": u(3), "data": {"state": "3"}},
            {"table": "incident", "sys_id": u(4), "data": {"state": "4"}}
        ], "stop_on_error": true}),
        "SN-Catalog-Submit/basic" => json!({"sys_id": u(30),
            "variables": {"model": "golden-laptop", "justification": "golden"},
            "quantity": 2, "requested_for": u(31)}),
        "SN-Validate-Configuration/basic" => json!({"catalog_item": u(30),
            "checks": {"variables": true, "ui_policies": true, "business_rules": true}}),
        "SN-Create-Workflow/two-activities" => json!({"name": "Golden Workflow",
            "table": "incident", "description": "golden", "condition": "active=true",
            "activities": [{"name": "Start", "script": "answer = true;"},
                {"name": "Finish", "script": "gs.info(\"done\");"}],
            "transitions": [{"from": 0, "to": 1, "condition": "true"}],
            "publish": true, "progress": true}),
        _ => return None,
    };
    Some(call(tool, args))
}
