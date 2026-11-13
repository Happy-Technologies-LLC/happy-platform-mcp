//! HTTP transports (lane E), productionizing `spikes/s1-transport`: Streamable HTTP
//! (stateless modern and legacy session paths) and the hand-written legacy SSE
//! adapter, with Host/Origin enforcement, session cap and idle expiry, and the
//! RFC §5.3 dispatch and -32022 mapping.

pub mod auth;
pub mod limits;
pub mod sse;
pub mod streamable_http;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub use streamable_http::{HttpApp, HttpConfig, JsonFn, PeerAddrs};

/// 6.2.0 `sendJson(res, status, { error })`.
pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}
