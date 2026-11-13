//! S1 transport spike. `hpm-s1` (no args) or `hpm-s1 stdio` → stdio;
//! `hpm-s1 http [--port N]` → single-user HTTP on 127.0.0.1 (all three HTTP paths on `/mcp`).
//! HTTP env (6.2.0 names): HAPPY_MCP_API_TOKEN (required), HAPPY_MCP_ALLOWED_HOSTS,
//! HAPPY_MCP_ALLOWED_ORIGINS, PORT.

mod http;
mod server;
mod sn;

use std::sync::Arc;

use rmcp::ServiceExt;

fn split_list(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// 6.2.0 `validateApiToken`: 64 hex or 43 base64url chars (`src/http-server.js:49-66`).
fn valid_token(t: &str) -> bool {
    let hex = t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit());
    let b64 = t.len() == 43 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    hex || b64
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let client = Arc::new(sn::resolve_instance().map(sn::Client::new));
    match &*client {
        Ok(c) => server::event(format!("instance {} ({})", c.instance().name, c.instance().url)),
        Err(e) => server::event(format!("no ServiceNow instance: {e}")),
    }
    let handler = server::HpmServer::new(client);

    match args.first().map(String::as_str) {
        None | Some("stdio") => {
            let running = handler.serve(rmcp::transport::stdio()).await.expect("stdio serve");
            let reason = running.waiting().await;
            server::event(format!("stdio ended: {reason:?}"));
        }
        Some("http") => {
            let port: u16 = args
                .iter()
                .position(|a| a == "--port")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .or_else(|| std::env::var("PORT").ok())
                .and_then(|p| p.parse().ok())
                .unwrap_or(3000);
            let token = std::env::var("HAPPY_MCP_API_TOKEN").unwrap_or_default();
            if !valid_token(&token) {
                eprintln!("HAPPY_MCP_API_TOKEN must be 64 hex or 43 base64url characters");
                std::process::exit(1);
            }
            let sec = http::Security {
                token,
                port,
                extra_hosts: split_list("HAPPY_MCP_ALLOWED_HOSTS"),
                origins: split_list("HAPPY_MCP_ALLOWED_ORIGINS"),
            };
            let app = http::router(handler, sec);
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
            server::event(format!("listening on http://127.0.0.1:{port}/mcp"));
            axum::serve(listener, app).await.expect("serve");
        }
        Some(other) => {
            eprintln!("unknown mode {other}; use `stdio` or `http [--port N]`");
            std::process::exit(2);
        }
    }
}
