//! Error type shared by the docs engine. Mirrors the JS split between coded `DocsError`s
//! (network/limits/cache) and plain `Error`s (validation, store failures).

/// Stable machine codes of the coded docs errors (the JS `DocsError.code` strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocsErrorCode {
    IndexInvalid,
    TooLarge,
    SyncBudget,
    LineTooLong,
    Timeout,
    Aborted,
    Redirect,
    Network,
    HttpStatus,
    UnknownFamily,
    TooManyLinks,
    SyncDeadline,
    CacheWrite,
    BranchMismatch,
}

impl DocsErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IndexInvalid => "DOCS_INDEX_INVALID",
            Self::TooLarge => "DOCS_TOO_LARGE",
            Self::SyncBudget => "DOCS_SYNC_BUDGET",
            Self::LineTooLong => "DOCS_LINE_TOO_LONG",
            Self::Timeout => "DOCS_TIMEOUT",
            Self::Aborted => "DOCS_ABORTED",
            Self::Redirect => "DOCS_REDIRECT",
            Self::Network => "DOCS_NETWORK",
            Self::HttpStatus => "DOCS_HTTP_STATUS",
            Self::UnknownFamily => "DOCS_UNKNOWN_FAMILY",
            Self::TooManyLinks => "DOCS_TOO_MANY_LINKS",
            Self::SyncDeadline => "DOCS_SYNC_DEADLINE",
            Self::CacheWrite => "DOCS_CACHE_WRITE",
            Self::BranchMismatch => "DOCS_BRANCH_MISMATCH",
        }
    }
}

/// A docs-engine failure. `message` is the exact 6.2.0 error text and never carries local
/// filesystem paths, response bodies or status text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DocsError {
    code: Option<DocsErrorCode>,
    message: String,
}

impl DocsError {
    /// A coded error (JS `new DocsError(code, message)`).
    pub fn coded(code: DocsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
        }
    }

    /// An uncoded error (JS plain `new Error(message)`).
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> Option<DocsErrorCode> {
        self.code
    }

    #[must_use]
    pub fn code_str(&self) -> Option<&'static str> {
        self.code.map(DocsErrorCode::as_str)
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<rusqlite::Error> for DocsError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(_, Some(message)) => Self::plain(message.clone()),
            _ => Self::plain(error.to_string()),
        }
    }
}
