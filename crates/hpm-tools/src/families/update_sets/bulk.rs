//! SN-Move-Records-To-Update-Set and SN-Clone-Update-Set
//! (6.2.0 `mcp-server-consolidated.js` 2754-2814, `servicenow-client.js` 2882-3060).

use hpm_core::client::{ClientError, PinnedClient, RecordParams, RecordQuery};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::{Map, Value, json};

use super::common::{Done, Progress, after_failure, done, error_for, ok_text, pinned};
use super::js_text::{pretty, put, text, truthy};
use crate::families::FamilyDeps;
use crate::families::common::failed_outcome;
use crate::logic::bulk_limits::{
    MAX_UPDATE_SET_QUERY_RECORDS, UPDATE_SET_ID_CHUNK_SIZE, UPDATE_SET_QUERY_PAGE_SIZE, is_sys_id,
    validate_update_set_clone, validate_update_set_move,
};
use crate::logic::js;

/// `_getAllRecordsBounded`: keyset pages in sys_id order, failing past the cap.
async fn all_records_bounded(
    client: &PinnedClient,
    table: &str,
    query: &str,
) -> Result<Vec<Value>, ClientError> {
    let mut records = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor
            .as_ref()
            .map_or_else(String::new, |c| format!("^sys_id>{c}"));
        let page = client
            .get_records(
                table,
                &RecordQuery {
                    query: Some(format!("{query}{after}^ORDERBYsys_id")),
                    limit: Some(UPDATE_SET_QUERY_PAGE_SIZE.to_string()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        let Value::Array(page) = page else {
            return Err(ClientError::plain(format!(
                "Unexpected {table} query response"
            )));
        };
        let len = page.len();
        let last = page
            .last()
            .and_then(|r| r.get("sys_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        records.extend(page);
        if records.len() > MAX_UPDATE_SET_QUERY_RECORDS {
            return Err(ClientError::plain(format!(
                "More than {MAX_UPDATE_SET_QUERY_RECORDS} {table} records match; narrow the selection"
            )));
        }
        if len < UPDATE_SET_QUERY_PAGE_SIZE {
            return Ok(records);
        }
        match last {
            Some(last)
                if is_sys_id(&last)
                    && cursor.as_ref().is_none_or(|c| last.as_str() > c.as_str()) =>
            {
                cursor = Some(last);
            }
            _ => {
                return Err(ClientError::plain(format!(
                    "Unexpected {table} pagination response"
                )));
            }
        }
    }
}

/// `_getRecordsBySysIds`: at most 100 ids per encoded query.
async fn records_by_sys_ids(
    client: &PinnedClient,
    table: &str,
    ids: &[String],
) -> Result<Vec<Value>, ClientError> {
    let mut records = Vec::new();
    for chunk in ids.chunks(UPDATE_SET_ID_CHUNK_SIZE) {
        let page = client
            .get_records(
                table,
                &RecordQuery {
                    query: Some(format!("sys_idIN{}", chunk.join(","))),
                    limit: Some(chunk.len().to_string()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        let Value::Array(page) = page else {
            return Err(ClientError::plain(format!(
                "Unexpected {table} query response"
            )));
        };
        records.extend(page);
    }
    Ok(records)
}

/// `shouldReport(index)` for `total` records.
fn should_report(enabled: bool, index: usize, total: usize) -> bool {
    if !enabled {
        return false;
    }
    let n = index + 1;
    if total <= 10 {
        true
    } else if total <= 50 {
        n.is_multiple_of(5) || n == total
    } else {
        n.is_multiple_of(total.div_ceil(10)) || n == total
    }
}

/// `Math.round(((i + 1) / total) * 100)`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "bounded record counts; percentage is 0..=100"
)]
fn percentage(n: usize, total: usize) -> u64 {
    ((n as f64 / total as f64) * 100.0 + 0.5).floor() as u64
}

/// `record.type || 'unknown'`.
fn type_or_unknown(record: &Value) -> String {
    record
        .get("type")
        .filter(|t| truthy(t))
        .map_or_else(|| "unknown".to_owned(), js::to_js_string)
}

pub(super) fn move_records_to_update_set<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let update_set_id = args.get("update_set_id");
        let mut options = Map::new();
        for key in ["record_sys_ids", "time_range", "source_update_set", "table"] {
            put(&mut options, key, args.get(key));
        }
        let ids = match validate_update_set_move(update_set_id, &options) {
            Ok(ids) => ids,
            Err(e) => {
                return done(MutationOutcome::NotSent, Err(ToolError::Message(e.message)));
            }
        };
        let table = args
            .get("table")
            .map_or_else(|| "sys_update_xml".to_owned(), js::to_js_string);
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let mut progress = Progress::new(ctx, args.get("progress").is_none_or(truthy));
        let wrap = |m: String| format!("Failed to move records to update set: {m}");

        let fetched = if !ids.is_empty() {
            progress
                .notify(
                    format!("Fetching {} records to move", ids.len()),
                    None,
                    None,
                )
                .await;
            records_by_sys_ids(&client, &table, &ids).await
        } else if let Some(range) = args.get("time_range").filter(|r| truthy(r)) {
            progress
                .notify("Fetching records by time range".into(), None, None)
                .await;
            let mut query = format!(
                "sys_created_on>={}^sys_created_on<={}",
                text(range.get("start")),
                text(range.get("end"))
            );
            if let Some(source) = args.get("source_update_set").filter(|s| truthy(s)) {
                query.push_str(&format!("^update_set.name={}", js::to_js_string(source)));
            }
            all_records_bounded(&client, &table, &query).await
        } else {
            Ok(Vec::new())
        };
        let to_move = match fetched {
            Ok(r) => r,
            Err(e) => {
                return done(
                    MutationOutcome::Failed,
                    Err(ToolError::Message(wrap(e.message))),
                );
            }
        };

        let mut moved: Vec<Value> = Vec::new();
        let mut errors: Vec<Value> = Vec::new();
        let total = to_move.len();
        let mut unknown = false;
        if total == 0 {
            progress
                .notify("No records found to move".into(), None, None)
                .await;
        } else {
            progress
                .notify(format!("Moving {total} records to update set"), None, None)
                .await;
            let body = json!({"update_set": update_set_id.cloned().unwrap_or(Value::Null)});
            for (i, record) in to_move.iter().enumerate() {
                if !ctx.is_live() {
                    let outcome = after_failure(moved.len(), None);
                    return done(outcome, Err(ToolError::Cancelled));
                }
                let sys_id = record.get("sys_id");
                match client.update_record(&table, &text(sys_id), &body).await {
                    Ok(_) => {
                        let mut item = Map::new();
                        put(&mut item, "sys_id", sys_id);
                        put(&mut item, "name", record.get("name"));
                        put(&mut item, "type", record.get("type"));
                        item.insert("status".into(), json!("moved"));
                        moved.push(Value::Object(item));
                        if should_report(progress.enabled(), i, total) {
                            progress
                                .always(
                                    format!(
                                        "Moving record {}/{total} ({}%): {}",
                                        i + 1,
                                        percentage(i + 1, total),
                                        type_or_unknown(record)
                                    ),
                                    Some(i + 1),
                                    Some(total),
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        unknown |= failed_outcome(&e) == MutationOutcome::Unknown;
                        let mut item = Map::new();
                        put(&mut item, "sys_id", sys_id);
                        item.insert("error".into(), json!(e.message));
                        errors.push(Value::Object(item));
                        progress
                            .notify(
                                format!(
                                    "Failed {}/{total}: {} - {}",
                                    i + 1,
                                    text(sys_id),
                                    e.message
                                ),
                                Some(i + 1),
                                Some(total),
                            )
                            .await;
                    }
                }
            }
            let message = if errors.is_empty() {
                format!("Complete: All {total} records moved successfully")
            } else {
                format!(
                    "Complete: {}/{total} records moved ({} failed)",
                    moved.len(),
                    errors.len()
                )
            };
            progress.notify(message, None, None).await;
        }

        let outcome = match (moved.is_empty(), errors.is_empty()) {
            (_, true) if total == 0 => MutationOutcome::NotAMutation,
            (false, true) => MutationOutcome::Committed,
            (false, false) => MutationOutcome::PartiallyCommitted,
            (true, _) if unknown => MutationOutcome::Unknown,
            (true, _) => MutationOutcome::Failed,
        };
        let moved_lines = if moved.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = moved
                .iter()
                .map(|r| {
                    format!(
                        "  - {}: {} ({})",
                        text(r.get("type")),
                        text(r.get("name")),
                        text(r.get("sys_id"))
                    )
                })
                .collect();
            format!("\nMoved records:\n{}", lines.join("\n"))
        };
        let error_lines = if errors.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = errors
                .iter()
                .map(|e| format!("  - {}: {}", text(e.get("sys_id")), text(e.get("error"))))
                .collect();
            format!("\n❌ Errors:\n{}", lines.join("\n"))
        };
        let result = json!({
            "moved": moved.len(),
            "failed": errors.len(),
            "records": moved,
            "errors": errors,
        });
        done(
            outcome,
            ok_text(format!(
                "✅ Records moved to update set!\n\nMoved: {} records\nFailed: {} records\n\n{moved_lines}\n\n{error_lines}\n\nFull result:\n{}",
                result["moved"],
                result["failed"],
                pretty(&result)
            )),
        )
    })
}

pub(super) fn clone_update_set<'a>(
    deps: &'a FamilyDeps,
    ctx: &'a RequestContext,
    args: Value,
) -> BoxFut<'a, Done> {
    Box::pin(async move {
        let source = args.get("source_update_set_id");
        let new_name = args.get("new_name");
        if let Err(e) = validate_update_set_clone(source, new_name) {
            return done(MutationOutcome::NotSent, Err(ToolError::Message(e.message)));
        }
        let source_id = text(source);
        let client = match pinned(deps, ctx, MutationOutcome::NotSent).await {
            Ok(c) => c,
            Err(d) => return d,
        };
        let mut progress = Progress::new(ctx, args.get("progress").is_none_or(truthy));
        let wrap = |m: String| format!("Failed to clone update set: {m}");
        let pre_send = |e: ClientError| {
            done(
                MutationOutcome::Failed,
                Err(ToolError::Message(wrap(e.message))),
            )
        };

        progress
            .notify("Fetching source update set".into(), None, None)
            .await;
        let source_set = match client
            .get_record("sys_update_set", &source_id, &RecordParams::default())
            .await
        {
            Ok(s) => s,
            Err(e) => return pre_send(e),
        };
        progress
            .notify("Fetching update records from source".into(), None, None)
            .await;
        let updates = match all_records_bounded(
            &client,
            "sys_update_xml",
            &format!("update_set={source_id}"),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => return pre_send(e),
        };

        progress
            .notify(
                format!("Creating new update set: {}", text(new_name)),
                None,
                None,
            )
            .await;
        let description = source_set
            .get("description")
            .filter(|d| truthy(d))
            .map_or_else(String::new, js::to_js_string);
        let mut set_body = Map::new();
        put(&mut set_body, "name", new_name);
        set_body.insert(
            "description".into(),
            json!(format!(
                "Clone of: {}\n\n{description}",
                text(source_set.get("name"))
            )),
        );
        put(&mut set_body, "application", source_set.get("application"));
        set_body.insert("state".into(), json!("in progress"));
        if !ctx.is_live() {
            return done(MutationOutcome::NotSent, Err(ToolError::Cancelled));
        }
        let new_set = match client
            .create_record("sys_update_set", &Value::Object(set_body))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let outcome = after_failure(0, Some(&e));
                return done(outcome, Err(error_for(outcome, wrap(e.message))));
            }
        };

        let total = updates.len();
        progress
            .notify(format!("Cloning {total} update records"), None, None)
            .await;
        let mut cloned = 0usize;
        let mut failed = 0usize;
        for (i, record) in updates.iter().enumerate() {
            if !ctx.is_live() {
                return done(
                    MutationOutcome::PartiallyCommitted,
                    Err(ToolError::Cancelled),
                );
            }
            let mut body = Map::new();
            put(&mut body, "update_set", new_set.get("sys_id"));
            for key in ["name", "type", "target_name", "payload", "category"] {
                put(&mut body, key, record.get(key));
            }
            match client
                .create_record("sys_update_xml", &Value::Object(body))
                .await
            {
                Ok(_) => {
                    cloned += 1;
                    if should_report(progress.enabled(), i, total) {
                        progress
                            .always(
                                format!(
                                    "Cloning record {}/{total} ({}%): {}",
                                    i + 1,
                                    percentage(i + 1, total),
                                    type_or_unknown(record)
                                ),
                                Some(i + 1),
                                Some(total),
                            )
                            .await;
                    }
                }
                Err(e) => {
                    failed += 1;
                    if failed <= 5 {
                        progress
                            .notify(
                                format!("Failed to clone record {}/{total}: {}", i + 1, e.message),
                                Some(i + 1),
                                Some(total),
                            )
                            .await;
                    }
                }
            }
        }
        let message = if failed > 0 {
            format!("Complete: {cloned}/{total} records cloned ({failed} failed)")
        } else {
            format!("Complete: All {total} records cloned successfully")
        };
        progress.notify(message, None, None).await;

        let outcome = if failed > 0 {
            MutationOutcome::PartiallyCommitted
        } else {
            MutationOutcome::Committed
        };
        done(
            outcome,
            ok_text(format!(
                "✅ Update set cloned successfully!\n\nSource Update Set: {}\nSource sys_id: {source_id}\n\nNew Update Set: {}\nNew sys_id: {}\n\nRecords cloned: {cloned} / {total}\n\nThe cloned update set is now in \"In Progress\" state and ready for use.",
                text(source_set.get("name")),
                text(new_set.get("name")),
                text(new_set.get("sys_id"))
            )),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_source_name_refuses_c1_controls_and_line_separators() {
        for bad in [
            "Default\u{85}",
            "Default\u{9b}x",
            "a\u{2028}b",
            "a\u{2029}b",
            "a^b",
            "a\u{7f}",
        ] {
            let mut opts = Map::new();
            opts.insert(
                "time_range".into(),
                json!({"start": "2025-01-01 00:00:00", "end": "2025-01-02 00:00:00"}),
            );
            opts.insert("source_update_set".into(), json!(bad));
            let err = validate_update_set_move(Some(&json!("a".repeat(32))), &opts).expect_err(bad);
            assert_eq!(
                err.message,
                "source_update_set must not contain '^' or control characters"
            );
        }
    }

    #[test]
    fn progress_frequency_matches_6_2_0() {
        assert!(should_report(true, 0, 10));
        assert!(!should_report(false, 0, 10));
        assert!(!should_report(true, 0, 20));
        assert!(should_report(true, 4, 20));
        assert!(should_report(true, 19, 20));
        // 100 records: every ceil(100/10) = 10th.
        assert!(should_report(true, 9, 100));
        assert!(!should_report(true, 10, 100));
        assert_eq!(percentage(1, 3), 33);
        assert_eq!(percentage(2, 3), 67);
    }
}
