//! Helpers shared by every tool family.

use hpm_core::client::ClientError;
use hpm_core::client::error::{INVALID_PATH_SEGMENT_CODE, is_credential_error_code};
use hpm_types::tool::MutationOutcome;

/// What a failed single ServiceNow request proves about the server side:
/// - `NotSent`: refused before the request left (stale generation before send,
///   credential errors, a rejected table / sys_id path segment);
/// - `Unknown`: it may have been acted on (stale generation after send,
///   502/503/504 from a gateway that may have forwarded it, transport errors);
/// - `Failed`: any other HTTP status, i.e. ServiceNow answered and refused it.
pub fn failed_outcome(e: &ClientError) -> MutationOutcome {
    if e.is_stale_after_send() {
        return MutationOutcome::Unknown;
    }
    let not_sent = e.is_stale()
        || e.code
            .as_deref()
            .is_some_and(|c| c == INVALID_PATH_SEGMENT_CODE || is_credential_error_code(c));
    if not_sent {
        return MutationOutcome::NotSent;
    }
    match e.status {
        Some(502..=504) => MutationOutcome::Unknown,
        Some(s) if s != 0 => MutationOutcome::Failed,
        _ => MutationOutcome::Unknown,
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
            failed_outcome(&ClientError::credential("CREDENTIAL_NOT_FOUND")),
            MutationOutcome::NotSent
        );
        let path = path_segment("..", "table").expect_err("dot segment");
        assert_eq!(failed_outcome(&path), MutationOutcome::NotSent);
        for status in [502, 503, 504] {
            let e = ClientError::plain("gateway").with_status(Some(status));
            assert_eq!(failed_outcome(&e), MutationOutcome::Unknown, "{status}");
        }
        for status in [400, 403, 404, 500] {
            let e = ClientError::plain("refused").with_status(Some(status));
            assert_eq!(failed_outcome(&e), MutationOutcome::Failed, "{status}");
        }
        assert_eq!(
            failed_outcome(&ClientError::plain("socket hang up")),
            MutationOutcome::Unknown
        );
    }
}
