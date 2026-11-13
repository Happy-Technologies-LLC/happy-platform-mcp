//! In-memory stand-in for raw.githubusercontent.com, used by client and sync tests. It
//! implements [`DocsTransport`], so no sockets are involved. Behaviours mirror the Node
//! fake server in `tests/helpers/docs-fake-github.js`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::client::{DOCS_RAW_ORIGIN, DocsBody, DocsResponse, DocsTransport, TransportError};

pub(crate) const REPO_PREFIX: &str = "/ServiceNow/ServiceNowDocs";
pub(crate) const INDEX_PATH: &str = "/ServiceNow/ServiceNowDocs/main/llms.txt";

pub(crate) fn doc_path(path: &str, branch: &str) -> String {
    format!("{REPO_PREFIX}/{branch}/{path}")
}

pub(crate) fn main_index(families: &[(&str, &str)]) -> String {
    let mut lines = vec![
        "# ServiceNow Product Documentation".to_owned(),
        String::new(),
        "- Family-to-branch mapping is:".to_owned(),
    ];
    lines.extend(
        families
            .iter()
            .map(|(name, branch)| format!("    - \"{name}\" : \"{branch}\" -- {name} family")),
    );
    lines.push(String::new());
    lines.join("\n")
}

#[derive(Clone)]
pub(crate) enum Route {
    /// 200 with the body in one chunk.
    Body(String),
    /// 200 with the body split into chunks of the given size.
    Chunked(String, usize),
    /// Arbitrary status with a body that must never be echoed.
    Status(u16, String),
    /// 302 (the transport reports it, the body is empty).
    Redirect,
    /// 200 with a declared `Content-Length` (independent of the actual body).
    Declared(u64, String),
    /// 200 with a never-ending stream of 1 KiB lines.
    Endless,
    /// 200 that sends one chunk and then stalls forever.
    Stall(String),
    /// Never answers.
    Hang,
}

impl From<&str> for Route {
    fn from(body: &str) -> Self {
        Self::Body(body.to_owned())
    }
}

impl From<String> for Route {
    fn from(body: String) -> Self {
        Self::Body(body)
    }
}

#[derive(Default)]
pub(crate) struct Inner {
    routes: Mutex<HashMap<String, Route>>,
    requests: Mutex<Vec<String>>,
    endless_written: AtomicU64,
}

#[derive(Clone, Default)]
pub(crate) struct FakeGithub {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeGithub {
    /// A fake that already serves the family index for `families`.
    pub(crate) fn with_families(families: &[(&str, &str)]) -> Self {
        let fake = Self::default();
        fake.set(INDEX_PATH, main_index(families));
        fake
    }

    pub(crate) fn australia() -> Self {
        Self::with_families(&[("australia", "australia")])
    }

    pub(crate) fn set(&self, path: &str, route: impl Into<Route>) {
        lock(&self.inner.routes).insert(path.to_owned(), route.into());
    }

    pub(crate) fn paths(&self) -> Vec<String> {
        lock(&self.inner.requests).clone()
    }

    pub(crate) fn endless_bytes_written(&self) -> u64 {
        self.inner.endless_written.load(Ordering::Relaxed)
    }
}

pub(crate) enum FakeBody {
    Chunks(VecDeque<Vec<u8>>),
    Endless(Arc<Inner>),
    Stall(Option<Vec<u8>>),
}

impl DocsBody for FakeBody {
    type Chunk = Vec<u8>;

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        match self {
            Self::Chunks(chunks) => Ok(chunks.pop_front()),
            Self::Endless(inner) => {
                tokio::task::yield_now().await;
                let mut line = vec![b'y'; 1023];
                line.push(b'\n');
                inner.endless_written.fetch_add(1024, Ordering::Relaxed);
                Ok(Some(line))
            }
            Self::Stall(first) => match first.take() {
                Some(chunk) => Ok(Some(chunk)),
                None => std::future::pending().await,
            },
        }
    }
}

fn response(status: u16, content_length: Option<u64>, body: FakeBody) -> DocsResponse<FakeBody> {
    DocsResponse {
        status,
        content_length: content_length.map(|length| length.to_string()),
        body,
    }
}

fn single(body: &str) -> FakeBody {
    FakeBody::Chunks(VecDeque::from([body.as_bytes().to_vec()]))
}

impl DocsTransport for FakeGithub {
    type Body = FakeBody;

    async fn get(&self, url: &str) -> Result<DocsResponse<FakeBody>, TransportError> {
        let path = url
            .strip_prefix(DOCS_RAW_ORIGIN)
            .filter(|rest| rest.starts_with('/'))
            .expect("unpinned docs URL requested");
        lock(&self.inner.requests).push(path.to_owned());
        let route = lock(&self.inner.routes).get(path).cloned();
        Ok(match route {
            None => response(404, None, single("not found")),
            Some(Route::Body(body)) => response(200, None, single(&body)),
            Some(Route::Chunked(body, size)) => response(
                200,
                None,
                FakeBody::Chunks(body.as_bytes().chunks(size).map(<[u8]>::to_vec).collect()),
            ),
            Some(Route::Status(status, body)) => response(status, None, single(&body)),
            Some(Route::Redirect) => response(302, None, FakeBody::Chunks(VecDeque::new())),
            Some(Route::Declared(length, body)) => response(200, Some(length), single(&body)),
            Some(Route::Endless) => response(200, None, FakeBody::Endless(Arc::clone(&self.inner))),
            Some(Route::Stall(first)) => {
                response(200, None, FakeBody::Stall(Some(first.into_bytes())))
            }
            Some(Route::Hang) => std::future::pending().await,
        })
    }
}
