//! ServiceNow documentation engine (lane D). Ports `src/docs/{chunker, config,
//! github-client, sqlite-store, sync, vector-index}.js`; tool definitions and handlers
//! are registered from hpm-tools in P3.
//!
//! - [`config`]: docs settings and path safety.
//! - [`chunker`]: heading-based markdown chunks with line ranges.
//! - [`client`] / [`transport`]: origin-pinned, bounded ServiceNowDocs client over a
//!   transport trait (production: `reqwest`).
//! - [`store`]: SQLite FTS5 store, schema-compatible with the 6.2.0 cache.
//! - [`vector`]: local embedding index (rebuilt on first run; the JS index is not portable).
//! - [`sync`]: family sync into the cache directory and the store.

pub mod chunker;
pub mod client;
pub mod config;
pub mod error;
pub mod store;
pub mod sync;
mod text;
pub mod transport;
pub mod vector;

#[cfg(test)]
mod testing;

pub use chunker::{Chunk, chunk_markdown};
pub use client::{
    DOCS_INDEX_REF, DOCS_RAW_ORIGIN, DOCS_REPO_RAW_BASE, DocsBody, DocsClient, DocsFamily,
    DocsLimits, DocsResponse, DocsTransport, DownloadBudget, RequestOptions, TransportError,
    parse_families_from_llms, scan_markdown_links,
};
pub use config::{
    DOCS_FILES_DIR, DocsConfig, DocsSystemProperties, MAX_DOCS_PATH_DEPTH, MAX_DOCS_PATH_LENGTH,
    assert_docs_branch_name, assert_docs_family_name, default_cache_dir, get_docs_config,
    normalize_docs_document_path, read_system_docs_properties, resolve_docs_cache_path,
};
pub use error::{DocsError, DocsErrorCode};
pub use store::{
    DocsStore, DocumentRecord, FamilyStatus, SearchResult, StoreStatus, StoredDocument,
};
pub use sync::{
    LinkRejection, MAX_REPORTED_SKIPS, ParsedLinks, RejectedLink, SkippedDocument, SyncOptions,
    SyncResult, SyncStore, parse_markdown_links, resolve_docs_link, sync_docs_family,
    sync_docs_family_with,
};
pub use transport::{ReqwestBody, ReqwestTransport};
pub use vector::{VectorConfig, VectorIndex, create_local_embedding, serialize_vector};
