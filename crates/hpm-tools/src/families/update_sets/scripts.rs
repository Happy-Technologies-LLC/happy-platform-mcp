//! SN-Execute-Background-Script and SN-Create-Fix-Script
//! (6.2.0 `mcp-server-consolidated.js` 2207-2511).

use hpm_core::client::script_processor::UNKNOWN_OUTCOME;
use hpm_core::client::{ClientError, PinnedClient, ProcessorOptions, ProcessorRun, TriggerOptions};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::{Value, json};

use super::common::{
    Done, done, error_for, metadata, now_iso, ok_text, ok_text_explicit, pinned, write_fix_script,
};
use super::js_text::{
    collapse_whitespace, pretty_opt, preview, text, truthy, truthy_opt, utf16_prefix,
};
use crate::families::FamilyDeps;
use crate::families::common::failed_outcome;
use crate::logic::generated_scripts::{FixScriptFile, file_timestamp, validate_fix_script_name};
use crate::logic::js;

const EMPTY_SCRIPT: &str = "❌ script must be a non-empty string";
const NOT_BOOLEANS: &str = "❌ wait and record_for_rollback must be booleans";
const SCOPE_NEEDS_WAIT: &str = "❌ scope is only supported when wait is true: the wait:false sys_trigger path cannot run a script in an application scope.";

/// Validated SN-Execute-Background-Script input.
struct ExecInput<'a> {
    script: &'a str,
    description: Option<&'a Value>,
    wait: bool,
    scope: Option<&'a Value>,
    record_for_rollback: bool,
}

impl ExecInput<'_> {
    /// `${description ? `…${description}…` : ''}`.
    fn description_text(&self) -> Option<String> {
        self.description.filter(|d| truthy(d)).map(js::to_js_string)
    }

    /// `Description: …\n` or nothing.
    fn description_line(&self) -> String {
        self.description_text()
            .map_or_else(String::new, |d| format!("Description: {d}\n"))
    }

    /// `scope !== undefined && scope !== 'global'`.
    fn scoped(&self) -> bool {
        self.scope.is_some_and(|s| s != &json!("global"))
    }
}

/// The handler's input checks, in 6.2.0 order (before any request).
fn validate_exec(args: &Value) -> Result<ExecInput<'_>, &'static str> {
    let script = match args.get("script") {
        Some(Value::String(s)) if !js::trim(s).is_empty() => s.as_str(),
        _ => return Err(EMPTY_SCRIPT),
    };
    let wait = args.get("wait").unwrap_or(&Value::Bool(true));
    let rollback = args
        .get("record_for_rollback")
        .unwrap_or(&Value::Bool(true));
    let (Value::Bool(wait), Value::Bool(record_for_rollback)) = (wait, rollback) else {
        return Err(NOT_BOOLEANS);
    };
    let input = ExecInput {
        script,
        description: args.get("description"),
        wait: *wait,
        scope: args.get("scope"),
        record_for_rollback: *record_for_rollback,
    };
    if input.scoped() && !input.wait {
        return Err(SCOPE_NEEDS_WAIT);
    }
    Ok(input)
}

/// `renderLogs(result)`.
fn render_logs(result: &Value) -> String {
    let Some(logs) = result.get("logs").filter(|l| truthy(l)) else {
        return String::new();
    };
    let lines: Vec<String> = logs
        .as_array()
        .map(|a| a.iter().map(js::to_js_string).collect())
        .unwrap_or_default();
    let joined = lines
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    if truthy_opt(result.get("logsWindowUnverified")) {
        let captured = if lines.is_empty() {
            "Nothing was captured — this does NOT mean the script produced no output.".to_owned()
        } else {
            format!("Captured anyway (may be incomplete):\n{joined}")
        };
        return format!(
            "\n⚠️ Script logs could not be verified (possible clock skew between this host and the instance). {captured}\n"
        );
    }
    if lines.is_empty() {
        return "\n📜 Script logs: (none captured)\n".to_owned();
    }
    let truncated = if truthy_opt(result.get("logsTruncated")) {
        format!("\n(truncated to the first {} lines)", lines.len())
    } else {
        String::new()
    };
    format!("\n📜 Script logs:\n{joined}{truncated}\n")
}

/// `runViaTrigger(notice)`: the sys_trigger path, with a fix-script fallback.
async fn run_via_trigger(
    deps: &FamilyDeps,
    ctx: &RequestContext,
    client: &PinnedClient,
    input: &ExecInput<'_>,
    notice: &str,
) -> Done {
    let description = input
        .description
        .map_or_else(|| "MCP Script Execution".to_owned(), js::to_js_string);
    let options = TriggerOptions {
        wait: input.wait,
        ctx: Some(ctx.clone()),
        ..TriggerOptions::default()
    };
    let result = match client
        .client()
        .execute_script_via_trigger(input.script, &description, true, &options)
        .await
    {
        Ok(r) => r,
        // Cancelled mid-poll: the trigger exists and may run; never offer a
        // manual re-run fix script.
        Err(_) if !ctx.is_live() => {
            return done(MutationOutcome::Unknown, Err(ToolError::Cancelled));
        }
        Err(e) => return trigger_fix_script(deps, input, notice, failed_outcome(&e)).await,
    };
    let desc = input.description_line();
    let sys_id = text(result.get("trigger_sys_id"));
    let name = text(result.get("trigger_name"));
    let next = text(result.get("next_action"));
    let auto_delete = if truthy_opt(result.get("auto_delete")) {
        "Yes"
    } else {
        "No"
    };
    match result.get("status").and_then(Value::as_str) {
        Some("completed") => done(
            MutationOutcome::Committed,
            ok_text_explicit(format!(
                "{notice}✅ Script executed successfully!\n\n{desc}\n📊 Trigger Details:\n- Trigger sys_id: {sys_id}\n- Trigger name: {name}\n- Ran at: {next}\n- Auto-delete: {auto_delete}\n{}\n📤 Output:\n{}",
                render_logs(&result),
                pretty_opt(result.get("output"))
            )),
        ),
        Some("failed") => {
            let stack = result
                .get("stack")
                .filter(|s| truthy(s))
                .map_or_else(String::new, |s| {
                    format!("\nStack:\n{}", js::to_js_string(s))
                });
            done(
                MutationOutcome::Committed,
                Err(ToolError::Text(format!(
                    "{notice}❌ Script threw an error during execution — the script did NOT complete successfully.\n\n{desc}\n📊 Trigger Details:\n- Trigger sys_id: {sys_id}\n- Trigger name: {name}\n- Ran at: {next}\n{}\n🧨 Error: {}\n{stack}",
                    render_logs(&result),
                    text(result.get("error"))
                ))),
            )
        }
        Some("timeout") => done(
            MutationOutcome::Unknown,
            Err(ToolError::Text(format!(
                "{notice}⏱️ Timed out waiting for the script to report completion — this is NOT a success.\n\n{desc}\n📊 Trigger Details:\n- Trigger sys_id: {sys_id}\n- Trigger name: {name}\n- Scheduled time: {next}\n\n{}\n\nCheck manually in:\n- System Logs → System Log → All (search for \"{sys_id}\" or the trigger name)\n- System Definition → Scheduled Jobs (filter by name: {name})",
                text(result.get("message"))
            ))),
        ),
        _ => done(
            MutationOutcome::Committed,
            ok_text_explicit(format!(
                "{notice}✅ Script scheduled for execution via sys_trigger!\n\n{desc}\n📊 Trigger Details:\n- Trigger sys_id: {sys_id}\n- Trigger name: {name}\n- Scheduled time: {next}\n- Auto-delete: {auto_delete}\n\n{}\n\nThe script will execute in ~1 second. You can monitor execution in:\n- System Logs → System Log → All\n- System Definition → Scheduled Jobs (filter by name: {name})",
                text(result.get("message"))
            )),
        ),
    }
}

/// The sys_trigger path failed: write the script as a fix-script file.
async fn trigger_fix_script(
    deps: &FamilyDeps,
    input: &ExecInput<'_>,
    notice: &str,
    outcome: MutationOutcome,
) -> Done {
    let created_at = now_iso();
    let meta = metadata(&[("description", input.description)]);
    let file = FixScriptFile {
        title: "Background Script (Manual Execution Required)",
        created_at: &created_at,
        notes: &[
            "Note: Direct execution failed due to authentication requirements.",
            "This script must be executed manually in ServiceNow UI.",
        ],
        instructions: &[
            "Copy the script below",
            "Navigate to ServiceNow: System Definition → Scripts - Background",
            "Paste the script",
            "Click \"Run script\"",
            "Verify output in the output panel",
        ],
        metadata: Some(&meta),
        body: input.script,
    };
    let file_name = format!("background_script_{}.js", file_timestamp());
    match write_fix_script(deps, file_name, &file).await {
        Ok(path) => done(
            outcome,
            ok_text(format!(
                "{notice}⚠️ Direct execution not available (requires UI session).\nCreated fix script for manual execution: {path}\n\n📋 To Execute Manually:\n1. Open: {path}\n2. Copy the script content\n3. In ServiceNow: System Definition → Scripts - Background\n4. Paste and click \"Run script\"\n\nScript Preview:\n{}",
                preview(input.script, 200)
            )),
        ),
        Err(m) => done(outcome, Err(error_for(outcome, m))),
    }
}

/// Renders a processor run (`executeScriptViaProcessor` result).
fn render_processor(input: &ExecInput<'_>, result: &Value) -> (bool, String) {
    let ok = truthy_opt(result.get("ok"));
    let strings = |key: &str| -> Vec<String> {
        result
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(js::to_js_string).collect())
            .unwrap_or_default()
    };
    let scope = result.get("scope");
    let scope_id = result.get("scopeId");
    let scope_suffix = match scope_id {
        Some(id) if truthy(id) && Some(id) != scope => format!(" ({})", js::to_js_string(id)),
        _ => String::new(),
    };
    let mut lines = vec![
        if ok {
            "✅ Script executed successfully.".to_owned()
        } else {
            "❌ Script reported errors — it did NOT complete successfully.".to_owned()
        },
        String::new(),
        "Method: /sys.scripts.do processor (ran as the authenticated user, in that user's own session)".to_owned(),
    ];
    if let Some(d) = input.description_text() {
        lines.push(format!("Description: {d}"));
    }
    lines.push(format!("Scope: {}{scope_suffix}", text(scope)));
    lines.push(String::new());
    lines.push("📤 Output:".to_owned());
    let output = strings("output");
    if output.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        lines.extend(output.iter().map(|l| format!("  {l}")));
    }
    let errors = strings("errors");
    if !errors.is_empty() {
        lines.push(String::new());
        lines.push("🧨 Errors:".to_owned());
        lines.extend(errors.iter().map(|m| format!("  - {m}")));
    }
    let warnings = strings("warnings");
    if !warnings.is_empty() {
        lines.push(String::new());
        lines.extend(warnings.iter().map(|m| format!("⚠️ {m}")));
    }
    lines.push(String::new());
    let links = result.get("links");
    let link = |key: &str| links.and_then(|l| l.get(key)).filter(|v| truthy(v));
    lines.push(link("executionHistory").map_or_else(
        || "🧾 Execution history: not reported by the instance".to_owned(),
        |h| format!("🧾 Execution history: {}", js::to_js_string(h)),
    ));
    if let Some(rollback) = link("rollbackContext") {
        lines.push(format!(
            "↩️ Rollback context: {} — this run's record changes can be rolled back from this rollback context (or its Script Execution History record).",
            js::to_js_string(rollback)
        ));
    } else if !truthy_opt(result.get("recordForRollback")) {
        lines.push("↩️ Rollback: not recorded (record_for_rollback: false).".to_owned());
    } else {
        lines.push("↩️ Rollback: no rollback context was found for this run, so there is nothing to roll back from here.".to_owned());
    }
    (ok, lines.join("\n"))
}

/// What to do when the processor did not run the script.
#[derive(Debug, PartialEq, Eq)]
enum NotRunRoute {
    /// A scoped script must not escape to the scheduler's global session.
    Refuse(String),
    /// Hand the script to `sys_trigger`, prefixed by this notice.
    Fallback(String),
}

fn not_run_route(input: &ExecInput<'_>, reason: &str) -> NotRunRoute {
    if input.scoped() {
        return NotRunRoute::Refuse(format!(
            "❌ The script was NOT run. The /sys.scripts.do processor did not run it ({reason}), and the sys_trigger fallback was not used because it cannot run a script in the requested scope '{}'.",
            text(input.scope)
        ));
    }
    NotRunRoute::Fallback(format!(
        "↪️ Method: sys_trigger fallback. The /sys.scripts.do processor did not run the script ({reason}), so it was handed to a sys_trigger job instead. That job runs in the scheduler's session, not yours: tracked configuration writes may be captured into the Default update set (verify sys_update_xml and move rows with SN-Move-Records-To-Update-Set), and no rollback context is recorded{}.\n\n",
        if input.record_for_rollback {
            " (record_for_rollback could not be honored)"
        } else {
            ""
        }
    ))
}

pub(super) fn execute_background_script<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let input = match validate_exec(&args) {
            Ok(i) => i,
            Err(m) => return done(MutationOutcome::NotSent, Err(ToolError::Text(m.to_owned()))),
        };
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        // Cancellation: the dispatcher checks once at admission; the script
        // POST (or trigger insert) is the first write, so check right before it.
        if !ctx.is_live() {
            return done(MutationOutcome::NotSent, Err(ToolError::Cancelled));
        }
        if !input.wait {
            return run_via_trigger(deps, ctx, &client, &input, "").await;
        }
        let options = ProcessorOptions {
            scope: input.scope.map(js::to_js_string),
            record_for_rollback: input.record_for_rollback,
            reason: input
                .description
                .filter(|d| !d.is_null())
                .map(js::to_js_string),
            ..ProcessorOptions::default()
        };
        // The NOT_RUN fallback runs inside the processor's UI session, whose
        // single logout follows it (core `execute_script_via_processor_or`).
        let fallback = |error: ClientError| {
            let input = &input;
            let client = &client;
            async move {
                let reason = error.reason.as_deref().unwrap_or("undefined");
                match not_run_route(input, reason) {
                    NotRunRoute::Refuse(message) => {
                        done(MutationOutcome::Failed, Err(ToolError::Text(message)))
                    }
                    NotRunRoute::Fallback(notice) => {
                        if !ctx.is_live() {
                            return done(MutationOutcome::NotSent, Err(ToolError::Cancelled));
                        }
                        run_via_trigger(deps, ctx, client, input, &notice).await
                    }
                }
            }
        };
        let error = match client
            .client()
            .execute_script_via_processor_or(input.script, &options, fallback)
            .await
        {
            Ok(ProcessorRun::Ran(result)) => {
                let (ok, message) = render_processor(&input, &result);
                let body = if ok {
                    ok_text_explicit(message)
                } else {
                    Err(ToolError::Text(message))
                };
                return done(MutationOutcome::Committed, body);
            }
            Ok(ProcessorRun::NotRun(completion)) => return completion,
            Err(e) => e,
        };
        let reason = error.reason.as_deref().unwrap_or("undefined");
        match error.code.as_deref() {
            Some(UNKNOWN_OUTCOME) => {
                let hint = input.description_text().map_or_else(String::new, |d| {
                    format!(
                        " (look for the reason comment \"{}\")",
                        utf16_prefix(&collapse_whitespace(&d), 200)
                    )
                });
                done(
                    MutationOutcome::Unknown,
                    Err(ToolError::Text(format!(
                        "⚠️ Unknown outcome — the script may or may not have run (method: /sys.scripts.do processor; {reason}).\n\nDo NOT re-run it blindly. First check System Definition → Script Execution History (/sys_script_execution_history_list.do?sysparm_query=ORDERBYDESCsys_created_on) for this run and its result{hint}. No fallback was attempted, because running a script that may already have executed a second time is unsafe."
                    ))),
                )
            }
            _ => {
                let outcome = match failed_outcome(&error) {
                    MutationOutcome::NotSent => MutationOutcome::NotSent,
                    _ => MutationOutcome::Failed,
                };
                done(
                    outcome,
                    Err(ToolError::Text(format!(
                        "❌ The script was NOT run: {}",
                        error.message
                    ))),
                )
            }
        }
    })
}

pub(super) fn create_fix_script<'a>(
    deps: &'a FamilyDeps,
    _ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let failed = |m: String| done(MutationOutcome::Failed, Err(ToolError::Message(m)));
        let script_name = match validate_fix_script_name(args.get("script_name")) {
            Ok(n) => n,
            Err(e) => return failed(e.to_string()),
        };
        let Some(Value::String(content)) = args.get("script_content") else {
            return failed("Invalid script_content: must be a string".to_owned());
        };
        let auto_delete = truthy_opt(args.get("auto_delete"));
        let mut instructions = vec![
            "Copy the entire script below",
            "Navigate to ServiceNow: System Definition → Scripts - Background",
            "Paste the script",
            "Click \"Run script\"",
            "Verify output in the output panel",
        ];
        if auto_delete {
            instructions.push("Delete this file after successful execution");
        }
        let created_at = now_iso();
        let meta = metadata(&[
            ("script_name", args.get("script_name")),
            ("description", args.get("description")),
        ]);
        let file = FixScriptFile {
            title: "Fix Script (Manual Execution Required)",
            created_at: &created_at,
            notes: &[],
            instructions: &instructions,
            metadata: Some(&meta),
            body: content,
        };
        let file_name = format!("{script_name}_{}.js", file_timestamp());
        match write_fix_script(deps, file_name, &file).await {
            Ok(path) => done(
                MutationOutcome::Committed,
                ok_text(format!(
                    "✅ Fix script created: {path}\n\n📋 Next Steps:\n1. Open the file: {path}\n2. Copy the entire script content\n3. In ServiceNow, navigate to: System Definition → Scripts - Background\n4. Paste and run the script\n5. Verify the output{}\n\nScript Preview (first 200 chars):\n{}",
                    if auto_delete {
                        "\n6. Delete the file after successful execution"
                    } else {
                        ""
                    },
                    preview(content, 200)
                )),
            ),
            Err(m) => failed(m),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_failure_after_insert_is_unknown_not_failed() {
        // run_via_trigger maps trigger errors through failed_outcome: a 403 on
        // the wrapper PUT / syslog poll after the insert must not claim Failed.
        let mut e = ClientError::plain("ACL").with_status(Some(403));
        e.code = Some(hpm_core::client::error::TRIGGER_INSERTED_RESULT_UNKNOWN_CODE.into());
        assert_eq!(failed_outcome(&e), MutationOutcome::Unknown);
        let before = ClientError::plain("ACL").with_status(Some(403));
        assert_eq!(failed_outcome(&before), MutationOutcome::Failed);
    }

    #[test]
    fn scoped_not_run_refuses_instead_of_trigger_fallback() {
        let args = json!({"script": "x", "scope": "x_app"});
        let input = validate_exec(&args).expect("valid");
        assert_eq!(
            not_run_route(&input, "not authorized"),
            NotRunRoute::Refuse(
                "❌ The script was NOT run. The /sys.scripts.do processor did not run it (not authorized), and the sys_trigger fallback was not used because it cannot run a script in the requested scope 'x_app'.".to_owned()
            )
        );
        let args = json!({"script": "x"});
        let input = validate_exec(&args).expect("valid");
        assert!(matches!(
            not_run_route(&input, "not authorized"),
            NotRunRoute::Fallback(n) if n.ends_with("(record_for_rollback could not be honored).\n\n")
        ));
    }

    fn err(args: Value) -> &'static str {
        match validate_exec(&args) {
            Err(m) => m,
            Ok(_) => panic!("expected a validation error"),
        }
    }

    #[test]
    fn exec_input_checks_follow_6_2_0_order() {
        assert_eq!(err(json!({})), EMPTY_SCRIPT);
        assert_eq!(err(json!({"script": " \n\t "})), EMPTY_SCRIPT);
        assert_eq!(err(json!({"script": 5})), EMPTY_SCRIPT);
        assert_eq!(err(json!({"script": "x", "wait": "yes"})), NOT_BOOLEANS);
        assert_eq!(
            err(json!({"script": "x", "record_for_rollback": null})),
            NOT_BOOLEANS
        );
        assert_eq!(
            err(json!({"script": "x", "wait": false, "scope": "x_app"})),
            SCOPE_NEEDS_WAIT
        );
        let args = json!({"script": "x", "wait": false, "scope": "global"});
        let ok = validate_exec(&args).expect("global scope is allowed without wait");
        assert!(!ok.scoped());
        assert!(ok.record_for_rollback);
    }

    #[test]
    fn logs_render_like_6_2_0() {
        assert_eq!(render_logs(&json!({})), "");
        assert_eq!(
            render_logs(&json!({"logs": []})),
            "\n📜 Script logs: (none captured)\n"
        );
        assert_eq!(
            render_logs(&json!({"logs": ["a", "b"], "logsTruncated": true})),
            "\n📜 Script logs:\n  a\n  b\n(truncated to the first 2 lines)\n"
        );
        assert_eq!(
            render_logs(&json!({"logs": [], "logsWindowUnverified": true})),
            "\n⚠️ Script logs could not be verified (possible clock skew between this host and the instance). Nothing was captured — this does NOT mean the script produced no output.\n"
        );
        assert_eq!(
            render_logs(&json!({"logs": ["a"], "logsWindowUnverified": true})),
            "\n⚠️ Script logs could not be verified (possible clock skew between this host and the instance). Captured anyway (may be incomplete):\n  a\n"
        );
    }

    #[test]
    fn processor_render_covers_failure_warnings_and_rollback_variants() {
        let args = json!({"script": "x", "record_for_rollback": false});
        let input = validate_exec(&args).expect("valid");
        let (ok, text) = render_processor(
            &input,
            &json!({
                "ok": false, "output": [], "errors": ["boom"], "warnings": ["w"],
                "scope": "x_app", "scopeId": "abc", "recordForRollback": false, "links": {}
            }),
        );
        assert!(!ok);
        assert_eq!(
            text,
            "❌ Script reported errors — it did NOT complete successfully.\n\nMethod: /sys.scripts.do processor (ran as the authenticated user, in that user's own session)\nScope: x_app (abc)\n\n📤 Output:\n  (none)\n\n🧨 Errors:\n  - boom\n\n⚠️ w\n\n🧾 Execution history: not reported by the instance\n↩️ Rollback: not recorded (record_for_rollback: false)."
        );
    }
}
