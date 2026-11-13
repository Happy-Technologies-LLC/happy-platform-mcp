//! Production [`DocsTransport`] on `reqwest`: anonymous, redirect-free, no ambient credentials.

use crate::client::{
    DOCS_RAW_ORIGIN, DocsBody, DocsClient, DocsLimits, DocsResponse, DocsTransport, TransportError,
};
use crate::error::DocsError;

pub struct ReqwestTransport {
    client: reqwest::Client,
    origin: String,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, DocsError> {
        Self::with_origin(DOCS_RAW_ORIGIN)
    }

    /// The client always builds URLs on [`DOCS_RAW_ORIGIN`]; `origin` only substitutes the
    /// scheme and authority actually dialled. Production uses the pinned origin; tests point
    /// it at a loopback server.
    pub(crate) fn with_origin(origin: &str) -> Result<Self, DocsError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("happy-platform-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| {
                DocsError::plain("Failed to initialize the ServiceNow docs HTTP client")
            })?;
        Ok(Self {
            client,
            origin: origin.to_owned(),
        })
    }
}

pub struct ReqwestBody {
    response: reqwest::Response,
}

impl DocsBody for ReqwestBody {
    type Chunk = bytes::Bytes;

    async fn next_chunk(&mut self) -> Result<Option<Self::Chunk>, TransportError> {
        self.response
            .chunk()
            .await
            .map_err(|_| TransportError::Network)
    }
}

impl DocsTransport for ReqwestTransport {
    type Body = ReqwestBody;

    async fn get(&self, url: &str) -> Result<DocsResponse<ReqwestBody>, TransportError> {
        let path = url
            .strip_prefix(DOCS_RAW_ORIGIN)
            .filter(|rest| rest.starts_with('/'))
            .ok_or(TransportError::Network)?;
        let response = self
            .client
            .get(format!("{}{path}", self.origin))
            .header(reqwest::header::ACCEPT, "text/plain")
            .send()
            .await
            .map_err(|error| {
                if error.is_redirect() {
                    TransportError::Redirect
                } else {
                    TransportError::Network
                }
            })?;
        let content_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(DocsResponse {
            status: response.status().as_u16(),
            content_length,
            body: ReqwestBody { response },
        })
    }
}

impl DocsClient<ReqwestTransport> {
    /// The production client: pinned origin, anonymous, redirects rejected.
    pub fn production(limits: DocsLimits) -> Result<Self, DocsError> {
        Ok(Self::new(ReqwestTransport::new()?, limits))
    }
}

// Loopback tests for the HTTP behaviour the in-memory fake cannot show. Ported from the
// real-socket cases of tests/docs-github-client.test.js. No traffic leaves 127.0.0.1.
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::client::RequestOptions;
    use crate::error::DocsErrorCode;
    use crate::testing::{INDEX_PATH, main_index};

    const DOC_PATH: &str = "/ServiceNow/ServiceNowDocs/australia/markdown/x.md";

    enum Reply {
        /// Complete raw HTTP response.
        Raw(String),
        /// Headers, then an endless body until the peer disconnects.
        Endless,
        /// Headers and a first chunk, then silence.
        Stall,
    }

    struct Server {
        origin: String,
        heads: Arc<Mutex<Vec<String>>>,
        endless_dropped: Arc<Mutex<bool>>,
    }

    impl Server {
        fn paths(&self) -> Vec<String> {
            self.heads
                .lock()
                .unwrap()
                .iter()
                .map(|head| head.split(' ').nth(1).unwrap_or_default().to_owned())
                .collect()
        }
    }

    fn ok(body: &str) -> Reply {
        Reply::Raw(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    async fn serve(doc: fn() -> Reply) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let endless_dropped = Arc::new(Mutex::new(false));
        let (log, dropped) = (Arc::clone(&heads), Arc::clone(&endless_dropped));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, dropped) = (Arc::clone(&log), Arc::clone(&dropped));
                tokio::spawn(async move { handle(stream, doc, log, dropped).await });
            }
        });
        Server {
            origin,
            heads,
            endless_dropped,
        }
    }

    async fn handle(
        mut stream: TcpStream,
        doc: fn() -> Reply,
        log: Arc<Mutex<Vec<String>>>,
        dropped: Arc<Mutex<bool>>,
    ) {
        let mut head = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&buffer[..n]),
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
        log.lock().unwrap().push(head);
        let reply = if path == INDEX_PATH {
            ok(&main_index(&[("australia", "australia")]))
        } else {
            doc()
        };
        match reply {
            Reply::Raw(raw) => {
                let _ = stream.write_all(raw.as_bytes()).await;
            }
            Reply::Endless => {
                let header =
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n";
                if stream.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                let line = format!("{}\n", "y".repeat(1023));
                loop {
                    if stream.write_all(line.as_bytes()).await.is_err() {
                        *dropped.lock().unwrap() = true;
                        return;
                    }
                }
            }
            Reply::Stall => {
                let partial = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n# partial\n";
                let _ = stream.write_all(partial.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }

    fn client(server: &Server, limits: DocsLimits) -> DocsClient<ReqwestTransport> {
        DocsClient::new(
            ReqwestTransport::with_origin(&server.origin).unwrap(),
            limits,
        )
    }

    fn opts() -> RequestOptions<'static> {
        RequestOptions::default()
    }

    #[tokio::test]
    async fn fetches_documents_anonymously_and_without_ambient_credentials() {
        let server = serve(|| ok("# Doc\n")).await;
        let client = client(&server, DocsLimits::default());

        assert_eq!(
            client
                .get_markdown("australia", "markdown/x.md", opts())
                .await
                .unwrap(),
            "# Doc\n"
        );

        let heads = server.heads.lock().unwrap().clone();
        assert_eq!(server.paths(), [INDEX_PATH, DOC_PATH]);
        for head in heads {
            let lower = head.to_lowercase();
            for forbidden in ["authorization:", "cookie:", "proxy-authorization:"] {
                assert!(!lower.contains(forbidden), "{forbidden}");
            }
            assert!(lower.contains("accept: text/plain"));
            assert!(lower.contains("user-agent: happy-platform-mcp/"));
        }
    }

    #[tokio::test]
    async fn does_not_follow_redirects() {
        let server = serve(|| {
            Reply::Raw("HTTP/1.1 302 Found\r\nLocation: /elsewhere.md\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())
        })
        .await;
        let client = client(&server, DocsLimits::default());

        let error = client
            .get_markdown("australia", "markdown/x.md", opts())
            .await
            .unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::Redirect));
        assert_eq!(server.paths(), [INDEX_PATH, DOC_PATH]);
    }

    #[tokio::test]
    async fn http_errors_do_not_echo_the_response_body() {
        let server = serve(|| {
            let body = "rate limited for token ghp_body_secret";
            Reply::Raw(format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        })
        .await;
        let client = client(&server, DocsLimits::default());

        let error = client
            .get_markdown("australia", "markdown/x.md", opts())
            .await
            .unwrap_err();

        assert!(error.message().contains("HTTP 403"));
        assert!(!error.message().contains("ghp_"));
        assert!(!error.message().contains("rate limited"));
    }

    #[tokio::test]
    async fn rejects_an_oversized_content_length_from_the_wire() {
        let server = serve(|| ok(&"x".repeat(4096))).await;
        let limits = DocsLimits {
            document_bytes: 1024,
            ..DocsLimits::default()
        };
        let client = client(&server, limits);

        let error = client
            .get_markdown("australia", "markdown/x.md", opts())
            .await
            .unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::TooLarge));
    }

    #[tokio::test]
    async fn cancels_a_never_ending_stream_and_the_server_sees_the_disconnect() {
        let server = serve(|| Reply::Endless).await;
        let limits = DocsLimits {
            document_bytes: 64 * 1024,
            ..DocsLimits::default()
        };
        let client = client(&server, limits);

        let started = Instant::now();
        let error = client
            .get_markdown("australia", "markdown/x.md", opts())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(DocsErrorCode::TooLarge));
        assert!(started.elapsed() < Duration::from_secs(5));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !*server.endless_dropped.lock().unwrap() {
            assert!(
                Instant::now() < deadline,
                "server never saw the client disconnect"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn times_out_a_stalled_body() {
        let server = serve(|| Reply::Stall).await;
        let limits = DocsLimits {
            request_timeout: Duration::from_millis(200),
            ..DocsLimits::default()
        };
        let client = client(&server, limits);

        let error = client
            .get_markdown("australia", "markdown/x.md", opts())
            .await
            .unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::Timeout));
    }

    #[tokio::test]
    async fn reports_connection_failures_without_leaking_the_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = DocsClient::new(
            ReqwestTransport::with_origin(&origin).unwrap(),
            DocsLimits::default(),
        );

        let error = client.list_families(opts()).await.unwrap_err();

        assert_eq!(error.code(), Some(DocsErrorCode::Network));
        assert!(!error.message().contains("127.0.0.1"));
    }
}
