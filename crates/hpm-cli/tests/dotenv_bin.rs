//! CF-1 (owner decision P4-3): the binaries load a restricted `.env` from the
//! working directory. Runs the real stdio and HTTP binaries in a temp directory.
// Process plumbing outside `#[test]` fns: failures should abort loudly.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SN_PASSWORD: &str = "pw-d0tenv-s3cr3t-77aa";
const TIMEOUT: Duration = Duration::from_secs(20);

fn command(exe: &str, dir: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.env_clear();
    for keep in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.current_dir(dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("HAPPY_CONFIG_PATH", dir.join("instances.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// stdio: the `.env` warnings go to stderr; stdout (the protocol) stays empty,
/// and no warning quotes a line (the malformed one holds a secret).
#[test]
fn stdio_server_warns_about_dotenv_on_stderr_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        format!(
            "SERVICENOW_INSTANCE_URL=https://dev.example.invalid\n\
             SERVICENOW_USERNAME=svc\n\
             SERVICENOW_PASSWORD={SN_PASSWORD}\n\
             not a pair {SN_PASSWORD}\n"
        ),
    )
    .unwrap();
    let child = command(env!("CARGO_BIN_EXE_happy-platform-mcp"), dir.path())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()));
    let output = rx
        .recv_timeout(TIMEOUT)
        .expect("stdio server exits at EOF")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout, "", "stderr: {stderr}");
    assert!(
        stderr.contains("SERVICENOW_INSTANCE_URL was loaded from .env"),
        "{stderr}"
    );
    assert!(stderr.contains(".env line 4: ignored"), "{stderr}");
    assert!(!stderr.contains(SN_PASSWORD), "{stderr}");
}

/// HTTP: the bearer token comes from `.env` (P4-3 keeps it), while a `.env`
/// bind host is ignored: the server still listens on 127.0.0.1.
#[test]
fn http_server_takes_the_token_but_not_the_bind_host_from_dotenv() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        format!(
            "HAPPY_MCP_API_TOKEN={TOKEN}\n\
             HAPPY_MCP_BIND_HOST=0.0.0.0\n\
             HAPPY_MCP_ALLOWED_ORIGINS=https://evil.example\n"
        ),
    )
    .unwrap();
    let mut child = command(env!("CARGO_BIN_EXE_happy-platform-mcp-http"), dir.path())
        .env("PORT", "0")
        .env("SERVICENOW_INSTANCE_URL", "https://dev.example.invalid")
        .env("SERVICENOW_USERNAME", "svc")
        .env("SERVICENOW_PASSWORD", SN_PASSWORD)
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let listening = loop {
        match rx.recv_timeout(TIMEOUT) {
            Ok(line) if line.starts_with("Happy MCP Server listening on ") => break Some(line),
            Ok(_) => continue,
            Err(_) => break None,
        }
    };
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let listening = listening.unwrap_or_else(|| {
        panic!(
            "no listening line; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        listening.starts_with("Happy MCP Server listening on http://127.0.0.1:"),
        "{listening}"
    );
}

/// A-6: like 6.2.0, `instance …` and help never read `.env`, so they print
/// none of its warnings.
#[test]
fn instance_cli_and_help_do_not_load_dotenv() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        "SERVICENOW_INSTANCE_URL=https://dev.example.invalid\nnot a pair\n",
    )
    .unwrap();
    for exe in [
        env!("CARGO_BIN_EXE_happy-platform-mcp"),
        env!("CARGO_BIN_EXE_servicenow-mcp-server"),
    ] {
        for args in [&["--help"][..], &["instance", "--help"][..]] {
            let output = command(exe, dir.path()).args(args).output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains(".env"), "{exe} {args:?}: {stderr}");
        }
    }
}
