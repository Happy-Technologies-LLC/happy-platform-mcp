//! Replays `fixtures/golden/surface/*.json` (6.2.0 protocol surface: initialize
//! per protocol version, tools/list, resources, unknown methods, malformed wire
//! lines, `--docs-only`) through the real single-user composition and the MCP
//! stdio handler (`support::harness::run_surface`).
//!
//! Every exchange is compared exactly, except:
//! - [`DEVIATIONS`]: documented 7.0 behavior that replaces the recorded one;
//! - `initialize`: `serverInfo.version` is this package's version (7.0 reports
//!   its own version; 6.2.0 reported `6.2.0`), and 7.0 does not advertise the
//!   empty `prompts` capability (see [`DEVIATIONS`] for `prompts/*`);
//! - `tools/list`: compared over the tools the composition registers (golden
//!   order, name, description, schema); a golden tool no family registers yet is
//!   lane-pending; each recorded tool is replaced by its entry in the 7.0 list
//!   (`fixtures/tools-list-7.0.json`: D-05 annotation hints and owner text
//!   corrections, checked against 6.2.0 by `tests/tools_list.rs`);
//! - a `tools/call` of a golden tool no family registers: lane-pending, must be
//!   `-32602 Unknown tool`;
//! - resources while the composition serves none (lane E pending): `resources/list`
//!   must be empty and `resources/read` `-32603 Unknown resource`.

#[allow(dead_code, reason = "support is shared with golden_tools.rs")]
mod support;

use serde_json::{Value, json};
use support::harness::{self, World};

/// A recorded exchange 7.0 deliberately answers differently (and with no
/// ServiceNow request).
struct Deviation {
    /// Only this golden exchange (`None`: every golden).
    at: Option<(&'static str, usize)>,
    method: &'static str,
    /// `params.name` of the request (`""`: absent / any).
    name: Option<&'static str>,
    expected: fn(&str) -> Value,
    reason: &'static str,
}

const DEVIATIONS: &[Deviation] = &[
    Deviation {
        at: Some(("surface/docs-only", 4)),
        method: "tools/call",
        name: Some("SN-Query-Table"),
        expected: |_| json!({"error": {"code": -32602, "message": "Unknown tool: SN-Query-Table"}}),
        reason: "--docs-only registers only the docs tools; any other name is an unknown \
                 tool, -32602 (RFC C6; 6.2.0: isError `unavailable in docs-only mode`)",
    },
    Deviation {
        at: Some(("surface/full-default", 15)),
        method: "tools/call",
        name: Some("SN-Query-Table"),
        expected: |_| json!({"error": {"code": -32602, "message": "missing required property: table_name"}}),
        reason: "7.0 validates arguments against the tool schema before any request \
                 (6.2.0 queried /api/now/table/undefined)",
    },
    Deviation {
        at: Some(("surface/full-default", 16)),
        method: "tools/call",
        name: Some("SN-Query-Table"),
        expected: |_| json!({"error": {"code": -32602, "message": "argument /limit must be of type number"}}),
        reason: "7.0 validates arguments against the tool schema before any request \
                 (6.2.0 sent sysparm_limit=not-a-number)",
    },
    Deviation {
        at: None,
        method: "tools/call",
        name: Some("SN-Does-Not-Exist"),
        expected: |_| json!({"error": {"code": -32602, "message": "Unknown tool: SN-Does-Not-Exist"}}),
        reason: "RFC §10.1 step 1 (C6): unknown tool is JSON-RPC -32602, not an isError result",
    },
    Deviation {
        at: None,
        method: "tools/call",
        name: Some(""),
        expected: |m| json!({"error": {"code": -32601, "message": m}}),
        reason: "7.0 protocol stack: a tools/call without params is rejected as -32601 \
                 (6.2.0: -32603 with the zod issue list)",
    },
    Deviation {
        at: None,
        method: "resources/templates/list",
        name: None,
        expected: |_| json!({"result": {"resourceTemplates": []}}),
        reason: "7.0 protocol stack answers the resources capability's template list empty",
    },
    Deviation {
        at: None,
        method: "prompts/list",
        name: None,
        expected: |_| json!({"result": {"prompts": []}}),
        reason: "7.0 protocol stack: empty prompt list (6.2.0: -32601)",
    },
    Deviation {
        at: None,
        method: "completion/complete",
        name: None,
        expected: |_| json!({"result": {"completion": {"values": []}}}),
        reason: "7.0 protocol stack: empty completion (6.2.0: -32601)",
    },
    Deviation {
        at: None,
        method: "prompts/get",
        name: None,
        expected: |m| json!({"error": {"code": -32601, "message": m}}),
        reason: "7.0 protocol stack: -32601 message names the method (6.2.0: `Method not found`)",
    },
    Deviation {
        at: None,
        method: "logging/setLevel",
        name: None,
        expected: |m| json!({"error": {"code": -32601, "message": m}}),
        reason: "7.0 protocol stack: -32601 message names the method (6.2.0: `Method not found`)",
    },
    Deviation {
        at: None,
        method: "no/such/method",
        name: None,
        expected: |m| json!({"error": {"code": -32601, "message": m}}),
        reason: "7.0 protocol stack: -32601 message names the method (6.2.0: `Method not found`)",
    },
];

/// Wire lines 6.2.0 dropped silently that 7.0 answers with `-32600` (empty batch,
/// request without method, `jsonrpc: "1.0"`).
const INVALID_REQUEST_WIRE: &[&str] = &[
    "[]",
    r#"{"jsonrpc":"2.0","id":902}"#,
    r#"{"jsonrpc":"1.0","id":903,"method":"ping"}"#,
];

fn deviation(id: &str, i: usize, method: &str, params: &Value) -> Option<&'static Deviation> {
    let name = params["name"].as_str().unwrap_or("");
    DEVIATIONS.iter().find(|d| {
        d.at.is_none_or(|(g, n)| g == id && n == i)
            && d.method == method
            && d.name.is_none_or(|n| n == name)
    })
}

/// The recorded `tools/list` entries swapped for their 7.0 versions (same order).
fn as_7_0(tools: &Value) -> Vec<Value> {
    let path: std::path::PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "fixtures",
        "tools-list-7.0.json",
    ]
    .iter()
    .collect();
    let list: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("tools-list-7.0.json"))
            .expect("7.0 list");
    let list = list["tools"].as_array().expect("7.0 tools");
    tools
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            list.iter()
                .find(|n| n["name"] == t["name"])
                .unwrap_or_else(|| panic!("{} not in tools-list-7.0.json", t["name"]))
                .clone()
        })
        .collect()
}

fn diff(problems: &mut Vec<String>, at: String, got: &Value, want: &Value) {
    if let Some(d) = harness::first_difference(got, want, String::new()) {
        problems.push(format!("{at} {d}"));
    }
}

async fn check(world: &World, file: &std::path::Path) -> Vec<String> {
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(file).expect("golden")).expect("json");
    let id = golden["id"].as_str().unwrap_or_default().to_owned();
    let run = match harness::run_surface(world, &golden).await {
        Ok(run) => run,
        Err(e) => return vec![format!("{id}: {e}")],
    };
    let mut problems = Vec::new();
    let exchanges = golden["exchanges"].as_array().cloned().unwrap_or_default();
    if run.exchanges.len() != exchanges.len() {
        problems.push(format!(
            "{id}: {} exchanges replayed, golden has {}",
            run.exchanges.len(),
            exchanges.len()
        ));
    }
    let serves_resources = run.exchanges.iter().zip(&exchanges).any(|(g, w)| {
        w["request"]["method"] == "resources/list"
            && g["response"]["result"]["resources"]
                .as_array()
                .is_some_and(|r| !r.is_empty())
    });
    let golden_tools: Vec<String> = exchanges
        .iter()
        .filter_map(|e| e["response"]["result"]["tools"].as_array())
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    for (i, (got, want)) in run.exchanges.iter().zip(&exchanges).enumerate() {
        let method = want["request"]["method"].as_str().unwrap_or_default();
        let params = &want["request"]["params"];
        let at = format!("{id} exchange {i} ({method})");
        let mut want_response = want["response"].clone();
        // The surface capture recorded ServiceNow requests only.
        let servicenow = |v: &Value| -> Value {
            v.as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["target"] == "servicenow")
                .cloned()
                .collect()
        };
        let want_outbound = servicenow(&want["outbound"]);
        let got_outbound = servicenow(&got["outbound"]);
        let tool = params["name"].as_str().unwrap_or_default();
        if let Some(dev) = deviation(&id, i, method, params) {
            let at = format!("{at} deviation ({})", dev.reason);
            diff(
                &mut problems,
                at.clone(),
                &got["response"],
                &(dev.expected)(method),
            );
            diff(&mut problems, at, &got_outbound, &json!([]));
            continue;
        }
        match method {
            "initialize" => {
                want_response["result"]["serverInfo"]["version"] = json!(env!("CARGO_PKG_VERSION"));
                if let Some(caps) = want_response["result"]["capabilities"].as_object_mut() {
                    caps.remove("prompts");
                }
            }
            "resources/read" if params["uri"] == "servicenow://instance" => {
                if let Some(text) = want_response["result"]["contents"][0]["text"].as_str() {
                    let text = text.replacen(
                        "\"version\": \"6.2.0\"",
                        &format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION")),
                        1,
                    );
                    want_response["result"]["contents"][0]["text"] = json!(text);
                }
            }
            "tools/list" => {
                let registered: Vec<Value> = as_7_0(&want_response["result"]["tools"])
                    .into_iter()
                    .filter(|t| {
                        t["name"]
                            .as_str()
                            .is_some_and(|n| run.registered.iter().any(|r| r == n))
                    })
                    .collect();
                diff(
                    &mut problems,
                    format!("{at} tools"),
                    &got["response"]["result"]["tools"],
                    &world.scrub(&json!(registered)),
                );
                continue;
            }
            "tools/call"
                if golden_tools.iter().any(|t| t == tool)
                    && !run.registered.iter().any(|r| r == tool) =>
            {
                diff(
                    &mut problems,
                    format!("{at} lane-pending"),
                    &got["response"],
                    &json!({"error": {"code": -32602, "message": format!("Unknown tool: {tool}")}}),
                );
                continue;
            }
            "resources/list" if !serves_resources => {
                diff(
                    &mut problems,
                    format!("{at} lane-pending"),
                    &got["response"],
                    &json!({"result": {"resources": []}}),
                );
                continue;
            }
            "resources/read" if !serves_resources => {
                let uri = params["uri"].as_str().unwrap_or_default();
                diff(
                    &mut problems,
                    format!("{at} lane-pending"),
                    &got["response"],
                    &json!({"error": {"code": -32603, "message": format!("Unknown resource: {uri}")}}),
                );
                continue;
            }
            _ => {}
        }
        diff(
            &mut problems,
            format!("{at} response"),
            &got["response"],
            &want_response,
        );
        let notifications = if want["notifications"].is_null() {
            json!([])
        } else {
            want["notifications"].clone()
        };
        diff(
            &mut problems,
            format!("{at} notifications"),
            &got["notifications"],
            &notifications,
        );
        diff(
            &mut problems,
            format!("{at} outbound"),
            &got_outbound,
            &want_outbound,
        );
    }
    let wire = golden["wireExchanges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (i, (got, want)) in run.wire.iter().zip(&wire).enumerate() {
        let sent = want["sentRaw"].as_str().unwrap_or_default();
        let expected = if INVALID_REQUEST_WIRE.contains(&sent) {
            json!([{"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid request"}}])
        } else {
            want["received"].clone()
        };
        diff(
            &mut problems,
            format!("{id} wire {i} {sent:?}"),
            &json!(got),
            &expected,
        );
    }
    problems
}

#[tokio::test(flavor = "multi_thread")]
async fn surface_goldens_replay() {
    let world = World::start().await;
    let mut files: Vec<_> = std::fs::read_dir(harness::surface_dir())
        .expect("surface goldens")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(files.len() >= 9, "surface goldens missing: {files:?}");
    let mut problems = Vec::new();
    for f in &files {
        problems.extend(check(&world, f).await);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
