//! Family sync: llms.txt → bounded document downloads → cache files + FTS index (port of
//! `src/docs/sync.js`).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::chunker::{Chunk, chunk_markdown};
use crate::client::{
    DOCS_RAW_ORIGIN, DOCS_REPO_RAW_BASE, DocsClient, DocsLimits, DocsTransport, DownloadBudget,
    RequestOptions, scan_markdown_links,
};
use crate::config::{
    assert_docs_branch_name, assert_docs_family_name, normalize_docs_document_path,
    resolve_docs_cache_path,
};
use crate::error::{DocsError, DocsErrorCode};
use crate::store::{DocsStore, DocumentRecord};
use crate::vector::VectorConfig;

pub const MAX_REPORTED_SKIPS: usize = 50;
const MAX_REPORTED_TARGET_UTF16: usize = 200;
const INDEX_DB_FILE: &str = "index.sqlite";

/// The store operations a sync needs. [`DocsStore`] is the production implementation; the
/// seam lets callers and tests observe store lifetime (JS `storeFactory`).
pub trait SyncStore: Send {
    fn initialize(&mut self) -> Result<(), DocsError>;
    fn begin_family_sync(&mut self, name: &str, branch: &str) -> Result<bool, DocsError>;
    fn replace_document(
        &mut self,
        document: &DocumentRecord,
        chunks: &[Chunk],
    ) -> Result<(), DocsError>;
    fn complete_family_sync(
        &mut self,
        name: &str,
        branch: &str,
        synced_at: &str,
        keep_paths: &HashSet<String>,
    ) -> Result<Vec<String>, DocsError>;
}

impl SyncStore for DocsStore {
    fn initialize(&mut self) -> Result<(), DocsError> {
        DocsStore::initialize(self)
    }

    fn begin_family_sync(&mut self, name: &str, branch: &str) -> Result<bool, DocsError> {
        DocsStore::begin_family_sync(self, name, branch)
    }

    fn replace_document(
        &mut self,
        document: &DocumentRecord,
        chunks: &[Chunk],
    ) -> Result<(), DocsError> {
        DocsStore::replace_document(self, document, chunks)
    }

    fn complete_family_sync(
        &mut self,
        name: &str,
        branch: &str,
        synced_at: &str,
        keep_paths: &HashSet<String>,
    ) -> Result<Vec<String>, DocsError> {
        DocsStore::complete_family_sync(self, name, branch, synced_at, keep_paths)
    }
}

// ---------------------------------------------------------------------------------------
// Link handling
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkRejection {
    InvalidPath,
    CrossBranch,
    CrossRepository,
    UnsupportedOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectedLink {
    pub target: String,
    pub reason: LinkRejection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLinks {
    pub links: Vec<String>,
    pub rejected: Vec<RejectedLink>,
    pub rejected_count: usize,
}

fn is_markdown_target(target: &str) -> bool {
    let end = target.find(['?', '#']).unwrap_or(target.len());
    target[..end].ends_with(".md")
}

/// `^[A-Za-z][A-Za-z0-9+.-]*:`
fn has_scheme(target: &str) -> bool {
    let mut bytes = target.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    for byte in bytes {
        if byte == b':' {
            return true;
        }
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')) {
            return false;
        }
    }
    false
}

/// Maps a family llms.txt link to a document path inside the approved branch. Only relative
/// paths and raw links to the pinned repository and the same branch are accepted; everything
/// else is rejected before any URL is built.
pub fn resolve_docs_link(target: &str, branch: &str) -> Result<String, LinkRejection> {
    let candidate = if let Some(rest) = target.strip_prefix(DOCS_REPO_RAW_BASE) {
        let (target_branch, path) = rest.split_once('/').ok_or(LinkRejection::InvalidPath)?;
        if target_branch != branch {
            return Err(LinkRejection::CrossBranch);
        }
        path
    } else if target
        .strip_prefix(DOCS_RAW_ORIGIN)
        .is_some_and(|rest| rest.starts_with('/'))
        || target.starts_with('/')
    {
        return Err(LinkRejection::CrossRepository);
    } else if has_scheme(target) || target.starts_with("//") {
        return Err(LinkRejection::UnsupportedOrigin);
    } else {
        target
    };

    normalize_docs_document_path(candidate)
        .map(str::to_owned)
        .map_err(|_| LinkRejection::InvalidPath)
}

fn reported_target(target: &str) -> String {
    let mut units = 0;
    for (index, c) in target.char_indices() {
        units += c.len_utf16();
        if units > MAX_REPORTED_TARGET_UTF16 {
            return format!("{}…", &target[..index]);
        }
    }
    target.to_owned()
}

/// Extracts the unique, safe `.md` document paths of a family `llms.txt`, bounded by
/// `max_links`; exceeding the cap is an error rather than a silent truncation.
pub fn parse_markdown_links(
    llms_text: &str,
    branch: &str,
    max_links: usize,
) -> Result<ParsedLinks, DocsError> {
    let mut links: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut rejected = Vec::new();
    let mut rejected_count = 0;

    scan_markdown_links(llms_text, |_label, target| {
        if !is_markdown_target(target) {
            return Ok(());
        }
        let path = match resolve_docs_link(target, branch) {
            Ok(path) => path,
            Err(reason) => {
                rejected_count += 1;
                if rejected.len() < MAX_REPORTED_SKIPS {
                    rejected.push(RejectedLink {
                        target: reported_target(target),
                        reason,
                    });
                }
                return Ok(());
            }
        };
        if seen.contains(&path) {
            return Ok(());
        }
        if links.len() >= max_links {
            return Err(DocsError::coded(
                DocsErrorCode::TooManyLinks,
                format!(
                    "ServiceNow docs family index lists more than {max_links} documents; refusing to sync a truncated set"
                ),
            ));
        }
        seen.insert(path.clone());
        links.push(path);
        Ok(())
    })?;

    Ok(ParsedLinks {
        links,
        rejected,
        rejected_count,
    })
}

// ---------------------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedDocument {
    pub path: String,
    pub error: String,
}

/// Outcome of one family sync. JSON keys and order match 6.2.0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub family: String,
    pub branch: String,
    pub documents_synced: usize,
    pub documents_skipped: usize,
    pub skipped_documents: Vec<SkippedDocument>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped_documents_truncated: bool,
    pub documents_removed: usize,
    pub links_rejected: usize,
    pub rejected_links: Vec<RejectedLink>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rejected_links_truncated: bool,
}

pub struct SyncOptions<'a, T> {
    pub family: &'a str,
    /// Optional ref override; must equal the approved ref of the family.
    pub branch: Option<&'a str>,
    pub cache_dir: &'a Path,
    pub client: &'a DocsClient<T>,
    pub vector_config: VectorConfig,
    pub limits: DocsLimits,
    pub cancel: Option<&'a CancellationToken>,
}

fn is_fatal(error: &DocsError) -> bool {
    matches!(
        error.code(),
        Some(DocsErrorCode::SyncBudget | DocsErrorCode::SyncDeadline | DocsErrorCode::Aborted)
    )
}

fn skip_reason(error: &DocsError) -> String {
    if error.code().is_some() {
        error.message().to_owned()
    } else {
        "ServiceNow docs request failed".to_owned()
    }
}

/// errno-style name of an I/O failure (JS `error.code`), `EIO` when unknown.
fn errno_name(error: &std::io::Error) -> &'static str {
    match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::StorageFull => "ENOSPC",
        ErrorKind::ReadOnlyFilesystem => "EROFS",
        ErrorKind::QuotaExceeded => "EDQUOT",
        ErrorKind::InvalidInput | ErrorKind::InvalidFilename => "EINVAL",
        ErrorKind::ResourceBusy => "EBUSY",
        _ => "EIO",
    }
}

/// Filesystem errors carry absolute local paths; report only the docs-relative label and the
/// errno name.
fn cache_io<T>(label: &str, result: std::io::Result<T>) -> Result<T, DocsError> {
    result.map_err(|error| {
        DocsError::coded(
            DocsErrorCode::CacheWrite,
            format!(
                "Failed to update the ServiceNow docs cache for {label} ({})",
                errno_name(&error)
            ),
        )
    })
}

/// `fs.rm(path, { recursive, force })`: removes a file, symlink or tree; missing is fine.
async fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = if metadata.is_dir() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    match result {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `new Date().toISOString()`.
fn iso8601_millis(time: SystemTime) -> String {
    let millis_total = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let millis = u32::try_from(millis_total % 1000).unwrap_or(0);
    let seconds = i64::try_from(millis_total / 1000).unwrap_or(i64::MAX / 2);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Syncs one family into `<cache_dir>/index.sqlite` and `<cache_dir>/files/<family>/…` using
/// the production [`DocsStore`].
pub async fn sync_docs_family<T: DocsTransport>(
    options: SyncOptions<'_, T>,
) -> Result<SyncResult, DocsError> {
    sync_docs_family_with(options, DocsStore::open).await
}

/// Like [`sync_docs_family`] with a custom store factory (JS `storeFactory`). The store is
/// dropped, and so closed, on every exit path including the deadline.
pub async fn sync_docs_family_with<T, S, F>(
    options: SyncOptions<'_, T>,
    store_factory: F,
) -> Result<SyncResult, DocsError>
where
    T: DocsTransport,
    S: SyncStore + 'static,
    F: FnOnce(&Path, &VectorConfig) -> Result<S, DocsError> + Send + 'static,
{
    assert_docs_family_name(options.family)?;
    if let Some(branch) = options.branch {
        assert_docs_branch_name(branch)?;
    }

    let deadline = options.limits.sync_timeout;
    match tokio::time::timeout(deadline, run_sync(&options, store_factory)).await {
        Ok(result) => result,
        Err(_) => Err(DocsError::coded(
            DocsErrorCode::SyncDeadline,
            format!(
                "ServiceNow docs sync exceeded its {} ms deadline",
                deadline.as_millis()
            ),
        )),
    }
}

/// Runs blocking SQLite (and embedding) work off the async workers so the sync deadline,
/// cancellation and unrelated requests on the shared runtime stay responsive. A job that is
/// already running cannot be interrupted: when the deadline drops the sync, the job finishes
/// its current transaction and the store is closed right after it.
async fn run_blocking<R, F>(job: F) -> Result<R, DocsError>
where
    R: Send + 'static,
    F: FnOnce() -> Result<R, DocsError> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|_| DocsError::plain("ServiceNow docs cache task failed"))?
}

/// The store, shared with the blocking jobs. Dropped (and so closed) once the sync and any
/// job still running have finished.
struct StoreCell<S>(Arc<Mutex<S>>);

impl<S: SyncStore + 'static> StoreCell<S> {
    async fn run<R, F>(&self, job: F) -> Result<R, DocsError>
    where
        R: Send + 'static,
        F: FnOnce(&mut S) -> Result<R, DocsError> + Send + 'static,
    {
        let store = Arc::clone(&self.0);
        run_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(PoisonError::into_inner);
            job(&mut guard)
        })
        .await
    }
}

async fn run_sync<T, S, F>(
    options: &SyncOptions<'_, T>,
    store_factory: F,
) -> Result<SyncResult, DocsError>
where
    T: DocsTransport,
    S: SyncStore + 'static,
    F: FnOnce(&Path, &VectorConfig) -> Result<S, DocsError> + Send + 'static,
{
    let SyncOptions {
        family,
        branch,
        cache_dir,
        client,
        vector_config,
        limits,
        cancel,
    } = options;
    let family = *family;
    let cache_dir = *cache_dir;

    let budget = DownloadBudget::new(limits.sync_bytes);
    let request = RequestOptions {
        cancel: *cancel,
        budget: None,
    };
    let approved = client.resolve_family(family, request).await?;
    if let Some(requested) = branch
        && *requested != approved.branch
    {
        return Err(DocsError::coded(
            DocsErrorCode::BranchMismatch,
            format!(
                "Docs branch override \"{requested}\" does not match the approved ref \"{}\" for family \"{family}\"",
                approved.branch
            ),
        ));
    }

    let request = RequestOptions {
        budget: Some(&budget),
        ..request
    };
    let llms = client.get_llms(family, request).await?;
    let ParsedLinks {
        links,
        rejected,
        rejected_count,
    } = parse_markdown_links(&llms, &approved.branch, limits.max_links)?;

    cache_io(
        "the cache directory",
        tokio::fs::create_dir_all(cache_dir).await,
    )?;
    let (store, invalidated) = {
        let db_path = cache_dir.join(INDEX_DB_FILE);
        let vector_config = vector_config.clone();
        let (family, branch) = (family.to_owned(), approved.branch.clone());
        let (store, invalidated) = run_blocking(move || {
            let mut store = store_factory(&db_path, &vector_config)?;
            store.initialize()?;
            let invalidated = store.begin_family_sync(&family, &branch)?;
            Ok((store, invalidated))
        })
        .await?;
        (StoreCell(Arc::new(Mutex::new(store))), invalidated)
    };
    if invalidated {
        let family_dir = resolve_docs_cache_path(cache_dir, family, None)?;
        cache_io(family, remove_path(&family_dir).await)?;
    }

    let mut documents_synced = 0;
    let mut documents_skipped = 0;
    let mut skipped_documents = Vec::new();
    for link in &links {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(DocsError::coded(
                DocsErrorCode::Aborted,
                "ServiceNow docs sync was aborted",
            ));
        }
        let markdown = match client.get_markdown(family, link, request).await {
            Ok(markdown) => markdown,
            Err(error) if is_fatal(&error) => return Err(error),
            Err(error) => {
                documents_skipped += 1;
                if skipped_documents.len() < MAX_REPORTED_SKIPS {
                    skipped_documents.push(SkippedDocument {
                        path: link.clone(),
                        error: skip_reason(&error),
                    });
                }
                continue;
            }
        };

        let output_path = resolve_docs_cache_path(cache_dir, family, Some(link))?;
        let label = format!("{family}/{link}");
        if let Some(parent) = output_path.parent() {
            cache_io(&label, tokio::fs::create_dir_all(parent).await)?;
        }
        cache_io(&label, tokio::fs::write(&output_path, &markdown).await)?;

        let (family, branch, path) = (family.to_owned(), approved.branch.clone(), link.clone());
        store
            .run(move |store| {
                let chunks = chunk_markdown(&family, &path, &markdown);
                let title = chunks
                    .first()
                    .map_or_else(|| path.clone(), |chunk| chunk.title.clone());
                store.replace_document(
                    &DocumentRecord {
                        family,
                        branch,
                        path,
                        sha: None,
                        title,
                        markdown,
                    },
                    &chunks,
                )
            })
            .await?;
        documents_synced += 1;
    }

    if documents_synced == 0 && documents_skipped > 0 {
        return Err(DocsError::plain(format!(
            "ServiceNow docs sync failed: all {documents_skipped} documents were skipped"
        )));
    }

    let keep: HashSet<String> = links.into_iter().collect();
    let removed_paths = {
        let (family, branch) = (family.to_owned(), approved.branch.clone());
        let synced_at = iso8601_millis(SystemTime::now());
        store
            .run(move |store| store.complete_family_sync(&family, &branch, &synced_at, &keep))
            .await?
    };
    for removed in &removed_paths {
        let path = resolve_docs_cache_path(cache_dir, family, Some(removed))?;
        cache_io(&format!("{family}/{removed}"), remove_path(&path).await)?;
    }

    Ok(SyncResult {
        family: family.to_owned(),
        branch: approved.branch,
        documents_synced,
        documents_skipped,
        skipped_documents_truncated: documents_skipped > skipped_documents.len(),
        skipped_documents,
        documents_removed: removed_paths.len(),
        links_rejected: rejected_count,
        rejected_links_truncated: rejected_count > rejected.len(),
        rejected_links: rejected,
    })
}

#[cfg(test)]
mod time_tests {
    use super::iso8601_millis;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn formats_like_to_iso_string() {
        assert_eq!(iso8601_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2026-11-13T12:34:56.789Z
        assert_eq!(
            iso8601_millis(UNIX_EPOCH + Duration::from_millis(1_794_573_296_789)),
            "2026-11-13T12:34:56.789Z"
        );
        // Leap day.
        assert_eq!(
            iso8601_millis(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }
}

// Ported from tests/docs-sync.test.js.
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::client::DocsClient;
    use crate::testing::{FakeGithub, INDEX_PATH, Route, doc_path};

    const RAW: &str = "https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs";
    const FAMILY_LLMS: &str = "/ServiceNow/ServiceNowDocs/australia/llms.txt";

    fn doc(path: &str) -> String {
        doc_path(path, "australia")
    }

    fn parse(text: &str) -> ParsedLinks {
        parse_markdown_links(text, "australia", 1000).unwrap()
    }

    // ---- parseMarkdownLinks ----

    #[test]
    fn accepts_nested_relative_and_same_repository_same_branch_raw_links() {
        let result = parse(
            &[
                "- [Relative](platform/relative.md)".to_owned(),
                format!("- [Raw]({RAW}/australia/markdown/pub/nested/raw.md) -- description"),
                "- [Duplicate](platform/relative.md)".to_owned(),
                "- [Site](https://www.servicenow.com/docs)".to_owned(),
                "- [Family](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/zurich/llms.txt)".to_owned(),
            ]
            .join("\n"),
        );
        assert_eq!(
            result.links,
            ["platform/relative.md", "markdown/pub/nested/raw.md"]
        );
        assert_eq!(result.rejected_count, 0);
    }

    #[test]
    fn rejects_cross_repository_cross_branch_encoded_traversal_query_fragment_and_control_links() {
        let targets = [
            "https://raw.githubusercontent.com/attacker/ServiceNowDocs/australia/x.md",
            "https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/zurich/x.md",
            "http://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/x.md",
            "//evil.example/ServiceNow/ServiceNowDocs/australia/x.md",
            "https://evil.example/x.md",
            "/ServiceNow/other/main/x.md",
            "markdown%2F..%2F..%2Fx.md",
            "markdown/%2e%2e/x.md",
            "../outside.md",
            "markdown/x.md?ref=zurich",
            "markdown/x.md#top",
            "markdown/x\u{1}.md",
        ];
        let text = targets
            .iter()
            .enumerate()
            .map(|(i, target)| format!("- [L{i}]({target})"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = parse(&text);

        assert!(result.links.is_empty());
        assert_eq!(result.rejected_count, targets.len());
        for reason in [
            LinkRejection::CrossRepository,
            LinkRejection::CrossBranch,
            LinkRejection::UnsupportedOrigin,
            LinkRejection::InvalidPath,
        ] {
            assert!(
                result.rejected.iter().any(|r| r.reason == reason),
                "{reason:?}"
            );
        }
        assert_eq!(
            serde_json::to_value(&result.rejected[0]).unwrap(),
            serde_json::json!({"target": targets[0], "reason": "cross-repository"})
        );
        assert_eq!(
            serde_json::to_value(LinkRejection::UnsupportedOrigin).unwrap(),
            "unsupported-origin"
        );
    }

    #[test]
    fn bounds_rejected_link_reporting() {
        let text = (0..MAX_REPORTED_SKIPS + 25)
            .map(|i| format!("- [x](../bad{i}.md)"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = parse(&text);
        assert_eq!(result.rejected_count, MAX_REPORTED_SKIPS + 25);
        assert_eq!(result.rejected.len(), MAX_REPORTED_SKIPS);
    }

    #[test]
    fn truncates_overlong_rejected_targets_to_200_units() {
        let long = format!("../{}.md", "a".repeat(400));
        let result = parse(&format!("- [x]({long})"));
        let target = &result.rejected[0].target;
        assert_eq!(target.chars().count(), 201);
        assert!(target.ends_with('…'));
    }

    #[test]
    fn fails_clearly_when_the_link_count_exceeds_the_cap_instead_of_truncating() {
        let text = (0..6)
            .map(|i| format!("- [x](d{i}.md)"))
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_markdown_links(&text, "australia", 5).unwrap_err();
        assert!(error.message().contains("more than 5 documents"));
        assert_eq!(error.code(), Some(DocsErrorCode::TooManyLinks));
    }

    #[test]
    fn scans_adversarial_bracket_heavy_input_in_linear_time() {
        let hostile = [
            format!("{}]({}", "[".repeat(20_000), ")".repeat(20_000)),
            format!("{}](", "x".repeat(200)).repeat(200) + ")",
            "[a](".repeat(10_000),
            "](x)".repeat(10_000),
        ]
        .join("\n");
        let started = Instant::now();
        parse(&hostile);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    // ---- syncDocsFamily ----

    struct Env {
        fake: FakeGithub,
        cache: TempDir,
        limits: DocsLimits,
    }

    impl Env {
        fn new(routes: &[(&str, &str)]) -> Self {
            Self::with(FakeGithub::australia(), routes, DocsLimits::default())
        }

        fn with_limits(routes: &[(&str, &str)], limits: DocsLimits) -> Self {
            Self::with(FakeGithub::australia(), routes, limits)
        }

        fn with(fake: FakeGithub, routes: &[(&str, &str)], limits: DocsLimits) -> Self {
            for (path, body) in routes {
                fake.set(path, *body);
            }
            Self {
                fake,
                cache: tempfile::tempdir().unwrap(),
                limits,
            }
        }

        fn client(&self) -> DocsClient<FakeGithub> {
            DocsClient::new(self.fake.clone(), self.limits.clone())
        }

        fn cached(&self, parts: &[&str]) -> std::path::PathBuf {
            let mut path = self.cache.path().join("files");
            path.extend(parts);
            path
        }

        fn db(&self) -> std::path::PathBuf {
            self.cache.path().join("index.sqlite")
        }

        fn store(&self) -> DocsStore {
            let mut store = DocsStore::open(&self.db(), &VectorConfig::default()).unwrap();
            store.initialize().unwrap();
            store
        }

        async fn sync(&self, family: &str, branch: Option<&str>) -> Result<SyncResult, DocsError> {
            let client = self.client();
            sync_docs_family(SyncOptions {
                family,
                branch,
                cache_dir: self.cache.path(),
                client: &client,
                vector_config: VectorConfig::default(),
                limits: self.limits.clone(),
                cancel: None,
            })
            .await
        }

        async fn sync_tracked(
            &self,
            family: &str,
            branch: Option<&str>,
            stats: &Stats,
        ) -> Result<SyncResult, DocsError> {
            let client = self.client();
            sync_docs_family_with(
                SyncOptions {
                    family,
                    branch,
                    cache_dir: self.cache.path(),
                    client: &client,
                    vector_config: VectorConfig::default(),
                    limits: self.limits.clone(),
                    cancel: None,
                },
                {
                    let (opened, closed) = (Arc::clone(&stats.opened), Arc::clone(&stats.closed));
                    move |path: &Path, vector: &VectorConfig| {
                        opened.fetch_add(1, Ordering::SeqCst);
                        Ok(Tracked {
                            inner: DocsStore::open(path, vector)?,
                            closed,
                        })
                    }
                },
            )
            .await
        }
    }

    #[derive(Default)]
    struct Stats {
        opened: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl Stats {
        fn counts(&self) -> (usize, usize) {
            (
                self.opened.load(Ordering::SeqCst),
                self.closed.load(Ordering::SeqCst),
            )
        }
    }

    struct Tracked {
        inner: DocsStore,
        closed: Arc<AtomicUsize>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl SyncStore for Tracked {
        fn initialize(&mut self) -> Result<(), DocsError> {
            self.inner.initialize()
        }
        fn begin_family_sync(&mut self, name: &str, branch: &str) -> Result<bool, DocsError> {
            self.inner.begin_family_sync(name, branch)
        }
        fn replace_document(
            &mut self,
            document: &DocumentRecord,
            chunks: &[Chunk],
        ) -> Result<(), DocsError> {
            self.inner.replace_document(document, chunks)
        }
        fn complete_family_sync(
            &mut self,
            name: &str,
            branch: &str,
            synced_at: &str,
            keep_paths: &HashSet<String>,
        ) -> Result<Vec<String>, DocsError> {
            self.inner
                .complete_family_sync(name, branch, synced_at, keep_paths)
        }
    }

    #[tokio::test]
    async fn downloads_relative_and_raw_links_from_the_approved_branch_and_indexes_them() {
        let env = Env::new(&[
            (
                FAMILY_LLMS,
                &format!(
                    "- [Flow](platform/flow-designer.md)\n- [Raw]({RAW}/australia/markdown/pub/raw.md)"
                ),
            ),
            (
                &doc("platform/flow-designer.md"),
                "# Flow Designer\n\nCreate actions.",
            ),
            (&doc("markdown/pub/raw.md"), "# Raw\n\nNested."),
        ]);

        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(
            (result.family.as_str(), result.branch.as_str()),
            ("australia", "australia")
        );
        assert_eq!((result.documents_synced, result.documents_skipped), (2, 0));
        let cached =
            std::fs::read_to_string(env.cached(&["australia", "platform", "flow-designer.md"]))
                .unwrap();
        assert!(cached.contains("Create actions"));
        let store = env.store();
        assert_eq!(
            store
                .get_document("australia", "markdown/pub/raw.md")
                .unwrap()
                .unwrap()
                .branch,
            "australia"
        );
        assert_eq!(
            store.search("actions", Some("australia"), 5).unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn result_serializes_with_the_6_2_0_key_order_and_omits_false_truncation_flags() {
        let env = Env::new(&[
            (FAMILY_LLMS, "- [Good](good.md)"),
            (&doc("good.md"), "# Good"),
        ]);
        let result = env.sync("australia", None).await.unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(
            json,
            r#"{"family":"australia","branch":"australia","documentsSynced":1,"documentsSkipped":0,"skippedDocuments":[],"documentsRemoved":0,"linksRejected":0,"rejectedLinks":[]}"#
        );
    }

    #[tokio::test]
    async fn never_requests_rejected_links_and_reports_them() {
        let env = Env::new(&[
            (
                FAMILY_LLMS,
                &[
                    "- [Good](platform/good.md)".to_owned(),
                    format!("- [Other branch]({RAW}/zurich/platform/zurich.md)"),
                    "- [Encoded](platform%2F..%2F..%2Fescape.md)".to_owned(),
                ]
                .join("\n"),
            ),
            (&doc("platform/good.md"), "# Good"),
        ]);

        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(result.documents_synced, 1);
        assert_eq!(result.links_rejected, 2);
        assert_eq!(
            env.fake.paths(),
            [
                INDEX_PATH.to_owned(),
                FAMILY_LLMS.to_owned(),
                doc("platform/good.md")
            ]
        );
    }

    #[tokio::test]
    async fn rejects_a_branch_override_that_differs_from_the_approved_ref_before_touching_the_cache()
     {
        let env = Env::new(&[]);
        let stats = Stats::default();

        let mismatch = env
            .sync_tracked("australia", Some("zurich"), &stats)
            .await
            .unwrap_err();
        assert!(
            mismatch
                .message()
                .contains("does not match the approved ref")
        );
        let invalid = env
            .sync_tracked("australia", Some("../main"), &stats)
            .await
            .unwrap_err();
        assert!(invalid.message().contains("Invalid ServiceNow docs branch"));

        assert_eq!(stats.counts().0, 0);
        assert_eq!(env.fake.paths(), [INDEX_PATH]);
        assert!(!env.cached(&["australia"]).exists());
    }

    #[tokio::test]
    async fn accepts_a_branch_override_equal_to_the_approved_ref() {
        let env = Env::new(&[
            (FAMILY_LLMS, "- [Good](good.md)"),
            (&doc("good.md"), "# Good"),
        ]);
        let result = env.sync("australia", Some("australia")).await.unwrap();
        assert_eq!(result.documents_synced, 1);
    }

    #[tokio::test]
    async fn rejects_families_missing_from_the_validated_index() {
        let env = Env::new(&[]);
        let error = env.sync("attacker", None).await.unwrap_err();
        assert!(error.message().contains("Unknown ServiceNow docs family"));
    }

    #[tokio::test]
    async fn fails_clearly_on_too_many_links_before_allocating_the_store() {
        let links = (0..4)
            .map(|i| format!("- [d](d{i}.md)"))
            .collect::<Vec<_>>()
            .join("\n");
        let limits = DocsLimits {
            max_links: 3,
            ..DocsLimits::default()
        };
        let env = Env::with_limits(&[(FAMILY_LLMS, &links)], limits);
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert!(error.message().contains("more than 3 documents"));
        assert_eq!(stats.counts().0, 0);
        assert_eq!(env.fake.paths(), [INDEX_PATH, FAMILY_LLMS]);
    }

    #[tokio::test]
    async fn aborts_on_the_aggregate_byte_budget_and_closes_the_store() {
        let body = format!("# Doc\n{}\n", "a".repeat(500));
        let limits = DocsLimits {
            sync_bytes: 1200,
            ..DocsLimits::default()
        };
        let env = Env::with_limits(
            &[
                (FAMILY_LLMS, "- [a](a.md)\n- [b](b.md)\n- [c](c.md)"),
                (&doc("a.md"), &body),
                (&doc("b.md"), &body),
                (&doc("c.md"), &body),
            ],
            limits,
        );
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert_eq!(error.code_str(), Some("DOCS_SYNC_BUDGET"));
        assert_eq!(stats.counts(), (1, 1));
        assert!(!env.cached(&["australia", "c.md"]).exists());
    }

    #[tokio::test]
    async fn skips_oversized_and_long_line_documents_with_bounded_reasons_and_keeps_syncing() {
        let limits = DocsLimits {
            document_bytes: 1024,
            line_bytes: 256,
            ..DocsLimits::default()
        };
        let env = Env::with_limits(
            &[
                (
                    FAMILY_LLMS,
                    "- [big](big.md)\n- [line](line.md)\n- [ok](ok.md)",
                ),
                (&doc("big.md"), &format!("# Big\n{}", "b\n".repeat(700))),
                (&doc("line.md"), &format!("# Line\n{}\n", "l".repeat(400))),
                (&doc("ok.md"), "# Ok"),
            ],
            limits,
        );

        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(result.documents_synced, 1);
        assert_eq!(result.skipped_documents.len(), 2);
        assert_eq!(result.skipped_documents[0].path, "big.md");
        assert!(
            result.skipped_documents[0]
                .error
                .contains("exceeds 1024 bytes")
        );
        assert_eq!(result.skipped_documents[1].path, "line.md");
        assert!(
            result.skipped_documents[1]
                .error
                .contains("line longer than 256 bytes")
        );
        assert!(!env.cached(&["australia", "big.md"]).exists());
    }

    #[tokio::test]
    async fn bounds_the_skipped_document_report() {
        let count = MAX_REPORTED_SKIPS + 5;
        let mut links: Vec<String> = (0..count)
            .map(|i| format!("- [m](missing{i}.md)"))
            .collect();
        links.push("- [ok](ok.md)".to_owned());
        let env = Env::new(&[(FAMILY_LLMS, &links.join("\n")), (&doc("ok.md"), "# Ok")]);

        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(result.documents_skipped, count);
        assert_eq!(result.skipped_documents.len(), MAX_REPORTED_SKIPS);
        assert!(result.skipped_documents_truncated);
        assert!(result.skipped_documents[0].error.contains("HTTP 404"));
    }

    #[tokio::test]
    async fn closes_the_store_when_the_sync_deadline_expires() {
        let limits = DocsLimits {
            sync_timeout: Duration::from_millis(150),
            ..DocsLimits::default()
        };
        let env = Env::with_limits(&[(FAMILY_LLMS, "- [slow](slow.md)")], limits);
        env.fake.set(&doc("slow.md"), Route::Hang);
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert_eq!(error.code_str(), Some("DOCS_SYNC_DEADLINE"));
        assert_eq!(
            error.message(),
            "ServiceNow docs sync exceeded its 150 ms deadline"
        );
        // A store job already running on the blocking pool when the deadline fires cannot be
        // interrupted (see `run_blocking`); the store closes right after it, so wait for that.
        let wait = Instant::now();
        while stats.counts().1 == 0 {
            assert!(
                wait.elapsed() < Duration::from_secs(5),
                "store was never closed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stats.counts(), (1, 1));
    }

    #[tokio::test]
    async fn the_deadline_fires_on_time_while_a_slow_embedder_blocks_the_store_work() {
        let limits = DocsLimits {
            sync_timeout: Duration::from_millis(150),
            ..DocsLimits::default()
        };
        let env = Env::with_limits(
            &[
                (FAMILY_LLMS, "- [slow](slow.md)"),
                (&doc("slow.md"), "# Slow\n\nBody that must be embedded."),
            ],
            limits,
        );
        let stats = Stats::default();
        let client = env.client();
        let vector_config = VectorConfig {
            enable_vector: true,
            embedding_provider: "custom".to_owned(),
            embed_text: Some(Arc::new(|_| {
                std::thread::sleep(Duration::from_millis(800));
                vec![0.0; crate::vector::DEFAULT_VECTOR_DIMENSIONS]
            })),
            ..VectorConfig::default()
        };

        let started = Instant::now();
        let error = sync_docs_family_with(
            SyncOptions {
                family: "australia",
                branch: None,
                cache_dir: env.cache.path(),
                client: &client,
                vector_config,
                limits: env.limits.clone(),
                cancel: None,
            },
            {
                let (opened, closed) = (Arc::clone(&stats.opened), Arc::clone(&stats.closed));
                move |path: &Path, vector: &VectorConfig| {
                    opened.fetch_add(1, Ordering::SeqCst);
                    Ok(Tracked {
                        inner: DocsStore::open(path, vector)?,
                        closed,
                    })
                }
            },
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error.code_str(), Some("DOCS_SYNC_DEADLINE"));
        assert!(
            elapsed < Duration::from_millis(600),
            "deadline fired after {elapsed:?}"
        );
        // The running transaction finishes, then the store is closed.
        let wait = Instant::now();
        while stats.counts().1 == 0 {
            assert!(
                wait.elapsed() < Duration::from_secs(5),
                "store was never closed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stats.counts(), (1, 1));
    }

    #[tokio::test]
    async fn does_not_open_the_store_when_the_family_index_fetch_fails() {
        let env = Env::new(&[]);
        env.fake
            .set(FAMILY_LLMS, Route::Status(500, "boom ghp_secret".into()));
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert!(error.message().contains("HTTP 500"));
        assert!(!error.message().contains("ghp_secret"));
        assert_eq!(stats.counts().0, 0);
    }

    #[tokio::test]
    async fn fails_when_every_document_is_skipped_and_still_closes_the_store() {
        let env = Env::new(&[(FAMILY_LLMS, "- [m](missing.md)")]);
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert!(error.message().contains("all 1 documents were skipped"));
        assert_eq!(stats.counts(), (1, 1));
    }

    #[tokio::test]
    async fn invalidates_cached_content_when_the_approved_ref_for_a_family_changes() {
        let env = Env::new(&[
            (FAMILY_LLMS, "- [old](old.md)"),
            (&doc("old.md"), "# Old branch content"),
        ]);
        env.sync("australia", None).await.unwrap();

        let moved = FakeGithub::with_families(&[("australia", "australia-2")]);
        moved.set(&doc_path("llms.txt", "australia-2"), "- [new](new.md)");
        moved.set(&doc_path("new.md", "australia-2"), "# New branch content");
        let env = Env {
            fake: moved,
            cache: env.cache,
            limits: env.limits,
        };
        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(
            (result.branch.as_str(), result.documents_synced),
            ("australia-2", 1)
        );
        let store = env.store();
        assert!(store.get_document("australia", "old.md").unwrap().is_none());
        assert!(
            store
                .search("Old", Some("australia"), 20)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .get_document("australia", "new.md")
                .unwrap()
                .unwrap()
                .branch,
            "australia-2"
        );
        assert!(!env.cached(&["australia", "old.md"]).exists());
    }

    #[tokio::test]
    async fn removes_documents_that_disappeared_from_the_family_index_on_resync() {
        let env = Env::new(&[
            (FAMILY_LLMS, "- [a](a.md)\n- [b](b.md)"),
            (&doc("a.md"), "# A"),
            (&doc("b.md"), "# B"),
        ]);
        env.sync("australia", None).await.unwrap();
        env.fake.set(FAMILY_LLMS, "- [a](a.md)");

        let result = env.sync("australia", None).await.unwrap();

        assert_eq!(result.documents_removed, 1);
        let store = env.store();
        assert!(store.get_document("australia", "b.md").unwrap().is_none());
        assert!(store.get_document("australia", "a.md").unwrap().is_some());
        assert!(!env.cached(&["australia", "b.md"]).exists());
    }

    #[tokio::test]
    async fn family_cache_directories_can_never_collide_with_or_delete_the_index_database() {
        let both = FakeGithub::with_families(&[
            ("australia", "australia"),
            ("index.sqlite", "index.sqlite"),
        ]);
        both.set(&doc_path("llms.txt", "index.sqlite"), "- [a](a.md)");
        both.set(&doc_path("a.md", "index.sqlite"), "# Reserved name");
        let env = Env::with(
            both,
            &[
                (FAMILY_LLMS, "- [keep](keep.md)"),
                (&doc("keep.md"), "# Keep me"),
            ],
            DocsLimits::default(),
        );
        env.sync("australia", None).await.unwrap();
        assert_eq!(
            env.sync("index.sqlite", None)
                .await
                .unwrap()
                .documents_synced,
            1
        );

        let moved =
            FakeGithub::with_families(&[("australia", "australia"), ("index.sqlite", "moved")]);
        moved.set(&doc_path("llms.txt", "moved"), "- [b](b.md)");
        moved.set(&doc_path("b.md", "moved"), "# Moved");
        let env = Env {
            fake: moved,
            cache: env.cache,
            limits: env.limits,
        };
        env.sync("index.sqlite", None).await.unwrap();

        assert!(env.db().exists());
        let store = env.store();
        assert_eq!(
            store
                .get_document("australia", "keep.md")
                .unwrap()
                .unwrap()
                .branch,
            "australia"
        );
        assert_eq!(
            store
                .get_document("index.sqlite", "b.md")
                .unwrap()
                .unwrap()
                .branch,
            "moved"
        );
    }

    #[tokio::test]
    async fn cache_write_failures_are_reported_without_local_filesystem_paths_and_close_the_store()
    {
        let env = Env::new(&[
            (FAMILY_LLMS, "- [x](platform/x.md)"),
            (&doc("platform/x.md"), "# X"),
        ]);
        std::fs::create_dir_all(env.cached(&["australia"])).unwrap();
        std::fs::write(env.cached(&["australia", "platform"]), "not a directory").unwrap();
        let stats = Stats::default();

        let error = env
            .sync_tracked("australia", None, &stats)
            .await
            .unwrap_err();

        assert_eq!(error.code_str(), Some("DOCS_CACHE_WRITE"));
        assert!(error.message().contains("australia/platform/x.md"));
        let cache = env.cache.path().to_string_lossy().into_owned();
        assert!(!error.message().contains(&cache));
        assert!(
            !error
                .message()
                .contains(&std::env::temp_dir().to_string_lossy().into_owned())
        );
        assert_eq!(stats.counts(), (1, 1));
    }

    #[tokio::test]
    async fn aborts_when_the_cancellation_token_fires() {
        let env = Env::new(&[(FAMILY_LLMS, "- [slow](slow.md)")]);
        env.fake.set(&doc("slow.md"), Route::Hang);
        let client = env.client();
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });

        let error = sync_docs_family(SyncOptions {
            family: "australia",
            branch: None,
            cache_dir: env.cache.path(),
            client: &client,
            vector_config: VectorConfig::default(),
            limits: env.limits.clone(),
            cancel: Some(&token),
        })
        .await
        .unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::Aborted));
    }

    #[tokio::test]
    async fn discards_a_legacy_cache_without_branch_provenance_including_fts_and_vector_rows() {
        let cache = tempfile::tempdir().unwrap();
        let db = cache.path().join("index.sqlite");
        {
            let legacy = Connection::open(&db).unwrap();
            legacy
                .execute_batch(
                    "CREATE TABLE families (name TEXT PRIMARY KEY, branch TEXT NOT NULL, synced_at TEXT);
                     CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, family TEXT NOT NULL, path TEXT NOT NULL,
                       sha TEXT, title TEXT, markdown TEXT NOT NULL, UNIQUE(family, path));
                     CREATE TABLE chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, document_id INTEGER NOT NULL, family TEXT NOT NULL,
                       path TEXT NOT NULL, title TEXT, heading TEXT, start_line INTEGER, end_line INTEGER, body TEXT NOT NULL,
                       FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE);
                     CREATE VIRTUAL TABLE chunks_fts USING fts5(title, heading, body, content='chunks', content_rowid='id');
                     INSERT INTO families VALUES ('australia', 'attacker-branch', '2026-01-01');
                     INSERT INTO documents (family, path, title, markdown) VALUES ('australia', 'x.md', 'X', 'poisoned');
                     INSERT INTO chunks (document_id, family, path, title, heading, start_line, end_line, body)
                       VALUES (1, 'australia', 'x.md', 'X', 'X', 1, 1, 'poisoned payload');
                     INSERT INTO chunks_fts (rowid, title, heading, body) VALUES (1, 'X', 'X', 'poisoned payload');
                     CREATE TABLE docs_chunk_embeddings (chunk_id INTEGER PRIMARY KEY, embedding BLOB NOT NULL);
                     INSERT INTO docs_chunk_embeddings VALUES (1, zeroblob(512));",
                )
                .unwrap();
        }

        let vector = VectorConfig {
            enable_vector: true,
            embedding_provider: "local".to_owned(),
            ..VectorConfig::default()
        };
        let mut store = DocsStore::open(&db, &vector).unwrap();
        store.initialize().unwrap();
        assert!(store.get_document("australia", "x.md").unwrap().is_none());
        assert!(store.status().unwrap().families.is_empty());
        assert!(
            store
                .search("poisoned", Some("australia"), 20)
                .unwrap()
                .is_empty()
        );
        store.close().unwrap();

        let raw =
            Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count = |sql: &str| raw.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        assert_eq!(
            count("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'poisoned'"),
            0
        );
        assert_eq!(count("SELECT count(*) FROM chunks"), 0);
        assert_eq!(count("SELECT count(*) FROM docs_chunk_embeddings"), 0);
    }

    #[test]
    fn maps_io_error_kinds_to_errno_names_and_defaults_to_eio() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            errno_name(&Error::from(ErrorKind::PermissionDenied)),
            "EACCES"
        );
        assert_eq!(errno_name(&Error::from(ErrorKind::NotFound)), "ENOENT");
        assert_eq!(errno_name(&Error::from(ErrorKind::Other)), "EIO");
    }
}
