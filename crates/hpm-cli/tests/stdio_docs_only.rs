//! Runs the real stdio binary with an isolated HOME and no credentials and checks
//! the docs-only trigger (`src/stdio-server.js:47-100`) against the 6.2.0 goldens.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn golden_tool_names(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden/surface")
        .join(name);
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let list = golden["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["request"]["method"] == "tools/list")
        .unwrap();
    tool_names(&list["response"]["result"])
}

fn tool_names(result: &Value) -> Vec<String> {
    result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

struct Run {
    /// The `tools/list` result, or why there is none.
    result: Result<Value, String>,
    stderr: String,
    code: Option<i32>,
}

/// Spawns the binary, sends initialize + tools/list, closes stdin and waits.
/// Every stdout line must be JSON-RPC (stderr-only diagnostics).
fn run(args: &[&str], vars: &[(&str, &str)]) -> Run {
    run_in(tempfile::tempdir().unwrap(), args, vars)
}

fn run_in(home: tempfile::TempDir, args: &[&str], vars: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_happy-platform-mcp"));
    cmd.env_clear();
    for keep in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("HAPPY_CONFIG_PATH", home.path().join("instances.json"))
        .current_dir(home.path())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let msgs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"t","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ];
    for m in &msgs {
        // The process may already have exited (startup error): ignore broken pipes.
        let _ = writeln!(stdin, "{m}");
    }
    let mut result = Err("no tools/list response".to_string());
    let mut line = String::new();
    while stdout.read_line(&mut line).unwrap_or(0) > 0 {
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        line.clear();
        if v["id"] == 2 {
            result = Ok(v["result"].clone());
            break;
        }
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    for rest in String::from_utf8_lossy(&out.stdout).lines() {
        serde_json::from_str::<Value>(rest).unwrap();
    }
    Run {
        result,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

/// `lines` appear in `stderr` in this order (other lines may sit between them).
fn assert_in_order(stderr: &str, lines: &[&str]) {
    let all: Vec<&str> = stderr.lines().collect();
    let mut at = 0;
    for want in lines {
        let found = all[at..].iter().position(|l| l == want);
        let found = found.unwrap_or_else(|| panic!("missing {want:?} in order:\n{stderr}"));
        at += found + 1;
    }
}

const DOCS_ONLY_STARTUP: &[&str] = &[
    "📚 Starting Happy MCP in docs-only mode",
    "Happy MCP Server (stdio) started successfully",
    "Mode: docs-only",
    "Release notes and security advisories: https://happy-tech.biz/mcp",
];

#[test]
fn docs_only_env_lists_golden_docs_only_tools() {
    for value in ["true", "TRUE", "1", "Yes"] {
        let r = run(&[], &[("HAPPY_MCP_DOCS_ONLY", value)]);
        let result = r
            .result
            .unwrap_or_else(|e| panic!("{value}: {e}\n{}", r.stderr));
        assert_eq!(
            tool_names(&result),
            golden_tool_names("docs-only.json"),
            "{value}"
        );
        assert_in_order(&r.stderr, DOCS_ONLY_STARTUP);
        assert_eq!(r.code, Some(0), "{value}");
    }
}

#[test]
fn docs_only_flag_lists_golden_docs_only_flag_tools() {
    let r = run(&["--docs-only"], &[]);
    let result = r.result.unwrap_or_else(|e| panic!("{e}\n{}", r.stderr));
    assert_eq!(
        tool_names(&result),
        golden_tool_names("docs-only-flag.json")
    );
    assert_in_order(&r.stderr, DOCS_ONLY_STARTUP);
}

#[test]
fn no_config_falls_back_to_docs_only() {
    let r = run(&[], &[]);
    let result = r.result.unwrap_or_else(|e| panic!("{e}\n{}", r.stderr));
    assert_eq!(tool_names(&result), golden_tool_names("docs-only.json"));
    assert_in_order(&r.stderr, DOCS_ONLY_STARTUP);
}

/// `src/stdio-server.js:121-141` exits 1 when the fallback does not apply.
fn assert_refused(r: &Run, needle: &str) {
    assert!(r.result.is_err(), "served {:?}", r.result);
    assert_eq!(r.code, Some(1), "{}", r.stderr);
    assert!(
        r.stderr.contains("Failed to start Happy MCP Server: "),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains(needle), "{}", r.stderr);
    assert!(!r.stderr.contains("started successfully"), "{}", r.stderr);
}

#[test]
fn explicit_false_disables_fallback() {
    for value in ["false", "FALSE"] {
        assert_refused(
            &run(&[], &[("HAPPY_MCP_DOCS_ONLY", value)]),
            "Missing ServiceNow credentials",
        );
    }
}

#[test]
fn env_credentials_block_fallback() {
    let r = run(
        &[],
        &[
            ("SERVICENOW_INSTANCE_URL", "https://dev.example.invalid"),
            ("SERVICENOW_OAUTH_GRANT_TYPE", "client_credentials"),
        ],
    );
    assert_refused(&r, "Missing ServiceNow credentials");
}

/// A registry file with zero instances is `REGISTRY_EMPTY`, not a fallback error:
/// 6.2.0 refuses to start too.
#[test]
fn empty_registry_file_exits_1() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("instances.json"),
        r#"{"version":1,"instances":[]}"#,
    )
    .unwrap();
    assert_refused(
        &run_in(home, &[], &[]),
        "Instance registry contains no instances",
    );
}

#[test]
fn full_mode_prints_instance_startup_lines() {
    let r = run(
        &[],
        &[
            ("SERVICENOW_INSTANCE_URL", "https://dev.example.invalid"),
            ("SERVICENOW_USERNAME", "u"),
            ("SERVICENOW_PASSWORD", "p"),
        ],
    );
    let result = r.result.unwrap_or_else(|e| panic!("{e}\n{}", r.stderr));
    assert_eq!(tool_names(&result).len(), 60);
    assert_in_order(
        &r.stderr,
        &[
            "🔗 Default ServiceNow instance: default (https://dev.example.invalid)",
            "💡 Use SN-Set-Instance tool to switch instances during session",
            "Happy MCP Server (stdio) started successfully",
            "Instance: default - https://dev.example.invalid",
            "Release notes and security advisories: https://happy-tech.biz/mcp",
        ],
    );
    assert!(!r.stderr.contains("docs-only"), "{}", r.stderr);
}
