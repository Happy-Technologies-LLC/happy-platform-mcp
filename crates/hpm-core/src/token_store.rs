//! OAuth refresh-token store (lane A). Ports `src/token-store.js`: keychain by
//! default, file store when `SERVICENOW_TOKEN_STORE=file`, both behind
//! [`crate::keychain::SecretStore`] semantics and the same account grammar.
