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

/// Spawns the binary, sends initialize + tools/list, returns (result or error, stderr).
fn run(args: &[&str], vars: &[(&str, &str)]) -> (Result<Value, String>, String) {
    let home = tempfile::tempdir().unwrap();
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
    (result, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn docs_only_env_lists_golden_docs_only_tools() {
    for value in ["true", "TRUE", "1", "Yes"] {
        let (result, stderr) = run(&[], &[("HAPPY_MCP_DOCS_ONLY", value)]);
        let result = result.unwrap_or_else(|e| panic!("{value}: {e}\n{stderr}"));
        assert_eq!(
            tool_names(&result),
            golden_tool_names("docs-only.json"),
            "{value}"
        );
        assert!(stderr.contains("📚 Starting Happy MCP in docs-only mode"));
    }
}

#[test]
fn docs_only_flag_lists_golden_docs_only_flag_tools() {
    let (result, stderr) = run(&["--docs-only"], &[]);
    let result = result.unwrap_or_else(|e| panic!("{e}\n{stderr}"));
    assert_eq!(
        tool_names(&result),
        golden_tool_names("docs-only-flag.json")
    );
}

#[test]
fn empty_registry_falls_back_to_docs_only() {
    let (result, stderr) = run(&[], &[]);
    let result = result.unwrap_or_else(|e| panic!("{e}\n{stderr}"));
    assert_eq!(tool_names(&result), golden_tool_names("docs-only.json"));
    assert!(stderr.contains("📚 Starting Happy MCP in docs-only mode"));
}

#[test]
fn explicit_false_disables_fallback() {
    for value in ["false", "FALSE"] {
        let (result, stderr) = run(&[], &[("HAPPY_MCP_DOCS_ONLY", value)]);
        assert!(result.is_err(), "{value}: served {result:?}");
        assert!(
            stderr.contains("Missing ServiceNow credentials"),
            "{value}: {stderr}"
        );
    }
}
