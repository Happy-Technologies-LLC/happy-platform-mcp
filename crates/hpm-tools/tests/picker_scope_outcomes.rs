//! SN-Set-Update-Set failure phases (review TF-10, M-1, M-2; P3 decision 12)
//! and the SN-Execute-Background-Script scope mismatch (owner decision P4-1,
//! review L-1), end to end through the tool.

#[allow(dead_code, reason = "support is shared with the golden tests")]
mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::harness::{self, World};
use support::scenarios::{Scenario, processor_stub};
use support::stub_sn::{Answer, Body, json_answer};

const SET: &str = "ed62390adb712ed83486d9105c442b81";
const OTHER: &str = "0123456789abcdef0123456789abcdef";
const MAYBE_CURRENT: &str = "may already be your current update set";

/// Happy-path picker stub; `fault(method, path, nth picker GET)` overrides.
fn picker(fault: fn(&str, &str, usize) -> Option<Answer>) -> Scenario {
    let gets = Arc::new(std::sync::Mutex::new(0usize));
    Scenario {
        stub: Some(Arc::new(move |req, _state| {
            let (m, p) = (req.method.as_str(), req.path.as_str());
            let nth = if m == "GET" && p == "/api/now/ui/concoursepicker/updateset" {
                let mut n = gets.lock().unwrap_or_else(|e| e.into_inner());
                *n += 1;
                *n
            } else {
                0
            };
            if let Some(a) = fault(m, p, nth) {
                return Some(a);
            }
            match (m, p) {
                ("GET", "/") => Some(Answer {
                    status: 200,
                    body: Body::Text("<html></html>".into()),
                    headers: vec![("set-cookie".into(), "JSESSIONID=s1; Path=/".into())],
                }),
                ("GET", "/api/now/ui/concoursepicker/updateset") => Some(json_answer(
                    200,
                    json!({"result": {"current": {"sysId": SET, "name": "Story set"}}}),
                )),
                ("PUT", "/api/now/ui/concoursepicker/updateset") => {
                    Some(json_answer(200, json!({"result": {"success": true}})))
                }
                ("GET", p) if p == format!("/api/now/table/sys_update_set/{SET}") => Some(
                    json_answer(200, json!({"result": {"sys_id": SET, "name": "Story set"}})),
                ),
                _ => None,
            }
        })),
        ..Scenario::default()
    }
}

async fn set_update_set(id: &str, sc: Scenario) -> (String, bool, Value) {
    let world = World::start().await;
    let r = harness::call_tool(
        &world,
        id,
        sc,
        "SN-Set-Update-Set",
        json!({"update_set_sys_id": SET}),
    )
    .await
    .expect("call");
    let result = &r.response["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    (text, result["isError"] == true, r.files)
}

fn has_fix_script(files: &Value) -> bool {
    files
        .as_object()
        .is_some_and(|f| f.keys().any(|k| k.contains("set_update_set_")))
}

#[tokio::test]
async fn verify_read_403_after_put_is_unknown_without_fix_script() {
    let (text, is_error, files) = set_update_set(
        "picker/verify-403",
        picker(|_, _, nth| {
            (nth == 2).then(|| json_answer(403, json!({"error": {"message": "ACL"}})))
        }),
    )
    .await;
    assert!(is_error, "{text}");
    assert!(text.contains(MAYBE_CURRENT), "{text}");
    assert!(text.contains("SN-Get-Current-Update-Set"), "{text}");
    assert!(!text.contains("not available"), "{text}");
    assert!(!has_fix_script(&files), "{files}");
}

#[tokio::test]
async fn put_500_is_unknown_without_fix_script() {
    let (text, is_error, files) = set_update_set(
        "picker/put-500",
        picker(|m, _, _| {
            (m == "PUT").then(|| json_answer(500, json!({"error": {"message": "boom"}})))
        }),
    )
    .await;
    assert!(is_error, "{text}");
    assert!(text.contains(MAYBE_CURRENT), "{text}");
    assert!(!has_fix_script(&files), "{files}");
}

#[tokio::test]
async fn put_403_is_refused_and_writes_the_fix_script() {
    let (text, _, files) = set_update_set(
        "picker/put-403",
        picker(|m, _, _| {
            (m == "PUT").then(|| json_answer(403, json!({"error": {"message": "ACL"}})))
        }),
    )
    .await;
    assert!(text.contains("Created fix script"), "{text}");
    assert!(has_fix_script(&files), "{files}");
}

#[tokio::test]
async fn verify_naming_another_set_writes_the_fix_script() {
    let (text, _, files) = set_update_set(
        "picker/verify-other",
        picker(|_, _, nth| {
            (nth == 2).then(|| {
                json_answer(
                    200,
                    json!({"result": {"current": {"sysId": OTHER, "name": "Default"}}}),
                )
            })
        }),
    )
    .await;
    assert!(text.contains("Created fix script"), "{text}");
    assert!(has_fix_script(&files), "{files}");
}

#[tokio::test]
async fn failure_before_the_put_writes_the_fix_script() {
    // M-1: the first picker read fails, so the change was never sent.
    let (text, _, files) = set_update_set(
        "picker/pre-put-500",
        picker(|_, _, nth| {
            (nth == 1).then(|| json_answer(500, json!({"error": {"message": "boom"}})))
        }),
    )
    .await;
    assert!(text.contains("Created fix script"), "{text}");
    assert!(!text.contains(MAYBE_CURRENT), "{text}");
    assert!(has_fix_script(&files), "{files}");
}

#[tokio::test]
async fn scope_mismatch_is_an_error_that_names_both_scopes() {
    let processor = processor_stub(
        || {
            Some(
                "<a href=\"sys_script_execution_history.do?sys_id=0123456789abcdef0123456789abcdef\">h</a>Script completed in scope x_other: script<pre>*** Script: hi<BR/></pre>"
                    .to_owned(),
            )
        },
        200,
    );
    let world = World::start().await;
    let r = harness::call_tool(
        &world,
        "scripts/scope-mismatch",
        Scenario {
            stub: Some(processor),
            ..Scenario::default()
        },
        "SN-Execute-Background-Script",
        json!({"script": "gs.info('hi');", "scope": "global"}),
    )
    .await
    .expect("call");
    let result = &r.response["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(result["isError"], true, "{text}");
    assert!(
        text.contains("DID run, but in scope 'x_other', not in the requested scope 'global'"),
        "{text}"
    );
    assert!(!text.contains("executed successfully"), "{text}");
}
