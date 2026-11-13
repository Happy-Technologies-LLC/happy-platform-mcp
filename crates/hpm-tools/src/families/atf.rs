//! Lane D: ATF / CI-CD tools (6.2.0 `src/cicd-atf.js` tool layer).
//!
//! The sn_cicd protocol (validation, dispatch, polling, results, logs, error
//! mapping) is `hpm_core::client::cicd_atf`; this module registers the five
//! tools and runs each call on the client pinned to the call's binding, so a
//! run's dispatch, every progress poll and the result reads reach one instance
//! generation (W2-B L-C). A reconfigured instance fails the later steps closed.

use std::sync::Arc;

use hpm_core::client::cicd_atf::handle_atf_tool;
use hpm_core::client::{ClientError, PinnedClient};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{Action, Operation, OperationSet, Subject};
use hpm_types::servicenow::ServiceNowAccess;
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, NormalizeInput, Normalizer, OpCompletion,
    OutputClass, ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError,
    ToolHandler,
};
use serde_json::Value;

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};

const RUN_TEST: &str = "SN-Run-ATF-Test";
const RUN_SUITE: &str = "SN-Run-ATF-Suite";
const PROGRESS: &str = "SN-Get-CICD-Progress";
const RESULTS: &str = "SN-Get-ATF-Results";
const LOGS: &str = "SN-Get-ATF-Test-Logs";

/// `(name, description, schema, runs a test)` in 6.2.0 source order.
const TOOLS: [(&str, &str, &str, bool); 5] = [
    (
        RUN_TEST,
        "Run one Automated Test Framework test on the instance through the CI/CD REST API (POST /api/sn_cicd/tests/run_test) and, by default, wait for its pass/fail result. Provide exactly one of test_sys_id or test_name (a name must match exactly one sys_atf_test record). This EXECUTES the test, which may create, change, or roll back data. Requires the CI/CD REST API plugin (com.glide.continuousdelivery) and the sn_cicd.sys_ci_automation or admin role.",
        include_str!("atf/SN-Run-ATF-Test.schema.json"),
        true,
    ),
    (
        RUN_SUITE,
        "Run an Automated Test Framework test suite through the CI/CD REST API (POST /api/sn_cicd/testsuite/run) and, by default, wait for pass/fail/error/skip counts and per-test status. Provide exactly one of test_suite_sys_id or test_suite_name. This EXECUTES every test in the suite, which may create, change, or roll back data. Requires the CI/CD REST API plugin (com.glide.continuousdelivery) and the sn_cicd.sys_ci_automation or admin role.",
        include_str!("atf/SN-Run-ATF-Suite.schema.json"),
        true,
    ),
    (
        PROGRESS,
        "Read a CI/CD REST API progress tracker (GET /api/sn_cicd/progress/{progress_id}), such as the progress_id returned by SN-Run-ATF-Test or SN-Run-ATF-Suite. Returns state (pending, running, successful, failed, canceled), percent complete, messages, and the results id once finished. Read-only: it never starts work. Set wait to keep polling until the tracker finishes.",
        include_str!("atf/SN-Get-CICD-Progress.schema.json"),
        false,
    ),
    (
        RESULTS,
        "Read an ATF run result through the CI/CD REST API. result_type \"test\" reads GET /api/sn_cicd/tests/test/results/{result_id} (status, output, links). result_type \"suite\" reads GET /api/sn_cicd/testsuite/results/{result_id} (pass/fail/error/skip counts, child suites, links) plus per-test status from sys_atf_test_result. Read-only.",
        include_str!("atf/SN-Get-ATF-Results.schema.json"),
        false,
    ),
    (
        LOGS,
        "Read the logs captured for one ATF test result (GET /api/sn_cicd/tests/test/results/{result_id}/logs). An optional regular expression filters log lines on the instance, which always matches case-insensitively. Read-only.",
        include_str!("atf/SN-Get-ATF-Test-Logs.schema.json"),
        false,
    ),
];

fn normalizer(name: &str) -> Normalizer {
    match name {
        RUN_TEST => |_, i| Ok(op(RUN_TEST, Action::Execute, i)),
        RUN_SUITE => |_, i| Ok(op(RUN_SUITE, Action::Execute, i)),
        PROGRESS => |_, i| Ok(op(PROGRESS, Action::Read, i)),
        RESULTS => |_, i| Ok(op(RESULTS, Action::Read, i)),
        _ => |_, i| Ok(op(LOGS, Action::Read, i)),
    }
}

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    for (name, description, schema, runs) in TOOLS {
        let handler = Arc::new(Atf {
            name,
            runs,
            servicenow: deps.servicenow.clone(),
        });
        let tool =
            ToolDescriptor::new(name, description, schema, normalizer(name), handler, render)
                .output(OutputClass::Opaque)
                .needs_instance(true)
                .credentials(CredentialNeed::Rest);
        let tool = if runs {
            tool.access(AccessClass::Execute)
                .side_effect(SideEffectClass::Remote)
                .retry(RetryEligibility::BeforeSendOnly)
        } else {
            tool.access(AccessClass::Read)
                .side_effect(SideEffectClass::None)
                .retry(RetryEligibility::Idempotent)
        };
        // 6.2.0 validated SN-Get-ATF-Results in the handler (`result_type must be
        // one of: …`), ignoring unknown keys; the core handler does the same.
        let tool = tool.handler_validates_input(name == RESULTS);
        registry.register_tool(tool)?;
    }
    Ok(())
}

fn op(name: &'static str, action: Action, input: &NormalizeInput<'_>) -> OperationSet {
    OperationSet(vec![
        Operation::new(Subject::Tool(name), action)
            .instance_id(input.instance.map(|i| i.instance_id.clone())),
    ])
}

/// The handler returns 6.2.0's verbatim text in [`ResultBody::ToolResult`].
fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::ToolResult { text, .. } => text.clone(),
        _ => String::new(),
    }
}

struct Atf {
    name: &'static str,
    /// Runs a test or suite (an execute operation) rather than reading.
    runs: bool,
    servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
}

impl ToolHandler for Atf {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move { self.run(ctx, args).await })
    }
}

impl Atf {
    fn done(
        &self,
        outcome: MutationOutcome,
        result: Result<ResultBody, ToolError>,
    ) -> HandlerCompletion {
        if self.runs {
            HandlerCompletion::new(
                outcome,
                vec![OpCompletion {
                    op_index: 0,
                    outcome,
                }],
                result,
            )
        } else {
            HandlerCompletion::not_a_mutation(result)
        }
    }

    async fn run(&self, ctx: &RequestContext, args: Value) -> HandlerCompletion {
        let client = match self.servicenow.client(ctx).await {
            Ok(c) => c,
            Err(e) => return self.done(MutationOutcome::NotSent, Err(e)),
        };
        if ctx.cancel().is_cancelled() {
            return self.done(MutationOutcome::NotSent, Err(ToolError::Cancelled));
        }
        // Dispatch, polls and result reads all run on this one pinned client.
        let result = tokio::select! {
            biased;
            () = ctx.cancel().cancelled() => {
                // A run may already have been dispatched: its fate is unknown.
                return self.done(MutationOutcome::Unknown, Err(ToolError::Cancelled));
            }
            r = handle_atf_tool(self.name, &args, client.client()) => r,
        };
        match result {
            Ok(response) => {
                let (text, is_error) = tool_text(&response);
                // A run tool answers only after its dispatch POST succeeded.
                let outcome = MutationOutcome::Committed;
                if is_error {
                    self.done(outcome, Err(ToolError::Text(text)))
                } else {
                    self.done(
                        outcome,
                        Ok(ResultBody::ToolResult {
                            text,
                            explicit_is_error_false: true,
                        }),
                    )
                }
            }
            Err(e) => {
                let outcome = run_error_outcome(&e);
                let error = match outcome {
                    // 6.2.0 text, kept verbatim over the generic committed/unknown text.
                    MutationOutcome::Committed | MutationOutcome::Unknown if self.runs => {
                        ToolError::Text(format!("Error: {}", e.message))
                    }
                    _ => ToolError::from(e),
                };
                self.done(outcome, Err(error))
            }
        }
    }
}

/// 6.2.0 `jsonResponse`: `(content[0].text, isError)`.
fn tool_text(response: &Value) -> (String, bool) {
    let text = response
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (text, response.get("isError") == Some(&Value::Bool(true)))
}

/// What a failed run call did on the instance.
/// - input, bulk-limit and name-resolution errors: before the dispatch POST;
/// - an sn_cicd 4xx (except 408): the instance refused the dispatch;
/// - `CICD_NO_PROGRESS_ID`: the dispatch POST succeeded;
/// - 5xx, 408, a 2xx with a non-JSON body, a stale generation, a cancel or a
///   transport failure without a status: the run may have started, so unknown.
fn run_error_outcome(e: &ClientError) -> MutationOutcome {
    if e.is_stale() || e.is_cancelled() {
        return MutationOutcome::Unknown;
    }
    if e.name != "CicdApiError" {
        return MutationOutcome::NotSent;
    }
    match (e.code.as_deref(), e.status) {
        (Some("CICD_NO_PROGRESS_ID"), _) => MutationOutcome::Committed,
        // A definite refusal: the instance rejected the dispatch.
        (_, Some(s)) if (400..500).contains(&s) && s != 408 => MutationOutcome::Failed,
        // 5xx, 408, a 2xx with a non-JSON body, or no status: the run may have started.
        _ => MutationOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cicd(code: &str, status: Option<u16>) -> ClientError {
        ClientError::new("CicdApiError", "x", Some(code)).with_status(status)
    }

    #[test]
    fn dispatch_outcomes_fail_only_on_definite_refusals() {
        let o = |code, status| run_error_outcome(&cicd(code, status));
        assert_eq!(o("CICD_HTTP_ERROR", Some(403)), MutationOutcome::Failed);
        assert_eq!(o("CICD_HTTP_ERROR", Some(400)), MutationOutcome::Failed);
        assert_eq!(o("CICD_HTTP_ERROR", Some(503)), MutationOutcome::Unknown);
        assert_eq!(o("CICD_HTTP_ERROR", Some(500)), MutationOutcome::Unknown);
        assert_eq!(o("CICD_HTTP_ERROR", Some(408)), MutationOutcome::Unknown);
        // 200 with an HTML body (cicd_request's non-JSON branch).
        assert_eq!(
            o("CICD_PLUGIN_INACTIVE", Some(200)),
            MutationOutcome::Unknown
        );
        assert_eq!(o("CICD_HTTP_ERROR", None), MutationOutcome::Unknown);
        assert_eq!(
            o("CICD_NO_PROGRESS_ID", Some(200)),
            MutationOutcome::Committed
        );
        assert_eq!(
            run_error_outcome(&ClientError::plain("bad input")),
            MutationOutcome::NotSent
        );
    }
}
