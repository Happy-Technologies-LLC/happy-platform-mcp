//! Helpers shared by every tool family.

use hpm_core::client::ClientError;
use hpm_core::client::error::{
    CANCELLED_AFTER_SEND_CODE, CANCELLED_BEFORE_SEND_CODE, CREDENTIAL_ORIGIN_MISMATCH_CODE,
    CREDENTIAL_ORIGIN_UNBOUND_CODE, INVALID_PATH_SEGMENT_CODE,
    TRIGGER_INSERTED_RESULT_UNKNOWN_CODE, is_credential_error_code,
};
use hpm_types::tool::MutationOutcome;

/// What a failed single ServiceNow request proves about the server side:
/// - `NotSent`: refused before the request left (stale generation before send,
///   a cancel before send, credential errors incl. an unbound or
///   origin-mismatched saved secret, a rejected table / sys_id path segment);
/// - `Unknown`: it may have been acted on (stale generation after send, a
///   cancel while in flight, any 5xx (a 500 can follow a partial business-rule
///   commit), 408, transport errors with no status, any failure after the
///   `sys_trigger` fallback inserted its trigger);
/// - `Failed`: a 4xx other than 408, i.e. ServiceNow answered and refused it.
pub fn failed_outcome(e: &ClientError) -> MutationOutcome {
    if e.is_stale_after_send()
        || matches!(
            e.code.as_deref(),
            Some(TRIGGER_INSERTED_RESULT_UNKNOWN_CODE | CANCELLED_AFTER_SEND_CODE)
        )
    {
        return MutationOutcome::Unknown;
    }
    let not_sent = e.is_stale()
        || e.code.as_deref().is_some_and(|c| {
            c == INVALID_PATH_SEGMENT_CODE
                || c == CANCELLED_BEFORE_SEND_CODE
                || c == CREDENTIAL_ORIGIN_MISMATCH_CODE
                || c == CREDENTIAL_ORIGIN_UNBOUND_CODE
                || is_credential_error_code(c)
        });
    if not_sent {
        return MutationOutcome::NotSent;
    }
    match e.status {
        Some(408) | Some(500..=599) | Some(0) | None => MutationOutcome::Unknown,
        Some(_) => MutationOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpm_core::client::path_segment;

    #[test]
    fn maps_each_failure_class() {
        assert_eq!(
            failed_outcome(&ClientError::stale()),
            MutationOutcome::NotSent
        );
        assert_eq!(
            failed_outcome(&ClientError::stale_after_send()),
            MutationOutcome::Unknown
        );
        assert_eq!(
            failed_outcome(&ClientError::cancelled()),
            MutationOutcome::NotSent
        );
        assert_eq!(
            failed_outcome(&ClientError::cancelled_after_send()),
            MutationOutcome::Unknown
        );
        assert_eq!(
            failed_outcome(&ClientError::credential("CREDENTIAL_NOT_FOUND")),
            MutationOutcome::NotSent
        );
        let path = path_segment("..", "table").expect_err("dot segment");
        assert_eq!(failed_outcome(&path), MutationOutcome::NotSent);
        let mut polled = ClientError::plain("ACL").with_status(Some(403));
        polled.code = Some(TRIGGER_INSERTED_RESULT_UNKNOWN_CODE.into());
        assert_eq!(failed_outcome(&polled), MutationOutcome::Unknown);
        let moved = ClientError::new(
            "CredentialResolutionError",
            "m",
            Some(CREDENTIAL_ORIGIN_MISMATCH_CODE),
        );
        assert_eq!(failed_outcome(&moved), MutationOutcome::NotSent);
        let unbound = ClientError::new(
            "CredentialResolutionError",
            "m",
            Some(CREDENTIAL_ORIGIN_UNBOUND_CODE),
        );
        assert_eq!(failed_outcome(&unbound), MutationOutcome::NotSent);
        for status in [408, 500, 501, 502, 503, 504, 505, 599] {
            let e = ClientError::plain("maybe").with_status(Some(status));
            assert_eq!(failed_outcome(&e), MutationOutcome::Unknown, "{status}");
        }
        for status in [400, 401, 403, 404, 409, 429] {
            let e = ClientError::plain("refused").with_status(Some(status));
            assert_eq!(failed_outcome(&e), MutationOutcome::Failed, "{status}");
        }
        assert_eq!(
            failed_outcome(&ClientError::plain("socket hang up")),
            MutationOutcome::Unknown
        );
    }
}
