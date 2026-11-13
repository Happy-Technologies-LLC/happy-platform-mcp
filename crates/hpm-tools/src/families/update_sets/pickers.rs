//! SN-Set-Update-Set, SN-Get-Current-Update-Set, SN-List-Update-Sets,
//! SN-Set-Current-Application, SN-Inspect-Update-Set
//! (6.2.0 `mcp-server-consolidated.js` 2053-2205, 2607-2622).

use hpm_core::client::{ClientError, PickerChange, PinnedClient, RecordParams, RecordQuery};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::{Map, Value, json};

use super::common::{
    Done, done, error_for, now_iso, ok_text, ok_text_explicit, pinned, read_failed,
    write_fix_script,
};
use super::js_text::{array_index, or, pretty, put, text, truthy, truthy_opt};
use crate::families::FamilyDeps;
use crate::families::common::failed_outcome;
use crate::logic::generated_scripts::{
    FixScriptFile, file_timestamp, to_file_name_fragment, to_javascript_string_literal,
};
use crate::logic::js;

/// `name || 'unknown'`.
fn or_unknown(value: Option<&Value>) -> String {
    js::to_js_string(&or(value, json!("unknown")))
}

pub(super) fn set_update_set<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let sys_id_value = args.get("update_set_sys_id");
        let sys_id = text(sys_id_value);
        if !crate::logic::bulk_limits::is_sys_id(&sys_id) {
            return done(
                MutationOutcome::NotSent,
                Err(ToolError::Text(
                    "❌ update_set_sys_id must be a 32-character lowercase hexadecimal sys_id"
                        .to_owned(),
                )),
            );
        }
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        // The fallback runs while the picker's UI session is still open, so
        // its `/logout.do` is deterministically the last request.
        match client
            .client()
            .set_current_update_set_or(&sys_id, |error, change| {
                fix_script_fallback(deps, &client, &sys_id, error, change)
            })
            .await
        {
            Ok(result) => {
                let previous = result
                    .get("previous_update_set")
                    .filter(|p| truthy(p))
                    .map_or_else(String::new, |p| {
                        format!(
                            "\n↩️ Previous update set: {} ({})",
                            or_unknown(p.get("name")),
                            or_unknown(p.get("sys_id"))
                        )
                    });
                done(
                    MutationOutcome::Committed,
                    ok_text(format!(
                        "✅ Update set set to current: {}\n\n🔧 Method: UI API endpoint (/api/now/ui/concoursepicker/updateset)\n✅ Verification: passed{previous}\n\nThe update set has been set as your current update set. Refresh your ServiceNow browser to see the change in the top bar.",
                        text(result.get("update_set"))
                    )),
                )
            }
            Err(d) => d,
        }
    })
}

/// The automated change was not sent or definitely refused: fall back to a
/// fix script. When the picker write may have landed (`Unknown`), no fix
/// script is written: the update set may already be current (review TF-10,
/// P3 decision 12).
async fn fix_script_fallback(
    deps: &FamilyDeps,
    client: &PinnedClient,
    sys_id: &str,
    error: ClientError,
    change: PickerChange,
) -> Done {
    let outcome = match change {
        PickerChange::NotSent => MutationOutcome::NotSent,
        PickerChange::Refused => MutationOutcome::Failed,
        PickerChange::Unknown => MutationOutcome::Unknown,
    };
    if outcome == MutationOutcome::Unknown {
        return done(
            outcome,
            Err(ToolError::Text(format!(
                "⚠️ The update set change could not be confirmed: {}\n\nThe update set (sys_id {sys_id}) may already be your current update set. Check before trying again:\n- SN-Get-Current-Update-Set\n- or the update set picker in the ServiceNow UI (refresh the browser first)",
                error.message
            ))),
        );
    }
    let update_set = match client
        .get_record("sys_update_set", sys_id, &RecordParams::default())
        .await
    {
        Ok(v) => v,
        Err(e) => return done(outcome, Err(error_for(outcome, e.message))),
    };
    let name = update_set.get("name");
    let file_name = format!(
        "set_update_set_{}_{}.js",
        to_file_name_fragment(name),
        file_timestamp()
    );
    let body = format!(
        "// Set current update set using GlideUpdateSet API\nvar updateSetSysId = {};\nvar updateSetName = {};\nvar gus = new GlideUpdateSet();\ngus.set(updateSetSysId);\ngs.info('✅ Update set changed to: ' + updateSetName);",
        to_javascript_string_literal(sys_id),
        to_javascript_string_literal(&match name {
            None | Some(Value::Null) => String::new(),
            Some(v) => js::to_js_string(v),
        })
    );
    let created_at = now_iso();
    let file = FixScriptFile {
        title: "Fix Script: Set Current Update Set",
        created_at: &created_at,
        notes: &[
            "Note: Automated methods failed. Manual execution required.",
            "",
            "ALTERNATIVE: Manual UI Method",
            "Navigate to System Update Sets → Local Update Sets, open the update set",
            "whose sys_id is in updateSetSysId below, and click \"Make this my current set\".",
        ],
        instructions: &[
            "Copy the script below this comment",
            "Navigate to ServiceNow: System Definition → Scripts - Background",
            "Paste the script",
            "Click \"Run script\"",
            "Verify output: \"Update set changed to: \" followed by the update set name",
            "Refresh your browser to see the update set in the top bar",
        ],
        metadata: None,
        body: &body,
    };
    let path = match write_fix_script(deps, file_name, &file).await {
        Ok(p) => p,
        Err(m) => return done(outcome, Err(error_for(outcome, m))),
    };
    done(
        outcome,
        ok_text(format!(
            "⚠️ Automated update set change not available.\nCreated fix script for manual execution: {path}\n\nUpdate Set: {}\nSys ID: {sys_id}\n\n🔧 To Apply:\n1. Open: {path}\n2. Copy the GlideUpdateSet script\n3. Run in ServiceNow: System Definition → Scripts - Background\n4. Refresh browser to see change\n\n💡 Alternative: Set manually in UI (System Update Sets → Local Update Sets → Make Current)",
            text(name)
        )),
    )
}

pub(super) fn get_current_update_set<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    _args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let client = match pinned(deps, ctx, MutationOutcome::NotAMutation).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        match client.client().get_current_update_set().await {
            Ok(result) => done(
                MutationOutcome::NotAMutation,
                ok_text(format!("Current update set:\n{}", pretty(&result))),
            ),
            Err(e) => read_failed(e.message),
        }
    })
}

/// `if (value) params.append(...)` with `String(value)`.
fn param(value: Option<&Value>) -> Option<String> {
    value.filter(|v| truthy(v)).map(js::to_js_string)
}

pub(super) fn list_update_sets<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let default_limit = json!(25);
        let query = RecordQuery {
            query: param(args.get("query")),
            limit: param(Some(args.get("limit").unwrap_or(&default_limit))),
            fields: param(args.get("fields")),
            offset: param(args.get("offset")),
            order_by: param(args.get("order_by")),
        };
        let client = match pinned(deps, ctx, MutationOutcome::NotAMutation).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        match client.client().list_update_sets(&query).await {
            Ok(results) => {
                let count = results
                    .as_array()
                    .map_or_else(|| "undefined".to_owned(), |a| a.len().to_string());
                done(
                    MutationOutcome::NotAMutation,
                    ok_text(format!(
                        "Found {count} Update Set(s):\n{}",
                        pretty(&results)
                    )),
                )
            }
            Err(e) => read_failed(e.message),
        }
    })
}

pub(super) fn set_current_application<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let app_sys_id = text(args.get("app_sys_id"));
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let result = match client.client().set_current_application(&app_sys_id).await {
            Ok(r) => r,
            Err(e) => {
                return done(
                    failed_outcome(&e),
                    Err(ToolError::Text(format!(
                        "❌ Failed to set current application: {}\n\nPlease verify:\n1. The app_sys_id is valid\n2. You have permissions to access the application\n3. The application exists in your instance",
                        e.message
                    ))),
                );
            }
        };
        let verified = result.get("verified") == Some(&Value::Bool(true));
        let shown_verified = truthy_opt(result.get("verified"));
        let verification = if shown_verified {
            "✅ Verification: passed".to_owned()
        } else {
            let detail = result
                .get("verification_error")
                .filter(|v| truthy(v))
                .map_or_else(String::new, |v| format!(" ({})", js::to_js_string(v)));
            format!("❌ Verification: failed{detail}")
        };
        let previous = result
            .get("previous_scope")
            .filter(|p| truthy(p))
            .map_or_else(String::new, |p| {
                format!(
                    "\n↩️ Previous scope: {} ({})",
                    or_unknown(p.get("name")),
                    text(p.get("sys_id"))
                )
            });
        let message = format!(
            "{}: {}\n\n🔧 Method: UI API endpoint (/api/now/ui/concoursepicker/application)\n{verification}{previous}\n\n{}",
            if shown_verified {
                "✅ Application set to current"
            } else {
                "⚠️ Application change requested but not verified"
            },
            text(result.get("application")),
            if shown_verified {
                "The application scope has been set as your current application. Refresh your ServiceNow browser to see the change in the top bar."
            } else {
                "Check the application picker in the ServiceNow UI before making changes."
            }
        );
        if verified {
            done(MutationOutcome::Committed, ok_text_explicit(message))
        } else {
            // The picker write was sent but not confirmed.
            done(MutationOutcome::Unknown, Err(ToolError::Text(message)))
        }
    })
}

pub(super) fn inspect_update_set<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let update_set_id = text(args.get("update_set"));
        // Interpolated into an encoded query below: one operand only, checked
        // before any request (7.0 deviation; 6.2.0 sent it unchecked). Names
        // stay allowed (6.2.0 golden `by-name`).
        if let Err(e) = crate::logic::encoded_query::query_value("update_set", &update_set_id) {
            return done(MutationOutcome::NotSent, Err(e));
        }
        let show_components = args.get("show_components").is_none_or(truthy);
        let client = match pinned(deps, ctx, MutationOutcome::NotAMutation).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let failed = |m: String| read_failed(format!("Failed to inspect update set: {m}"));
        let update_set = match client
            .get_record("sys_update_set", &update_set_id, &RecordParams::default())
            .await
        {
            Ok(v) => v,
            Err(e) => return failed(e.message),
        };
        let mut header = Map::new();
        for key in ["sys_id", "name", "state", "description"] {
            put(&mut header, key, update_set.get(key));
        }
        let mut result = Map::new();
        result.insert("update_set".into(), Value::Object(header));
        result.insert("total_records".into(), json!(0));
        result.insert("components".into(), json!([]));
        if show_components {
            let updates = match client
                .get_records(
                    "sys_update_xml",
                    &RecordQuery {
                        query: Some(format!("update_set={update_set_id}")),
                        fields: Some("type,name,target_name".into()),
                        limit: Some("10000".into()),
                        ..RecordQuery::default()
                    },
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return failed(e.message),
            };
            let Some(updates) = updates.as_array() else {
                return failed("Cannot read properties of undefined (reading 'length')".into());
            };
            result.insert("total_records".into(), json!(updates.len()));
            result.insert("components".into(), group_by_type(updates));
        }
        let state = update_set.get("state");
        result.insert(
            "ready_to_deploy".into(),
            json!(state == Some(&json!("complete"))),
        );
        done(
            MutationOutcome::NotAMutation,
            ok_text(format!(
                "Update set inspection:\n{}",
                pretty(&Value::Object(result))
            )),
        )
    })
}

/// `Object.keys(typeGroups).map(type => ({ type, count, items }))`.
fn group_by_type(updates: &[Value]) -> Value {
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for update in updates {
        let kind = js::to_js_string(&or(update.get("type"), json!("unknown")));
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, items)) => items.push(update),
            None => groups.push((kind, vec![update])),
        }
    }
    // Object.keys: array-index keys ascending first, then insertion order.
    groups.sort_by_key(|(k, _)| array_index(k).map_or((1, 0), |i| (0, i)));
    Value::Array(
        groups
            .into_iter()
            .map(|(kind, items)| {
                let names: Vec<Value> = items
                    .iter()
                    .take(10)
                    .map(|u| {
                        let name = u.get("name");
                        if truthy_opt(name) {
                            name.cloned().unwrap_or(Value::Null)
                        } else {
                            u.get("target_name").cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                json!({"type": kind, "count": items.len(), "items": names})
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_group_like_object_keys() {
        let updates = vec![
            json!({"type": "b", "name": "n1"}),
            json!({"name": "", "target_name": "t2"}),
            json!({"type": "7", "name": "n3"}),
            json!({"type": "b"}),
        ];
        assert_eq!(
            group_by_type(&updates),
            json!([
                {"type": "7", "count": 1, "items": ["n3"]},
                {"type": "b", "count": 2, "items": ["n1", null]},
                {"type": "unknown", "count": 1, "items": ["t2"]}
            ])
        );
    }
}
