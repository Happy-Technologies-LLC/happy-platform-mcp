//! HTTP transports (lane E), productionizing `spikes/s1-transport`: Streamable HTTP
//! (stateless modern and legacy session paths) and the hand-written legacy SSE
//! adapter, with Host/Origin enforcement, session cap and idle expiry, and the
//! RFC §5.3 dispatch and -32022 mapping.

pub mod auth;
pub mod limits;
pub mod sse;
pub mod streamable_http;
