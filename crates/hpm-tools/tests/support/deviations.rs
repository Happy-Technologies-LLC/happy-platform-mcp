//! Intentional 7.0 deviations from the immutable 6.2.0 goldens.
//!
//! Each entry names one golden step where 7.0 deliberately behaves differently
//! (an owner rule, not a porting gap). The golden file stays unchanged; the
//! harness compares that step against [`Expected`] instead of the recorded
//! response and outbound requests. Every other step of the golden, and every
//! other golden, is compared exactly.

use serde_json::Value;

/// What 7.0 must do instead of the recorded 6.2.0 behavior.
#[derive(Debug, Clone, Copy)]
pub enum Expected {
    /// JSON-RPC `-32602` (invalid params) before any ServiceNow request.
    InvalidParamsNoOutbound,
    /// The recorded outbound requests and the recorded single error text, then
    /// exactly `\nAlready created and not rolled back: {...}. Do not re-run the
    /// whole batch.` naming `operation_0..operation_{created-1}` (full text
    /// compared; the sys_ids come from the stub).
    RecordedTextThenNotRolledBack { created: usize },
    /// Exactly the first `requests` recorded outbound requests, then only
    /// requests to the `then` paths (in order), then an `isError: true`
    /// single text that contains `contains`.
    OutboundPrefixThenErrorText {
        requests: usize,
        then: &'static [&'static str],
        contains: &'static str,
    },
}

#[derive(Debug)]
pub struct Deviation {
    /// Golden `id` (as recorded in the file).
    pub golden: &'static str,
    pub step: usize,
    pub expected: Expected,
    pub reason: &'static str,
}

pub const DEVIATIONS: &[Deviation] = &[
    Deviation {
        golden: "SN-Create-Workflow/two-activities",
        step: 0,
        expected: Expected::InvalidParamsNoOutbound,
        reason: "6.2.0 accepted numbers for string fields, sent 7 writes, then crashed \
                 (`Cannot read properties of undefined`); owner rule: 7.0 rejects the \
                 schema-invalid input up front with -32602 and sends nothing",
    },
    Deviation {
        golden: "SN-Batch-Create/failure-rollback",
        step: 0,
        expected: Expected::RecordedTextThenNotRolledBack { created: 1 },
        reason: "6.2.0 hid that operation 0 was already created although the call \
                 asked for a transaction; 7.0 names the records it did not roll back \
                 and says not to re-run the whole batch (partially committed)",
    },
    Deviation {
        golden: "SN-Execute-Background-Script/processor-unauthorized-trigger-fallback",
        step: 0,
        expected: Expected::OutboundPrefixThenErrorText {
            requests: 4,
            then: &["/logout.do"],
            contains: "allow_trigger_fallback: true",
        },
        reason: "owner decision P4-2: the sys_trigger fallback (runs the script \
                 outside record_for_rollback, in the Default update set) is opt-in in \
                 7.0; without consent the call stops after /sys.scripts.do refused, \
                 ends the cookie session it opened, and names the opt-in instead of \
                 inserting a sys_trigger",
    },
];

pub fn lookup(golden: &str, step: usize) -> Option<&'static Deviation> {
    DEVIATIONS
        .iter()
        .find(|d| d.golden == golden && d.step == step)
}

const NOT_ROLLED_BACK: &str = "\nAlready created and not rolled back: ";
const DO_NOT_RERUN: &str = ". Do not re-run the whole batch.";

fn single_text(response: &Value) -> Option<&str> {
    match response["result"]["content"].as_array()?.as_slice() {
        [only] if only["type"] == "text" => only["text"].as_str(),
        _ => None,
    }
}

impl Expected {
    /// Problems with `got` (one replayed step) against this expectation;
    /// `want` is the recorded 6.2.0 step.
    pub fn check(
        self,
        got: &Value,
        outbound: &[Value],
        want: &Value,
        want_outbound: &[Value],
    ) -> Vec<String> {
        let mut problems = Vec::new();
        match self {
            Self::InvalidParamsNoOutbound => {
                if got["response"]["error"]["code"] != -32602 {
                    problems.push(format!("expected -32602, got {}", got["response"]));
                }
                if !outbound.is_empty() {
                    problems.push(format!(
                        "expected no outbound requests, got {}",
                        outbound.len()
                    ));
                }
            }
            Self::RecordedTextThenNotRolledBack { created } => {
                if outbound != want_outbound {
                    problems.push(format!(
                        "outbound differs from the golden: got {outbound:?}, want {want_outbound:?}"
                    ));
                }
                if got["response"]["result"]["isError"] != true {
                    problems.push(format!("expected isError: true, got {}", got["response"]));
                }
                let recorded = single_text(&want["response"]).unwrap_or_default();
                let Some(text) = single_text(&got["response"]) else {
                    problems.push(format!("expected one text item, got {}", got["response"]));
                    return problems;
                };
                let ids = text
                    .strip_prefix(recorded)
                    .and_then(|rest| rest.strip_prefix(NOT_ROLLED_BACK))
                    .and_then(|rest| rest.strip_suffix(DO_NOT_RERUN))
                    .and_then(|ids| serde_json::from_str::<Value>(ids).ok());
                let keys_ok = ids.as_ref().and_then(Value::as_object).is_some_and(|m| {
                    m.len() == created
                        && (0..created).all(|i| {
                            m.get(&format!("operation_{i}"))
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.is_empty())
                        })
                });
                if !keys_ok {
                    problems.push(format!(
                        "expected {recorded:?} + {NOT_ROLLED_BACK:?} + {{operation_0..{created}}} + {DO_NOT_RERUN:?}, got {text:?}"
                    ));
                }
            }
            Self::OutboundPrefixThenErrorText {
                requests,
                then,
                contains,
            } => {
                let prefix = &want_outbound[..requests.min(want_outbound.len())];
                let tail: Vec<&str> = outbound
                    .get(prefix.len()..)
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r["path"].as_str().unwrap_or_default())
                    .collect();
                if outbound.get(..prefix.len()) != Some(prefix) || tail != then {
                    problems.push(format!(
                        "outbound differs: got {outbound:?}, want the first {requests} recorded {prefix:?} then {then:?}"
                    ));
                }
                if got["response"]["result"]["isError"] != true {
                    problems.push(format!("expected isError: true, got {}", got["response"]));
                }
                if !single_text(&got["response"]).is_some_and(|t| t.contains(contains)) {
                    problems.push(format!(
                        "expected one text item containing {contains:?}, got {}",
                        got["response"]
                    ));
                }
            }
        }
        problems
    }
}
