//! Continuation admission (RFC §4.1): maps a pre-execution `ConnectRequired` into a
//! legacy URL elicitation, a modern `InputRequired`, or a plain connect link, and
//! redeems the modern handle (single use) before the retried call is admitted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hpm_types::BoxFut;
use hpm_types::continuation::{
    ContinuationBinding, ContinuationError, ContinuationHandle, ContinuationStore,
};
use rmcp::ErrorData;
use rmcp::model::{
    CallToolResponse, CallToolResult, ContentBlock, ElicitRequest, ElicitRequestParams, ErrorCode,
    InputRequest, InputRequests, InputRequiredResult,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// How long a modern `InputRequired` handle stays redeemable.
pub const CONTINUATION_TTL: Duration = Duration::from_secs(600);
/// MCP `2025-11-25` `URLElicitationRequiredError`.
pub const URL_ELICITATION_REQUIRED: i32 = -32042;
/// Key of the connect request inside `inputRequests`.
pub const CONNECT_INPUT_KEY: &str = "connect";

pub fn connect_message(url: &str) -> String {
    format!("Sign-in required. Open {url} and retry the call.")
}

/// SHA-256 of the canonical (key-sorted, compact) JSON form of the arguments.
pub fn args_digest(args: &Value) -> [u8; 32] {
    let mut out = String::new();
    canonical(args, &mut out);
    Sha256::digest(out.as_bytes()).into()
}

fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Legacy (`< 2026-07-28`) peer that declared `elicitation.url`.
pub fn legacy_url_elicitation(url: &str) -> ErrorData {
    let message = connect_message(url);
    ErrorData::new(
        ErrorCode(URL_ELICITATION_REQUIRED),
        "URL elicitation required",
        Some(json!({
            "elicitations": [{
                "mode": "url",
                "elicitationId": uuid::Uuid::new_v4().to_string(),
                "url": url,
                "message": message,
            }]
        })),
    )
}

/// Modern peer: an `InputRequired` result carrying a URL elicitation and the
/// opaque server-side handle as `requestState`.
pub fn input_required(url: &str, handle: ContinuationHandle) -> CallToolResponse {
    let params = ElicitRequestParams::UrlElicitationParams {
        meta: None,
        message: connect_message(url),
        url: url.to_owned(),
        elicitation_id: String::new(),
    };
    let mut requests = InputRequests::new();
    requests.insert(
        CONNECT_INPUT_KEY.to_owned(),
        InputRequest::Elicitation(ElicitRequest::new(params)),
    );
    InputRequiredResult::new(Some(requests), Some(handle.0)).into()
}

/// No elicitation capability: the 6.2.0-style text with the connect link.
pub fn connect_link(url: &str) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(connect_message(url))]).into()
}

pub fn redeem_error(e: &ContinuationError) -> ErrorData {
    ErrorData::invalid_params(format!("Invalid request state: {e}"), None)
}

/// Single-process, single-use continuation store (single-user deployments).
/// Enterprise replaces it with a replicated store in the composition root.
#[derive(Default)]
pub struct MemoryContinuationStore {
    entries: Mutex<HashMap<String, ContinuationBinding>>,
}

fn same_subject(a: &ContinuationBinding, b: &ContinuationBinding) -> bool {
    a.principal == b.principal
        && a.client_id == b.client_id
        && a.method == b.method
        && a.subject == b.subject
        && a.args_digest == b.args_digest
        && a.instance_id == b.instance_id
        && a.config_version == b.config_version
}

impl ContinuationStore for MemoryContinuationStore {
    fn issue<'a>(
        &'a self,
        binding: ContinuationBinding,
    ) -> BoxFut<'a, Result<ContinuationHandle, ContinuationError>> {
        Box::pin(async move {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let now = SystemTime::now();
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| ContinuationError::Storage("continuation store poisoned".into()))?;
            entries.retain(|_, b| b.expires_at > now);
            entries.insert(id.clone(), binding);
            Ok(ContinuationHandle(id))
        })
    }

    fn redeem<'a>(
        &'a self,
        handle: &'a ContinuationHandle,
        presented: &'a ContinuationBinding,
    ) -> BoxFut<'a, Result<(), ContinuationError>> {
        Box::pin(async move {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| ContinuationError::Storage("continuation store poisoned".into()))?;
            // Removal first: a handle is consumed even by a failed redemption.
            let stored = entries
                .remove(&handle.0)
                .ok_or(ContinuationError::AlreadyRedeemed)?;
            if stored.expires_at <= SystemTime::now() {
                return Err(ContinuationError::Expired);
            }
            if !same_subject(&stored, presented) {
                return Err(ContinuationError::BindingMismatch);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpm_types::context::Principal;
    use hpm_types::operation::Subject;

    fn binding(args: &Value, expires_in: Duration) -> ContinuationBinding {
        ContinuationBinding {
            principal: Principal::LocalOperator,
            client_id: "c".into(),
            method: "tools/call",
            subject: Subject::Tool("SN-Query-Table"),
            args_digest: args_digest(args),
            instance_id: Some("i".into()),
            config_version: 1,
            expires_at: SystemTime::now() + expires_in,
        }
    }

    #[test]
    fn digest_ignores_key_order() {
        let a = json!({"b": 1, "a": {"y": [1, 2], "x": "s"}});
        let b = json!({"a": {"x": "s", "y": [1, 2]}, "b": 1});
        assert_eq!(args_digest(&a), args_digest(&b));
        assert_ne!(args_digest(&a), args_digest(&json!({"b": 2})));
    }

    #[tokio::test]
    async fn redeem_is_single_use_and_bound() {
        let store = MemoryContinuationStore::default();
        let args = json!({"table_name": "incident"});
        let h = store
            .issue(binding(&args, CONTINUATION_TTL))
            .await
            .expect("issue");
        let presented = binding(&args, Duration::ZERO);
        store.redeem(&h, &presented).await.expect("first redeem");
        assert!(matches!(
            store.redeem(&h, &presented).await,
            Err(ContinuationError::AlreadyRedeemed)
        ));

        let h = store
            .issue(binding(&args, CONTINUATION_TTL))
            .await
            .expect("issue");
        let other = binding(&json!({"table_name": "sys_user"}), Duration::ZERO);
        assert!(matches!(
            store.redeem(&h, &other).await,
            Err(ContinuationError::BindingMismatch)
        ));

        let h = store
            .issue(binding(&args, Duration::ZERO))
            .await
            .expect("issue");
        assert!(matches!(
            store.redeem(&h, &presented).await,
            Err(ContinuationError::Expired)
        ));
    }

    #[test]
    fn shapes() {
        let e = legacy_url_elicitation("https://x/connect");
        assert_eq!(e.code.0, URL_ELICITATION_REQUIRED);
        let data = e.data.expect("data");
        assert_eq!(data["elicitations"][0]["mode"], "url");
        assert_eq!(data["elicitations"][0]["url"], "https://x/connect");

        let v = match input_required("https://x/connect", ContinuationHandle("h1".into())) {
            CallToolResponse::InputRequired(r) => serde_json::to_value(r).expect("ser"),
            _ => panic!("expected InputRequired"),
        };
        assert_eq!(v["resultType"], "input_required");
        assert_eq!(v["requestState"], "h1");
        assert_eq!(v["inputRequests"]["connect"]["params"]["mode"], "url");
        assert_eq!(
            v["inputRequests"]["connect"]["params"]["url"],
            "https://x/connect"
        );
    }
}
