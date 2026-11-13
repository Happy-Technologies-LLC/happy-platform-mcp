//! Minimal loopback HTTP/1.1 server shared by both stubs, and the request record
//! (`describeRequest` in `scripts/golden/lib/stub-sn.mjs`). One request per
//! connection (`Connection: close`); bodies by `Content-Length` or chunked.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Transport-incidental headers are not part of the behavioural contract.
const DROPPED_HEADERS: &[&str] = &["connection", "host", "content-length", "accept-encoding"];
/// Credential-bearing headers are never stored: only their presence/scheme is.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-usertoken",
    "x-csrf-token",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub scheme: String,
    pub session_cookie: bool,
    pub user_token: bool,
}

/// One recorded inbound request, as `describeRequest` stores it.
#[derive(Clone, Debug)]
pub struct Described {
    pub method: String,
    /// Raw (still percent-encoded) path, like `URL.pathname`.
    pub path: String,
    pub raw_query: String,
    /// Decoded `URLSearchParams` pairs, in order.
    pub query: Vec<(String, String)>,
    pub headers: BTreeMap<String, String>,
    pub credentials: Credentials,
    /// `None`: key absent (form bodies). `Some(Null)`: empty body.
    pub body: Option<Value>,
    pub form: Option<Vec<(String, String)>>,
}

impl Described {
    pub fn query_value(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The `outbound[]` entry of a golden step.
    pub fn to_json(&self, target: &str) -> Value {
        let pairs =
            |p: &[(String, String)]| -> Value { p.iter().map(|(k, v)| json!([k, v])).collect() };
        let mut out = Map::new();
        out.insert("target".into(), json!(target));
        out.insert("method".into(), json!(self.method));
        out.insert("path".into(), json!(self.path));
        out.insert("rawQuery".into(), json!(self.raw_query));
        out.insert("query".into(), pairs(&self.query));
        out.insert("headers".into(), json!(self.headers));
        out.insert(
            "credentials".into(),
            json!({
                "scheme": self.credentials.scheme,
                "sessionCookie": self.credentials.session_cookie,
                "userToken": self.credentials.user_token,
            }),
        );
        if let Some(body) = &self.body {
            out.insert("body".into(), body.clone());
        }
        if let Some(form) = &self.form {
            out.insert("form".into(), pairs(form));
        }
        Value::Object(out)
    }
}

/// `URLSearchParams` parsing: `&`-separated, `+` is a space, percent-decoding with
/// invalid escapes kept literally, decoded bytes read as lossy UTF-8.
pub fn parse_search_params(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
        } else if b == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// `describeRequest(req, rawBody)`.
pub fn describe(
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
    raw_body: &str,
) -> Described {
    let (path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let kept = headers
        .iter()
        .filter(|(k, _)| {
            !DROPPED_HEADERS.contains(&k.as_str()) && !CREDENTIAL_HEADERS.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let credentials = Credentials {
        scheme: headers
            .get("authorization")
            .map(|a| a.split(' ').next().unwrap_or("").to_lowercase())
            .unwrap_or_else(|| "none".into()),
        session_cookie: headers.get("cookie").is_some_and(|c| !c.is_empty()),
        user_token: headers.get("x-usertoken").is_some_and(|c| !c.is_empty()),
    };
    let content_type = headers
        .get("content-type")
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_default();
    let (raw, json_body) = if raw_body.is_empty() {
        (None, None)
    } else if content_type.contains("json") {
        (
            Some(raw_body.to_owned()),
            serde_json::from_str::<Value>(raw_body).ok(),
        )
    } else {
        (Some(raw_body.to_owned()), None)
    };
    let form = match &raw {
        Some(r) if content_type.contains("x-www-form-urlencoded") => Some(parse_search_params(r)),
        _ => None,
    };
    let body = match (json_body, &form) {
        (Some(j), _) => Some(j),
        (None, Some(_)) => None,
        (None, None) => Some(raw.map_or(Value::Null, Value::String)),
    };
    Described {
        method: method.to_owned(),
        path: path.to_owned(),
        raw_query: raw_query.to_owned(),
        query: parse_search_params(raw_query),
        headers: kept,
        credentials,
        body,
        form,
    }
}

/// A stub response. `body: None` sends no body.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// Raw request as received; `describe` turns it into a [`Described`].
pub struct RawRequest {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

pub type Handler = Arc<dyn Fn(RawRequest) -> Reply + Send + Sync>;

/// Binds `127.0.0.1:0` and serves `handler` until the runtime stops. Returns the port.
pub async fn serve(handler: Handler) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback stub");
    let port = listener.local_addr().expect("stub address").port();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = one_request(sock, handler).await;
            });
        }
    });
    port
}

async fn one_request(mut sock: TcpStream, handler: Handler) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut rest = buf[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or("").split(' ');
    let method = start.next().unwrap_or("").to_owned();
    let target = start.next().unwrap_or("/").to_owned();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        let joiner = if name == "cookie" { "; " } else { ", " };
        headers
            .entry(name)
            .and_modify(|v| {
                v.push_str(joiner);
                v.push_str(&value);
            })
            .or_insert(value);
    }

    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked(&mut sock, &mut rest).await?
    } else {
        let len: usize = headers
            .get("content-length")
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        while rest.len() < len {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        rest.truncate(len);
        rest
    };

    let reply = handler(RawRequest {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let mut out = format!("HTTP/1.1 {} Stub\r\n", reply.status);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = reply.body.unwrap_or_default();
    if reply.status != 204 {
        out.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    out.push_str("connection: close\r\n\r\n");
    sock.write_all(out.as_bytes()).await?;
    if reply.status != 204 {
        sock.write_all(&body).await?;
    }
    sock.shutdown().await
}

async fn read_chunked(sock: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // Size line.
        let line_end = loop {
            if let Some(at) = buf.windows(2).position(|w| w == b"\r\n") {
                break at;
            }
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok(body);
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let size_text = String::from_utf8_lossy(&buf[..line_end]).into_owned();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        buf.drain(..line_end + 2);
        if size == 0 {
            return Ok(body);
        }
        while buf.len() < size + 2 {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                body.extend_from_slice(buf);
                return Ok(body);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}
