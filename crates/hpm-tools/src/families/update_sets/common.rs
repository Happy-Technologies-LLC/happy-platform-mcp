//! Shared handler plumbing for lane C: pinned client acquisition, outcome
//! facts, error texts, progress counter and fix-script files.

use std::path::PathBuf;

use hpm_core::client::{ClientError, PinnedClient};
use hpm_types::context::RequestContext;
use hpm_types::tool::{HandlerCompletion, MutationOutcome, ResultBody, ToolError};
use serde_json::Value;

use crate::families::FamilyDeps;
use crate::families::common::failed_outcome;
use crate::logic::generated_scripts::{self, FixScriptFile};
use crate::logic::js;

/// Handler result: outcome facts plus the visible result.
pub(super) type Done = HandlerCompletion;

pub(super) fn done(outcome: MutationOutcome, result: Result<ResultBody, ToolError>) -> Done {
    HandlerCompletion::new(outcome, Vec::new(), result)
}

/// 6.2.0 success text with no `isError` field.
pub(super) fn ok_text(text: String) -> Result<ResultBody, ToolError> {
    Ok(ResultBody::ToolResult {
        text,
        explicit_is_error_false: false,
    })
}

/// 6.2.0 success text with an explicit `"isError": false`.
pub(super) fn ok_text_explicit(text: String) -> Result<ResultBody, ToolError> {
    Ok(ResultBody::ToolResult {
        text,
        explicit_is_error_false: true,
    })
}

/// The pinned client, or a completion carrying the credential error.
pub(super) async fn pinned(
    deps: &FamilyDeps,
    ctx: &RequestContext,
    outcome: MutationOutcome,
) -> Result<PinnedClient, Done> {
    deps.servicenow
        .client(ctx)
        .await
        .map_err(|e| done(outcome, Err(e)))
}

/// The 6.2.0 generic `Error: {message}` failure. When something already
/// committed the 6.2.0 text is kept verbatim (`Text`), so the dispatcher does
/// not replace it with a generic partial/committed text.
pub(super) fn error_for(outcome: MutationOutcome, message: String) -> ToolError {
    match outcome {
        MutationOutcome::Committed | MutationOutcome::PartiallyCommitted => {
            ToolError::Text(format!("Error: {message}"))
        }
        _ => ToolError::Message(message),
    }
}

/// A failed read-only call.
pub(super) fn read_failed(message: String) -> Done {
    done(
        MutationOutcome::NotAMutation,
        Err(ToolError::Message(message)),
    )
}

/// Outcome of a multi-step write after `committed` successful writes ended in
/// a failure `e` (or `None` when stopped before sending).
pub(super) fn after_failure(committed: usize, e: Option<&ClientError>) -> MutationOutcome {
    if committed > 0 {
        MutationOutcome::PartiallyCommitted
    } else {
        e.map_or(MutationOutcome::NotSent, failed_outcome)
    }
}

/// JS per-request progress counter (`configureProgressNotifications`).
pub(super) struct Progress<'a> {
    ctx: &'a RequestContext,
    enabled: bool,
    last: f64,
}

impl<'a> Progress<'a> {
    pub(super) fn new(ctx: &'a RequestContext, enabled: bool) -> Self {
        Self {
            ctx,
            enabled,
            last: 0.0,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// `notifyProgress` regardless of the tool's `progress` flag.
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts are bounded by the bulk limits"
    )]
    pub(super) async fn always(
        &mut self,
        message: String,
        current: Option<usize>,
        total: Option<usize>,
    ) {
        let mut progress = current.map_or(self.last + 1.0, |c| c as f64);
        if progress <= self.last {
            progress = self.last + 1.0;
        }
        self.last = progress;
        self.ctx
            .progress()
            .emit(progress, total.map(|t| t as f64), Some(message))
            .await;
    }

    /// `if (reportProgress) notifyProgress(...)`.
    pub(super) async fn notify(
        &mut self,
        message: String,
        current: Option<usize>,
        total: Option<usize>,
    ) {
        if self.enabled {
            self.always(message, current, total).await;
        }
    }
}

/// `path.resolve(process.cwd(), 'scripts')`, with the working directory from deps.
fn scripts_dir(deps: &FamilyDeps) -> PathBuf {
    deps.working_dir
        .as_ref()
        .map_or_else(|| PathBuf::from("scripts"), |d| d.join("scripts"))
}

/// `new Date().toISOString()`.
pub(super) fn now_iso() -> String {
    js::to_iso_string(js::now_millis()).unwrap_or_default()
}

/// Builds and writes a fix-script file; returns its absolute path text or
/// the 6.2.0 error message.
pub(super) async fn write_fix_script(
    deps: &FamilyDeps,
    file_name: String,
    file: &FixScriptFile<'_>,
) -> Result<String, String> {
    let content = generated_scripts::build_fix_script_file_content(file);
    let dir = scripts_dir(deps);
    tokio::task::spawn_blocking(move || {
        generated_scripts::write_fix_script_file(&dir, &file_name, &content)
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|path| path.display().to_string())
    .map_err(|e| e.to_string())
}

/// `{ key: value }` metadata with `undefined` members dropped.
pub(super) fn metadata(pairs: &[(&str, Option<&Value>)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        super::js_text::put(&mut map, key, *value);
    }
    Value::Object(map)
}
