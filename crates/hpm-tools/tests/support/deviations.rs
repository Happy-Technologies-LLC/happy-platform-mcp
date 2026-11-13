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
}

#[derive(Debug)]
pub struct Deviation {
    /// Golden `id` (as recorded in the file).
    pub golden: &'static str,
    pub step: usize,
    pub expected: Expected,
    pub reason: &'static str,
}

pub const DEVIATIONS: &[Deviation] = &[Deviation {
    golden: "SN-Create-Workflow/two-activities",
    step: 0,
    expected: Expected::InvalidParamsNoOutbound,
    reason: "6.2.0 accepted numbers for string fields, sent 7 writes, then crashed \
             (`Cannot read properties of undefined`); owner rule: 7.0 rejects the \
             schema-invalid input up front with -32602 and sends nothing",
}];

pub fn lookup(golden: &str, step: usize) -> Option<&'static Deviation> {
    DEVIATIONS
        .iter()
        .find(|d| d.golden == golden && d.step == step)
}

impl Expected {
    /// Problems with `got` (one replayed step) against this expectation.
    pub fn check(self, got: &Value, outbound: &[Value]) -> Vec<String> {
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
        }
        problems
    }
}
