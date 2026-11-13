//! Bounded, origin-pinned client for the ServiceNowDocs repository (port of
//! `src/docs/github-client.js`). HTTP sits behind [`DocsTransport`] so tests use a fake and
//! production uses [`crate::ReqwestTransport`].

use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::config::{assert_docs_family_name, normalize_docs_document_path};
use crate::error::{DocsError, DocsErrorCode};
use crate::text::{encode_uri_component, js_trim, js_trim_start};

/// Production origin pin: every request URL is built on this exact repository on the raw
/// GitHub origin. [`crate::ReqwestTransport::with_origin`] only changes which `http(s)` scheme
/// and authority is dialled (loopback servers in tests); production never sets it.
pub const DOCS_RAW_ORIGIN: &str = "https://raw.githubusercontent.com";
pub const DOCS_REPO_RAW_BASE: &str = "https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/";
pub const DOCS_INDEX_REF: &str = "main";

const MIB: u64 = 1024 * 1024;

/// Download and time caps. `Default` is the production configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsLimits {
    pub index_bytes: u64,
    pub document_bytes: u64,
    pub line_bytes: u64,
    pub max_links: usize,
    pub max_families: usize,
    pub sync_bytes: u64,
    pub request_timeout: Duration,
    pub sync_timeout: Duration,
}

impl Default for DocsLimits {
    fn default() -> Self {
        Self {
            index_bytes: MIB,
            document_bytes: 8 * MIB,
            line_bytes: 16 * 1024,
            max_links: 1000,
            max_families: 100,
            sync_bytes: 64 * MIB,
            request_timeout: Duration::from_millis(30_000),
            sync_timeout: Duration::from_millis(600_000),
        }
    }
}

/// A family approved by the pinned `main/llms.txt` index and the git ref its docs live on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocsFamily {
    pub name: String,
    pub branch: String,
}

/// Aggregate download budget shared by every request of one sync.
#[derive(Debug)]
pub struct DownloadBudget {
    remaining: AtomicI64,
}

impl DownloadBudget {
    #[must_use]
    pub fn new(bytes: u64) -> Self {
        Self {
            remaining: AtomicI64::new(i64::try_from(bytes).unwrap_or(i64::MAX)),
        }
    }

    #[must_use]
    pub fn remaining(&self) -> i64 {
        self.remaining.load(Ordering::Relaxed)
    }

    fn consume(&self, bytes: usize) -> i64 {
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        self.remaining
            .fetch_sub(bytes, Ordering::Relaxed)
            .saturating_sub(bytes)
    }
}

/// Per-call cancellation and download accounting (JS `{ signal, budget }`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestOptions<'a> {
    pub cancel: Option<&'a CancellationToken>,
    pub budget: Option<&'a DownloadBudget>,
}

// ---------------------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------------------

/// Failure inside a transport. Details are deliberately dropped: error text surfaced to
/// callers never echoes network or server content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// The transport itself refused to follow a redirect.
    Redirect,
    Network,
}

/// One HTTP response. `content_length` is the raw header text, validated by the client.
#[derive(Debug)]
pub struct DocsResponse<B> {
    pub status: u16,
    pub content_length: Option<String>,
    pub body: B,
}

pub trait DocsBody: Send {
    type Chunk: AsRef<[u8]> + Send;

    /// Next body chunk, `None` at end of stream. Dropping the body cancels the transfer.
    fn next_chunk(
        &mut self,
    ) -> impl Future<Output = Result<Option<Self::Chunk>, TransportError>> + Send;
}

/// Anonymous `GET` with `Accept: text/plain` that **never follows redirects**: a 3xx response
/// is returned as is (or reported as [`TransportError::Redirect`]).
pub trait DocsTransport: Send + Sync {
    type Body: DocsBody;

    fn get(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<DocsResponse<Self::Body>, TransportError>> + Send;
}

// ---------------------------------------------------------------------------------------
// Index parsing
// ---------------------------------------------------------------------------------------

/// Single forward pass over `[text](target)` links. Each line is scanned once: the backward
/// search for "[" never crosses the previous link's end, so the total work is linear in the
/// input size. `visit` may stop the scan by returning an error.
pub fn scan_markdown_links<E>(
    text: &str,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), E> {
    for line in text.split('\n') {
        let bytes = line.as_bytes();
        let mut pos = 0;
        while pos < line.len() {
            let Some(mid) = line[pos..].find("](").map(|offset| pos + offset) else {
                break;
            };
            let Some(close) = line[mid + 2..].find(')').map(|offset| mid + 2 + offset) else {
                break;
            };
            if let Some(open) = bytes[pos..mid]
                .iter()
                .rposition(|&b| b == b'[')
                .map(|offset| pos + offset)
            {
                visit(
                    js_trim(&line[open + 1..mid]),
                    js_trim(&line[mid + 2..close]),
                )?;
            }
            pos = close + 1;
        }
    }
    Ok(())
}

/// Upstream mapping line: `- "australia" : "australia" -- Australia family`
fn parse_mapping_line(line: &str) -> Option<(&str, &str)> {
    let mut rest = js_trim_start(line);
    if let Some(stripped) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        rest = js_trim_start(stripped);
    }
    let rest = rest.strip_prefix('"')?;
    let name_end = rest.find('"')?;
    let name = &rest[..name_end];
    let rest = js_trim_start(&rest[name_end + 1..]).strip_prefix(':')?;
    let rest = js_trim_start(rest).strip_prefix('"')?;
    let branch_end = rest.find('"')?;
    Some((name, &rest[..branch_end]))
}

fn is_safe_name(value: &str) -> bool {
    assert_docs_family_name(value).is_ok()
}

/// Approved families come only from the pinned main/llms.txt index, either as the upstream
/// family-to-branch mapping list or as links to a family's llms.txt in the pinned
/// repository. Unsafe or off-repository entries are ignored; the first entry wins for a
/// duplicate family or branch.
pub fn parse_families_from_llms(
    text: &str,
    max_families: usize,
) -> Result<Vec<DocsFamily>, DocsError> {
    let mut families: Vec<DocsFamily> = Vec::new();

    let mut add = |name: &str, branch: &str| -> Result<(), DocsError> {
        if !is_safe_name(name)
            || !is_safe_name(branch)
            || families
                .iter()
                .any(|f| f.name == name || f.branch == branch)
        {
            return Ok(());
        }
        if families.len() >= max_families {
            return Err(DocsError::coded(
                DocsErrorCode::IndexInvalid,
                format!("ServiceNow docs index lists more than {max_families} families"),
            ));
        }
        families.push(DocsFamily {
            name: name.to_owned(),
            branch: branch.to_owned(),
        });
        Ok(())
    };

    for line in text.split('\n') {
        if let Some((name, branch)) = parse_mapping_line(line) {
            add(name, branch)?;
        }
    }

    scan_markdown_links(text, |label, target| {
        let Some(rest) = target.strip_prefix(DOCS_REPO_RAW_BASE) else {
            return Ok(());
        };
        let Some(branch) = rest.strip_suffix("/llms.txt") else {
            return Ok(());
        };
        add(if is_safe_name(label) { label } else { branch }, branch)
    })?;

    Ok(families)
}

// ---------------------------------------------------------------------------------------
// Bounded reading
// ---------------------------------------------------------------------------------------

struct ReadLimits<'a> {
    max_bytes: u64,
    line_bytes: u64,
    budget: Option<&'a DownloadBudget>,
    label: &'a str,
}

impl ReadLimits<'_> {
    fn too_large(&self) -> DocsError {
        DocsError::coded(
            DocsErrorCode::TooLarge,
            format!(
                "ServiceNow docs response for {} exceeds {} bytes",
                self.label, self.max_bytes
            ),
        )
    }

    fn over_budget(&self) -> DocsError {
        DocsError::coded(
            DocsErrorCode::SyncBudget,
            format!(
                "ServiceNow docs sync exceeded its aggregate download budget while reading {}",
                self.label
            ),
        )
    }

    fn line_too_long(&self) -> DocsError {
        DocsError::coded(
            DocsErrorCode::LineTooLong,
            format!(
                "ServiceNow docs response for {} has a line longer than {} bytes",
                self.label, self.line_bytes
            ),
        )
    }
}

async fn read_bounded_text<B: DocsBody>(
    content_length: Option<&str>,
    body: &mut B,
    limits: &ReadLimits<'_>,
) -> Result<String, DocsError> {
    if let Some(declared) =
        content_length.filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
    {
        // Digits-only text that overflows u64 is certainly over any limit.
        let length = declared.parse::<u64>().unwrap_or(u64::MAX);
        if length > limits.max_bytes {
            return Err(limits.too_large());
        }
        if let Some(budget) = limits.budget
            && i64::try_from(length).unwrap_or(i64::MAX) > budget.remaining()
        {
            return Err(limits.over_budget());
        }
    }

    let mut collected: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut current_line: u64 = 0;
    loop {
        let chunk = body.next_chunk().await.map_err(|_| {
            DocsError::coded(
                DocsErrorCode::Network,
                format!(
                    "ServiceNow docs request for {} failed while reading the response",
                    limits.label
                ),
            )
        })?;
        let Some(chunk) = chunk else { break };
        let bytes = chunk.as_ref();
        total += bytes.len() as u64;
        if total > limits.max_bytes {
            return Err(limits.too_large());
        }
        if let Some(budget) = limits.budget
            && budget.consume(bytes.len()) < 0
        {
            return Err(limits.over_budget());
        }

        let mut rest = bytes;
        loop {
            let newline = rest.iter().position(|&b| b == b'\n');
            current_line += newline.unwrap_or(rest.len()) as u64;
            if current_line > limits.line_bytes {
                return Err(limits.line_too_long());
            }
            let Some(index) = newline else { break };
            current_line = 0;
            rest = &rest[index + 1..];
        }
        collected.extend_from_slice(bytes);
    }

    Ok(match String::from_utf8(collected) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    })
}

struct FetchParams<'a> {
    label: &'a str,
    timeout: Duration,
    max_bytes: u64,
    line_bytes: u64,
    options: RequestOptions<'a>,
}

fn redirect_error(label: &str) -> DocsError {
    DocsError::coded(
        DocsErrorCode::Redirect,
        format!("ServiceNow docs request for {label} was redirected; redirects are rejected"),
    )
}

async fn fetch_once<T: DocsTransport>(
    transport: &T,
    url: &str,
    params: &FetchParams<'_>,
) -> Result<String, DocsError> {
    let label = params.label;
    let response = transport.get(url).await.map_err(|error| match error {
        TransportError::Redirect => redirect_error(label),
        TransportError::Network => DocsError::coded(
            DocsErrorCode::Network,
            format!("ServiceNow docs request for {label} failed: network error"),
        ),
    })?;

    if (300..400).contains(&response.status) {
        return Err(redirect_error(label));
    }
    if !(200..300).contains(&response.status) {
        return Err(DocsError::coded(
            DocsErrorCode::HttpStatus,
            format!(
                "ServiceNow docs request for {label} failed with HTTP {}",
                response.status
            ),
        ));
    }

    let DocsResponse {
        content_length,
        mut body,
        ..
    } = response;
    let limits = ReadLimits {
        max_bytes: params.max_bytes,
        line_bytes: params.line_bytes,
        budget: params.options.budget,
        label,
    };
    read_bounded_text(content_length.as_deref(), &mut body, &limits).await
}

async fn fetch_bounded_text<T: DocsTransport>(
    transport: &T,
    url: &str,
    params: FetchParams<'_>,
) -> Result<String, DocsError> {
    let label = params.label;
    let guarded = async {
        tokio::time::timeout(params.timeout, fetch_once(transport, url, &params))
            .await
            .unwrap_or_else(|_| {
                Err(DocsError::coded(
                    DocsErrorCode::Timeout,
                    format!(
                        "ServiceNow docs request for {label} timed out after {} ms",
                        params.timeout.as_millis()
                    ),
                ))
            })
    };

    let aborted = || {
        DocsError::coded(
            DocsErrorCode::Aborted,
            format!("ServiceNow docs request for {label} was aborted"),
        )
    };
    match params.options.cancel {
        Some(cancel) => tokio::select! {
            biased;
            () = cancel.cancelled() => Err(aborted()),
            result = guarded => result,
        },
        None => guarded.await,
    }
}

// ---------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------

pub struct DocsClient<T> {
    transport: T,
    limits: DocsLimits,
    families: OnceCell<Vec<DocsFamily>>,
}

impl<T: DocsTransport> DocsClient<T> {
    pub fn new(transport: T, limits: DocsLimits) -> Self {
        Self {
            transport,
            limits,
            families: OnceCell::new(),
        }
    }

    #[must_use]
    pub const fn limits(&self) -> &DocsLimits {
        &self.limits
    }

    async fn request(
        &self,
        branch: &str,
        relative_path: &str,
        max_bytes: u64,
        options: RequestOptions<'_>,
    ) -> Result<String, DocsError> {
        let encoded_path = relative_path
            .split('/')
            .map(encode_uri_component)
            .collect::<Vec<_>>()
            .join("/");
        let url = format!(
            "{DOCS_REPO_RAW_BASE}{}/{encoded_path}",
            encode_uri_component(branch)
        );
        let label = format!("{branch}/{relative_path}");
        fetch_bounded_text(
            &self.transport,
            &url,
            FetchParams {
                label: &label,
                timeout: self.limits.request_timeout,
                max_bytes,
                line_bytes: self.limits.line_bytes,
                options,
            },
        )
        .await
    }

    /// Families approved by the pinned `main/llms.txt`. Cached after the first success;
    /// concurrent first callers share one fetch, and a failure is not cached.
    pub async fn list_families(
        &self,
        options: RequestOptions<'_>,
    ) -> Result<Vec<DocsFamily>, DocsError> {
        let load = self.families.get_or_try_init(|| async {
            let text = self
                .request(
                    DOCS_INDEX_REF,
                    "llms.txt",
                    self.limits.index_bytes,
                    RequestOptions {
                        budget: None,
                        ..options
                    },
                )
                .await?;
            parse_families_from_llms(&text, self.limits.max_families)
        });
        // The wait for a concurrent first fetch is cancellable too, not only the fetch itself.
        let families = match options.cancel {
            Some(cancel) => tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(DocsError::coded(
                        DocsErrorCode::Aborted,
                        format!("ServiceNow docs request for {DOCS_INDEX_REF}/llms.txt was aborted"),
                    ));
                }
                result = load => result?,
            },
            None => load.await?,
        };
        Ok(families.clone())
    }

    pub async fn resolve_family(
        &self,
        family: &str,
        options: RequestOptions<'_>,
    ) -> Result<DocsFamily, DocsError> {
        assert_docs_family_name(family)?;
        self.list_families(options)
            .await?
            .into_iter()
            .find(|entry| entry.name == family)
            .ok_or_else(|| {
                DocsError::coded(
                    DocsErrorCode::UnknownFamily,
                    format!(
                        "Unknown ServiceNow docs family \"{family}\"; use SN-Docs-Families to list families approved by the ServiceNowDocs index"
                    ),
                )
            })
    }

    pub async fn get_llms(
        &self,
        family: &str,
        options: RequestOptions<'_>,
    ) -> Result<String, DocsError> {
        let approved = self.resolve_family(family, options).await?;
        self.request(
            &approved.branch,
            "llms.txt",
            self.limits.index_bytes,
            options,
        )
        .await
    }

    pub async fn get_markdown(
        &self,
        family: &str,
        document_path: &str,
        options: RequestOptions<'_>,
    ) -> Result<String, DocsError> {
        let safe_path = normalize_docs_document_path(document_path)?;
        let approved = self.resolve_family(family, options).await?;
        self.request(
            &approved.branch,
            safe_path,
            self.limits.document_bytes,
            options,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::testing::{FakeGithub, INDEX_PATH, Route, doc_path, main_index};

    // Ported from tests/docs-github-client.test.js. HTTP-level behaviour that the in-memory
    // fake cannot show (no redirect following, no Authorization header, real streaming) is
    // covered against a loopback server in `transport::tests`.

    const DOC: &str = "markdown/flow/designer.md";

    fn doc_url_path() -> String {
        doc_path(DOC, "australia")
    }

    fn client_for(fake: &FakeGithub, limits: DocsLimits) -> DocsClient<FakeGithub> {
        DocsClient::new(fake.clone(), limits)
    }

    fn opts() -> RequestOptions<'static> {
        RequestOptions::default()
    }

    fn family(name: &str, branch: &str) -> DocsFamily {
        DocsFamily {
            name: name.to_owned(),
            branch: branch.to_owned(),
        }
    }

    #[test]
    fn parses_the_upstream_family_to_branch_mapping_list() {
        let index = main_index(&[("australia", "australia"), ("zurich", "zurich")]);
        assert_eq!(
            parse_families_from_llms(&index, 100).unwrap(),
            [family("australia", "australia"), family("zurich", "zurich")]
        );
    }

    #[test]
    fn parses_pinned_family_llms_links_and_ignores_off_repository_links() {
        let text = [
            "- [australia](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/llms.txt)",
            "- [washingtondc](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/washingtondc/llms.txt)",
            "- [evil](https://raw.githubusercontent.com/attacker/ServiceNowDocs/evil/llms.txt)",
            "- [dup](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/llms.txt)",
        ]
        .join("\n");
        assert_eq!(
            parse_families_from_llms(&text, 100).unwrap(),
            [
                family("australia", "australia"),
                family("washingtondc", "washingtondc")
            ]
        );
    }

    #[test]
    fn ignores_mapping_entries_with_unsafe_family_or_branch_names() {
        let text = [
            "- \"../x\" : \"australia\"",
            "- \"ok\" : \"feature/x\"",
            "- \"ok2\" : \"a..b\"",
            "- \"zurich\" : \"zurich\"",
        ]
        .join("\n");
        assert_eq!(
            parse_families_from_llms(&text, 100).unwrap(),
            [family("zurich", "zurich")]
        );
    }

    #[test]
    fn fails_when_the_index_lists_more_families_than_the_cap() {
        let index = main_index(&[("a", "a"), ("b", "b"), ("c", "c")]);
        let error = parse_families_from_llms(&index, 2).unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::IndexInvalid));
        assert!(error.message().contains("more than 2 families"));
    }

    #[tokio::test]
    async fn fetches_the_pinned_main_index() {
        let fake = FakeGithub::australia();
        let client = client_for(&fake, DocsLimits::default());

        assert_eq!(
            client.list_families(opts()).await.unwrap(),
            [family("australia", "australia")]
        );
        assert_eq!(fake.paths(), [INDEX_PATH]);
        // Cached: a second call does not hit the network again.
        client.list_families(opts()).await.unwrap();
        assert_eq!(fake.paths(), [INDEX_PATH]);
    }

    #[tokio::test]
    async fn resolves_the_approved_branch_from_the_index_and_encodes_a_nested_path() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), "# Designer\n\nBody");
        let client = client_for(&fake, DocsLimits::default());

        assert_eq!(
            client.get_markdown("australia", DOC, opts()).await.unwrap(),
            "# Designer\n\nBody"
        );
        assert_eq!(fake.paths(), [INDEX_PATH.to_owned(), doc_url_path()]);

        fake.set(
            &doc_path("a/b+c,d=e@f.md", "australia")
                .replace('+', "%2B")
                .replace(',', "%2C")
                .replace('=', "%3D")
                .replace('@', "%40"),
            "# Enc",
        );
        assert_eq!(
            client
                .get_markdown("australia", "a/b+c,d=e@f.md", opts())
                .await
                .unwrap(),
            "# Enc"
        );
    }

    #[tokio::test]
    async fn rejects_families_that_are_not_in_the_validated_main_index_before_fetching_documents() {
        let fake = FakeGithub::australia();
        let client = client_for(&fake, DocsLimits::default());

        let unknown = client
            .get_markdown("attacker", "markdown/x.md", opts())
            .await
            .unwrap_err();
        assert_eq!(unknown.code(), Some(DocsErrorCode::UnknownFamily));
        assert!(unknown.message().contains("Unknown ServiceNow docs family"));
        let invalid = client
            .get_markdown("../main", "markdown/x.md", opts())
            .await
            .unwrap_err();
        assert!(invalid.message().contains("Invalid ServiceNow docs family"));
        assert!(fake.paths().iter().all(|p| p == INDEX_PATH));
    }

    #[tokio::test]
    async fn rejects_unsafe_document_paths_without_any_request() {
        let long_deep = format!("{}x.md", "a/".repeat(40));
        let unsafe_paths = [
            "../secret.md",
            "markdown/../../other/x.md",
            "/markdown/x.md",
            "markdown%2F..%2Fx.md",
            "markdown/%2e%2e/x.md",
            "markdown\\x.md",
            "markdown/x.md?token=1",
            "markdown/x.md#frag",
            "markdown/x\u{0}.md",
            "markdown/x\n.md",
            "https://raw.githubusercontent.com/attacker/repo/main/x.md",
            "markdown/x.txt",
            "markdown//x.md",
            long_deep.as_str(),
        ];
        for unsafe_path in unsafe_paths {
            let fake = FakeGithub::australia();
            let client = client_for(&fake, DocsLimits::default());
            let error = client
                .get_markdown("australia", unsafe_path, opts())
                .await
                .unwrap_err();
            assert!(
                error.message().starts_with("Unsafe docs path"),
                "{unsafe_path:?}"
            );
            assert!(fake.paths().is_empty(), "{unsafe_path:?}");
        }
    }

    #[tokio::test]
    async fn rejects_redirects_instead_of_following_them() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Redirect);
        fake.set(&doc_path("markdown/other.md", "australia"), "# followed");
        let client = client_for(&fake, DocsLimits::default());

        let error = client
            .get_markdown("australia", DOC, opts())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::Redirect));
        assert!(error.message().to_lowercase().contains("redirect"));
        assert!(
            !fake
                .paths()
                .contains(&doc_path("markdown/other.md", "australia"))
        );
    }

    #[tokio::test]
    async fn sanitizes_http_failures_without_echoing_response_bodies() {
        let fake = FakeGithub::australia();
        fake.set(
            &doc_url_path(),
            Route::Status(403, "rate limited for token ghp_body_secret".into()),
        );
        let client = client_for(&fake, DocsLimits::default());

        let error = client
            .get_markdown("australia", DOC, opts())
            .await
            .unwrap_err();
        assert!(error.message().contains("HTTP 403"));
        for leaked in ["ghp_", "secret", "rate limited"] {
            assert!(!error.message().contains(leaked));
        }
        assert_eq!(error.code_str(), Some("DOCS_HTTP_STATUS"));
    }

    #[tokio::test]
    async fn rejects_an_oversized_content_length_before_reading_the_body() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Declared(2048, "x".repeat(2048)));
        let limits = DocsLimits {
            document_bytes: 1024,
            ..DocsLimits::default()
        };
        let client = client_for(&fake, limits);

        let error = client
            .get_markdown("australia", DOC, opts())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::TooLarge));
        assert_eq!(
            error.message(),
            "ServiceNow docs response for australia/markdown/flow/designer.md exceeds 1024 bytes"
        );
    }

    #[tokio::test]
    async fn cancels_a_never_ending_streamed_body_once_it_exceeds_the_byte_limit() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Endless);
        let limits = DocsLimits {
            document_bytes: 64 * 1024,
            request_timeout: Duration::from_secs(20),
            ..DocsLimits::default()
        };
        let client = client_for(&fake, limits);

        let started = Instant::now();
        let error = client
            .get_markdown("australia", DOC, opts())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::TooLarge));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(fake.endless_bytes_written() < 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn rejects_a_line_longer_than_the_per_line_limit_even_across_chunks() {
        let body = format!("# ok\n{}\n", "z".repeat(300));
        for route in [Route::Body(body.clone()), Route::Chunked(body, 7)] {
            let fake = FakeGithub::australia();
            fake.set(&doc_url_path(), route);
            let limits = DocsLimits {
                line_bytes: 256,
                ..DocsLimits::default()
            };
            let client = client_for(&fake, limits);
            let error = client
                .get_markdown("australia", DOC, opts())
                .await
                .unwrap_err();
            assert_eq!(error.code(), Some(DocsErrorCode::LineTooLong));
        }
    }

    #[tokio::test]
    async fn enforces_the_index_byte_limit() {
        let fake = FakeGithub::australia();
        fake.set(
            INDEX_PATH,
            format!(
                "{}{}",
                main_index(&[("australia", "australia")]),
                "#\n".repeat(1024)
            ),
        );
        let limits = DocsLimits {
            index_bytes: 1024,
            ..DocsLimits::default()
        };
        let client = client_for(&fake, limits);

        let error = client.list_families(opts()).await.unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::TooLarge));
        // A failed index fetch is not cached.
        fake.set(INDEX_PATH, main_index(&[("australia", "australia")]));
        assert_eq!(client.list_families(opts()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn times_out_a_stalled_response_body() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Stall("# partial\n".into()));
        let limits = DocsLimits {
            request_timeout: Duration::from_millis(100),
            ..DocsLimits::default()
        };
        let client = client_for(&fake, limits);

        let error = client
            .get_markdown("australia", DOC, opts())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::Timeout));
        assert!(error.message().ends_with("timed out after 100 ms"));
    }

    #[tokio::test]
    async fn aborts_on_cancellation_without_a_request_when_already_cancelled() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Hang);
        let client = client_for(&fake, DocsLimits::default());
        let token = CancellationToken::new();
        token.cancel();
        let options = RequestOptions {
            cancel: Some(&token),
            budget: None,
        };
        let error = client.list_families(options).await.unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::Aborted));
        assert!(fake.paths().is_empty());

        let live = CancellationToken::new();
        let canceller = live.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let hung = RequestOptions {
            cancel: Some(&live),
            budget: None,
        };
        let error = client
            .get_markdown("australia", DOC, hung)
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::Aborted));
    }

    #[tokio::test]
    async fn a_waiter_on_the_shared_index_fetch_can_be_cancelled_independently() {
        let fake = FakeGithub::default();
        fake.set(INDEX_PATH, Route::Hang);
        let client = std::sync::Arc::new(client_for(&fake, DocsLimits::default()));

        // The first caller starts (and hangs on) the shared fetch.
        let first = tokio::spawn({
            let client = std::sync::Arc::clone(&client);
            async move { client.list_families(opts()).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let started = Instant::now();
        let waiter = RequestOptions {
            cancel: Some(&token),
            budget: None,
        };
        let error = client.list_families(waiter).await.unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::Aborted));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            fake.paths(),
            [INDEX_PATH],
            "the waiter must not start a second fetch"
        );
        first.abort();
    }

    #[tokio::test]
    async fn the_aggregate_budget_rejects_declared_and_streamed_overruns() {
        let fake = FakeGithub::australia();
        fake.set(&doc_url_path(), Route::Declared(500, "x".repeat(500)));
        let client = client_for(&fake, DocsLimits::default());
        let budget = DownloadBudget::new(100);
        let options = RequestOptions {
            cancel: None,
            budget: Some(&budget),
        };
        let error = client
            .get_markdown("australia", DOC, options)
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::SyncBudget));

        fake.set(&doc_url_path(), "y".repeat(500));
        let error = client
            .get_markdown("australia", DOC, options)
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::SyncBudget));
    }

    #[test]
    fn uses_production_limits_and_exposes_the_documented_caps() {
        let limits = DocsLimits::default();
        assert_eq!(limits.index_bytes, 1024 * 1024);
        assert_eq!(limits.document_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.line_bytes, 16 * 1024);
        assert_eq!(limits.max_links, 1000);
        assert_eq!(limits.max_families, 100);
        assert_eq!(limits.sync_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.request_timeout, Duration::from_millis(30_000));
        assert_eq!(limits.sync_timeout, Duration::from_millis(600_000));
    }

    #[test]
    fn link_scanning_is_linear_on_hostile_input() {
        let hostile = [
            format!("{}]({}", "[".repeat(20_000), ")".repeat(20_000)),
            format!("{}](", "x".repeat(200)).repeat(200) + ")",
            "[a](".repeat(10_000),
            "](x)".repeat(10_000),
        ]
        .join("\n");
        let started = Instant::now();
        scan_markdown_links::<()>(&hostile, |_, _| Ok(())).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
