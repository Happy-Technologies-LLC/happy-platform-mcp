//! SN-Execute-Background-Script sys_trigger fallback (review recheck M1): a fix
//! script only when the trigger insert certainly never landed; an unknown
//! outcome names the trigger and says not to re-run. The fallback is opt-in
//! (owner decision P4-2): without consent nothing is inserted.

#[allow(dead_code, reason = "support is shared with the golden tests")]
mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::harness::{self, World};
use support::scenarios::{Scenario, processor_stub};
use support::stub_sn::{Answer, json_answer};

const UNKNOWN: &str = "The request reached ServiceNow but its result could not be confirmed.";

/// The processor refuses (403) so the call falls back to sys_trigger; `trigger`
/// answers the sys_trigger requests it wants to fail.
fn scenario(trigger: fn(&str, &str) -> Option<Answer>) -> Scenario {
    let processor = processor_stub(|| None, 403);
    Scenario {
        stub: Some(Arc::new(move |req, state| {
            trigger(&req.method, &req.path).or_else(|| processor(req, state))
        })),
        ..Scenario::default()
    }
}

async fn run(id: &str, sc: Scenario) -> (String, bool, Value, usize) {
    let world = World::start().await;
    let r = harness::call_tool(
        &world,
        id,
        sc,
        "SN-Execute-Background-Script",
        json!({"script": "gs.info('x');", "allow_trigger_fallback": true}),
    )
    .await
    .expect("call");
    let result = &r.response["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let posts = r
        .outbound
        .iter()
        .filter(|d| d.method == "POST" && d.path == "/api/now/table/sys_trigger")
        .count();
    (text, result["isError"] == true, r.files, posts)
}

fn assert_unknown(text: &str, trigger: &str, files: &Value) {
    assert!(text.contains(UNKNOWN), "{text}");
    assert!(text.contains(&format!("trigger {trigger}")), "{text}");
    assert!(text.contains("Do NOT re-run the script"), "{text}");
    assert!(text.contains("Scheduled Jobs"), "{text}");
    assert!(text.contains("System Log"), "{text}");
    assert!(!text.contains("fix script"), "{text}");
    assert_eq!(files, &json!({}), "no fix script may be written");
}

#[tokio::test]
async fn wrapper_put_500_is_unknown_and_names_the_trigger() {
    let (text, is_error, files, posts) = run(
        "flow/trigger-put-500",
        scenario(|m, p| {
            (m == "PUT" && p.starts_with("/api/now/table/sys_trigger/"))
                .then(|| json_answer(500, json!({"error": {"message": "boom"}})))
        }),
    )
    .await;
    assert!(is_error);
    assert_eq!(posts, 1, "the script is never re-submitted");
    assert!(text.contains("trigger sys_id "), "{text}");
    let id = text
        .split("trigger sys_id ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    assert!(!id.is_empty() && id != "undefined", "{text}");
    assert_unknown(&text, &format!("sys_id {id}"), &files);
}

#[tokio::test]
async fn insert_5xx_is_unknown_and_names_the_trigger() {
    let (text, is_error, files, posts) = run(
        "flow/trigger-insert-503",
        scenario(|m, p| {
            (m == "POST" && p == "/api/now/table/sys_trigger")
                .then(|| json_answer(503, json!({"error": {"message": "busy"}})))
        }),
    )
    .await;
    assert!(is_error);
    assert_eq!(posts, 1);
    assert_unknown(&text, "name MCP_Script_", &files);
}

#[tokio::test]
async fn definite_insert_refusal_still_writes_the_fix_script() {
    let (text, is_error, files, posts) = run(
        "flow/trigger-insert-403",
        scenario(|m, p| {
            (m == "POST" && p == "/api/now/table/sys_trigger")
                .then(|| json_answer(403, json!({"error": {"message": "ACL"}})))
        }),
    )
    .await;
    assert!(!is_error, "{text}");
    assert_eq!(posts, 1);
    assert!(
        text.contains("Created fix script for manual execution"),
        "{text}"
    );
    assert!(
        files
            .as_object()
            .is_some_and(|f| f.keys().any(|k| k.contains("background_script_"))),
        "{files}"
    );
}

#[tokio::test]
async fn fallback_is_refused_without_opt_in() {
    let world = World::start().await;
    let r = harness::call_tool(
        &world,
        "no-opt-in",
        scenario(|_, _| None),
        "SN-Execute-Background-Script",
        json!({"script": "gs.info('x');"}),
    )
    .await
    .expect("call");
    let result = &r.response["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(result["isError"], true, "{text}");
    assert!(text.contains("was NOT run"), "{text}");
    assert!(text.contains("allow_trigger_fallback: true"), "{text}");
    assert!(
        text.contains("HAPPY_MCP_ALLOW_TRIGGER_FALLBACK=1"),
        "{text}"
    );
    assert!(
        !r.outbound
            .iter()
            .any(|d| d.path.starts_with("/api/now/table/sys_trigger")),
        "no sys_trigger request without opt-in"
    );
    assert_eq!(r.files, json!({}), "no fix script without opt-in");
}

/// Opt-in success path end to end (review L-2): the stub answers syslog with
/// this run's `MARKER:LOG` lines, another run's line and the END marker.
fn succeeding_trigger(env: Vec<(&'static str, &'static str)>) -> Scenario {
    let processor = processor_stub(|| None, 403);
    Scenario {
        stub: Some(Arc::new(move |req, state| {
            if req.method == "GET"
                && req.path == "/api/now/table/syslog"
                && let Some(marker) = state.trigger_marker.clone()
            {
                let end = json!({"message": format!("{marker}:END{}", json!({"ok": true})), "sys_created_on": "2099-01-01 00:00:01"});
                let end_poll = req
                    .query
                    .iter()
                    .any(|(k, v)| k == "sysparm_query" && v.starts_with("messageLIKE"));
                let rows = if end_poll {
                    json!([end])
                } else {
                    json!([
                        {"message": format!("{marker}:LOG hello from this run"), "sys_created_on": "2099-01-01 00:00:00"},
                        {"message": "MCP_other:LOG someone else", "sys_created_on": "2099-01-01 00:00:00"},
                        end
                    ])
                };
                return Some(json_answer(200, json!({"result": rows})));
            }
            processor(req, state)
        })),
        env,
        ..Scenario::default()
    }
}

async fn run_success(id: &str, sc: Scenario, args: Value) -> (String, bool, usize) {
    let world = World::start().await;
    let r = harness::call_tool(&world, id, sc, "SN-Execute-Background-Script", args)
        .await
        .expect("call");
    let result = &r.response["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let posts = r
        .outbound
        .iter()
        .filter(|d| d.method == "POST" && d.path == "/api/now/table/sys_trigger")
        .count();
    assert_eq!(r.files, json!({}), "no fix script on success");
    (text, result["isError"] == true, posts)
}

fn assert_trigger_success(text: &str, is_error: bool, posts: usize) {
    assert!(!is_error, "{text}");
    assert_eq!(posts, 1, "{text}");
    assert!(text.contains("Script executed successfully!"), "{text}");
    assert!(text.contains("hello from this run"), "{text}");
    assert!(
        !text.contains("someone else"),
        "foreign run captured: {text}"
    );
}

#[tokio::test]
async fn opt_in_argument_runs_the_trigger_and_renders_this_runs_logs() {
    let (text, is_error, posts) = run_success(
        "flow/trigger-opt-in-arg",
        succeeding_trigger(Vec::new()),
        json!({"script": "gs.info('x');", "allow_trigger_fallback": true}),
    )
    .await;
    assert_trigger_success(&text, is_error, posts);
}

#[tokio::test]
async fn opt_in_environment_runs_the_trigger_without_the_argument() {
    let (text, is_error, posts) = run_success(
        "flow/trigger-opt-in-env",
        succeeding_trigger(vec![("HAPPY_MCP_ALLOW_TRIGGER_FALLBACK", "1")]),
        json!({"script": "gs.info('x');"}),
    )
    .await;
    assert_trigger_success(&text, is_error, posts);
}
