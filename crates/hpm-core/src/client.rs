//! ServiceNow client core (lane B): REST, auth providers, cookie/CSRF, pickers,
//! `/sys.scripts.do`, pagination and outcome classes, plus the single-user
//! `hpm_types::credential::CredentialProvider` implementations. Secrets are read
//! only through [`crate::keychain::SecretStore`].
