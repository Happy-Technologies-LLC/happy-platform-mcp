//! `FixScriptMode` (public seam PS-9, SEAM-T-09): the default writes the fix-script
//! file as 7.1.0; `Inline` creates no file and no directory, returns the script
//! text in the tool result, and leaves the outcome unchanged. Covers the three
//! producers: `SN-Create-Fix-Script`, the sys_trigger fallback of
//! `SN-Execute-Background-Script` and the `SN-Set-Update-Set` picker fallback.

#[allow(dead_code, reason = "support is shared with the golden tests")]
mod support;

use std::sync::Arc;

use hpm_tools::families::{FamilyDeps, FixScriptMode};
use serde_json::{Value, json};
use support::harness::{self, CallResult, World};
use support::scenarios::{Scenario, processor_stub};
use support::stub_sn::{Answer, Body, json_answer};

const SET: &str = "ed62390adb712ed83486d9105c442b81";
const SCRIPT: &str = "gs.info(\"inline fix script\");";

fn text_of(r: &CallResult) -> String {
    r.response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

/// The generated-file bytes without their `Created:` timestamp line.
fn without_timestamp(content: &str) -> String {
    content
        .lines()
        .filter(|l| !l.starts_with(" * Created:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of the first fenced block in `text`.
fn fenced_body(text: &str) -> String {
    let open = text.find("```javascript\n").expect("fenced script");
    let rest = &text[open + "```javascript\n".len()..];
    let close = rest.rfind("\n```").expect("closing fence");
    rest[..close].to_owned()
}

/// The single generated file of a file-mode run.
fn only_file(files: &Value) -> &str {
    let map = files.as_object().expect("files");
    assert_eq!(map.len(), 1, "{files}");
    map.values().next().and_then(Value::as_str).expect("text")
}

async fn create_fix_script(id: &str, mode: FixScriptMode) -> (CallResult, World) {
    let world = World::start().await;
    let r = harness::call_tool_mode(
        &world,
        id,
        Scenario::default(),
        "SN-Create-Fix-Script",
        json!({"script_name": "inline_fix", "script_content": SCRIPT, "description": "d", "auto_delete": true}),
        mode,
    )
    .await
    .expect("call");
    (r, world)
}

#[test]
fn the_default_mode_is_file_and_the_builder_is_public() {
    assert_eq!(FixScriptMode::default(), FixScriptMode::File);
    // `FamilyDeps::with_fix_script_mode` is exercised end to end below; this
    // only pins the default's identity so a changed default cannot go unseen.
    let _: fn(FamilyDeps, FixScriptMode) -> FamilyDeps = FamilyDeps::with_fix_script_mode;
}

#[tokio::test]
async fn create_fix_script_file_mode_writes_the_file() {
    let (r, _r_world) = create_fix_script("fixmode/create-file", FixScriptMode::File).await;
    let text = text_of(&r);
    assert!(text.starts_with("✅ Fix script created: "), "{text}");
    assert!(text.contains("1. Open the file: "), "{text}");
    assert_ne!(r.response["result"]["isError"], true, "{text}");
    let content = only_file(&r.files);
    assert!(content.contains(SCRIPT), "{content}");
    assert!(r.work.join("scripts").is_dir());
}

#[tokio::test]
async fn create_fix_script_inline_returns_the_file_bytes_and_writes_nothing() {
    let (file_run, _file_run_world) =
        create_fix_script("fixmode/create-cmp-file", FixScriptMode::File).await;
    let (r, _r_world) = create_fix_script("fixmode/create-inline", FixScriptMode::Inline).await;
    let text = text_of(&r);
    assert_ne!(r.response["result"]["isError"], true, "{text}");
    assert!(text.starts_with("✅ Fix script generated"), "{text}");
    assert!(text.contains("1. Copy the entire script below"), "{text}");
    assert!(!text.contains("Open the file"), "{text}");
    assert!(!text.contains("scripts/"), "{text}");
    assert_eq!(r.files, json!({}), "no file");
    assert!(!r.work.join("scripts").exists(), "no directory");
    assert_eq!(
        without_timestamp(&fenced_body(&text)),
        without_timestamp(only_file(&file_run.files)),
        "the same bytes the file would hold"
    );
}

#[tokio::test]
async fn inline_mode_still_validates_the_script_name() {
    let world = World::start().await;
    let r = harness::call_tool_mode(
        &world,
        "fixmode/create-bad-name",
        Scenario::default(),
        "SN-Create-Fix-Script",
        json!({"script_name": "../evil", "script_content": SCRIPT}),
        FixScriptMode::Inline,
    )
    .await
    .expect("call");
    assert!(
        r.response.get("error").is_some() || r.response["result"]["isError"] == true,
        "{}",
        r.response
    );
    assert_eq!(r.files, json!({}));
    assert!(!r.work.join("scripts").exists());
}

#[tokio::test]
async fn create_fix_script_description_is_inline_only_in_inline_mode() {
    let list = |mode: FixScriptMode| async move {
        let world = World::start().await;
        // `tools/list` through the same composition: the description is read
        // from the registered descriptor.
        let r = harness::list_tools_mode(&world, "fixmode/list", mode)
            .await
            .expect("list");
        r["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .find(|t| t["name"] == "SN-Create-Fix-Script")
            .and_then(|t| t["description"].as_str())
            .expect("description")
            .to_owned()
    };
    let file = list(FixScriptMode::File).await;
    let inline = list(FixScriptMode::Inline).await;
    assert!(file.contains("CREATES (not executes) a fix script file"));
    assert!(inline.contains("not written to any file"), "{inline}");
    assert!(!inline.contains("/scripts/"), "{inline}");
}

/// sys_trigger insert refused (certainly never landed): the fallback fix script.
fn trigger_refused() -> Scenario {
    let processor = processor_stub(|| None, 403);
    Scenario {
        stub: Some(Arc::new(move |req, state| {
            if req.method == "POST" && req.path == "/api/now/table/sys_trigger" {
                return Some(json_answer(403, json!({"error": {"message": "ACL"}})));
            }
            processor(req, state)
        })),
        ..Scenario::default()
    }
}

async fn trigger_fallback(id: &str, mode: FixScriptMode) -> (CallResult, World) {
    let world = World::start().await;
    let r = harness::call_tool_mode(
        &world,
        id,
        trigger_refused(),
        "SN-Execute-Background-Script",
        json!({"script": SCRIPT, "allow_trigger_fallback": true}),
        mode,
    )
    .await
    .expect("call");
    (r, world)
}

#[tokio::test]
async fn trigger_fallback_file_mode_is_unchanged() {
    let (r, _r_world) = trigger_fallback("fixmode/trigger-file", FixScriptMode::File).await;
    let text = text_of(&r);
    assert!(
        text.contains("Created fix script for manual execution: "),
        "{text}"
    );
    assert!(text.contains("Script Preview:"), "{text}");
    assert!(only_file(&r.files).contains(SCRIPT));
}

#[tokio::test]
async fn trigger_fallback_inline_returns_the_script_and_keeps_the_outcome() {
    let (file_run, _file_run_world) =
        trigger_fallback("fixmode/trigger-cmp-file", FixScriptMode::File).await;
    let (r, _r_world) = trigger_fallback("fixmode/trigger-inline", FixScriptMode::Inline).await;
    let text = text_of(&r);
    assert_eq!(
        r.response["result"]["isError"], file_run.response["result"]["isError"],
        "{text}"
    );
    assert!(!text.contains("Created fix script"), "{text}");
    assert!(!text.contains("Open:"), "{text}");
    assert!(text.contains("1. Copy the script below"), "{text}");
    assert_eq!(r.files, json!({}));
    assert!(!r.work.join("scripts").exists());
    assert_eq!(
        without_timestamp(&fenced_body(&text)),
        without_timestamp(only_file(&file_run.files))
    );
}

/// Picker PUT refused: the update-set fix-script fallback.
fn picker_refused() -> Scenario {
    Scenario {
        stub: Some(Arc::new(|req, _state| {
            match (req.method.as_str(), req.path.as_str()) {
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
                    Some(json_answer(403, json!({"error": {"message": "ACL"}})))
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

async fn picker_fallback(id: &str, mode: FixScriptMode) -> (CallResult, World) {
    let world = World::start().await;
    let r = harness::call_tool_mode(
        &world,
        id,
        picker_refused(),
        "SN-Set-Update-Set",
        json!({"update_set_sys_id": SET}),
        mode,
    )
    .await
    .expect("call");
    (r, world)
}

#[tokio::test]
async fn picker_fallback_file_mode_is_unchanged() {
    let (r, _r_world) = picker_fallback("fixmode/picker-file", FixScriptMode::File).await;
    let text = text_of(&r);
    assert!(
        text.contains("Created fix script for manual execution: "),
        "{text}"
    );
    assert!(text.contains("1. Open: "), "{text}");
    assert!(only_file(&r.files).contains("GlideUpdateSet"));
}

#[tokio::test]
async fn picker_fallback_inline_returns_the_script_and_keeps_the_outcome() {
    let (file_run, _file_run_world) =
        picker_fallback("fixmode/picker-cmp-file", FixScriptMode::File).await;
    let (r, _r_world) = picker_fallback("fixmode/picker-inline", FixScriptMode::Inline).await;
    let text = text_of(&r);
    assert_eq!(
        r.response["result"]["isError"], file_run.response["result"]["isError"],
        "{text}"
    );
    assert!(!text.contains("Created fix script"), "{text}");
    assert!(!text.contains("1. Open:"), "{text}");
    assert!(text.contains("Update Set: Story set"), "{text}");
    assert!(
        text.contains("1. Copy the GlideUpdateSet script below"),
        "{text}"
    );
    assert_eq!(r.files, json!({}));
    assert!(!r.work.join("scripts").exists());
    // The set name and timestamp-free body match the file the default writes
    // (file names carry a timestamp; the content's `Created:` line is dropped).
    assert_eq!(
        without_timestamp(&fenced_body(&text)),
        without_timestamp(only_file(&file_run.files))
    );
}
