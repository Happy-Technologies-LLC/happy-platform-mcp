//! Repository automation. Sole consumer of `fixtures/` (D-07 §2.4, G-3).
//!
//! Every fixture path is hardcoded here. There is no argument, environment variable or
//! manifest setting that can point a comparison at another directory.
//!
//! `cargo xtask fixtures verify`:
//! 1. inventory: every file under `fixtures/` must be listed (golden `MANIFEST.json`
//!    entries, the two golden meta files, the compat manifest and the 6.2.0 tools
//!    list). Unlisted files, symlinks and `fixtures/deltas/` content (no delta schema
//!    yet) fail;
//! 2. integrity: every `MANIFEST.json` entry matches its recorded size and SHA-256;
//! 3. drift: runs `scripts/golden/verify.mjs` (re-capture from the Node 6.2.0 baseline
//!    against a loopback stub, byte-compare with `fixtures/golden`) from a fresh temp
//!    cwd with ServiceNow targeting variables removed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

const USAGE: &str = "usage: cargo xtask fixtures verify";

/// Files under `fixtures/` that are not golden captures listed in MANIFEST.json.
const FIXTURE_FILES_OUTSIDE_MANIFEST: &[&str] = &[
    "compat-manifest.json",
    "tools-list-6.2.0.json",
    "golden/MANIFEST.json",
    "golden/NORMALIZATION.json",
];

/// Variables that could aim the Node baseline at a real instance. The golden harness
/// also refuses them (`scripts/golden/lib/guard.mjs`); removing them here keeps a
/// developer shell from tripping or bypassing that guard.
const SCRUBBED_ENV: &[&str] = &[
    "SERVICENOW_INSTANCE",
    "SERVICENOW_INSTANCE_URL",
    "SERVICENOW_URL",
    "HAPPY_CONFIG_PATH",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["fixtures", "verify"] => match fixtures_verify() {
            Ok(code) => code,
            Err(e) => {
                eprintln!("fixtures verify: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))
}

fn fixtures_verify() -> Result<ExitCode, String> {
    let root = repo_root()?;
    let fixtures = root.join("fixtures");
    let golden = fixtures.join("golden");

    let manifest_text = fs::read_to_string(golden.join("MANIFEST.json"))
        .map_err(|e| format!("fixtures/golden/MANIFEST.json: {e}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .map_err(|e| format!("fixtures/golden/MANIFEST.json: {e}"))?;
    let entries = manifest["files"]
        .as_object()
        .ok_or("fixtures/golden/MANIFEST.json: `files` is not an object")?;

    let mut expected: BTreeSet<String> = FIXTURE_FILES_OUTSIDE_MANIFEST
        .iter()
        .map(|s| s.to_string())
        .collect();
    expected.extend(entries.keys().map(|k| format!("golden/{k}")));

    let mut problems = Vec::new();
    let mut present = BTreeSet::new();
    walk(&fixtures, "", &mut present, &mut problems)?;
    for rel in present.difference(&expected) {
        problems.push(format!("unlisted file: fixtures/{rel}"));
    }
    for rel in expected.difference(&present) {
        problems.push(format!("missing file: fixtures/{rel}"));
    }

    for (rel, entry) in entries {
        if !present.contains(&format!("golden/{rel}")) {
            continue;
        }
        let bytes =
            fs::read(golden.join(rel)).map_err(|e| format!("fixtures/golden/{rel}: {e}"))?;
        let want_len = entry["bytes"].as_u64();
        let want_sha = entry["sha256"].as_str();
        let got_sha = format!("{:x}", Sha256::digest(&bytes));
        if want_len != Some(bytes.len() as u64) || want_sha != Some(got_sha.as_str()) {
            problems.push(format!("hash/size mismatch: fixtures/golden/{rel}"));
        }
    }

    if !problems.is_empty() {
        eprintln!("fixtures verify: {} problem(s):", problems.len());
        for p in &problems {
            eprintln!("  {p}");
        }
        return Ok(ExitCode::FAILURE);
    }
    eprintln!(
        "fixtures verify: inventory and hashes OK ({} manifest entries, {} files)",
        entries.len(),
        present.len()
    );

    run_golden_drift_check(&root)
}

/// Walks `dir` without following symlinks. Records every regular file as a path
/// relative to `fixtures/` with `/` separators.
fn walk(
    dir: &Path,
    rel: &str,
    files: &mut BTreeSet<String>,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 name under {}", dir.display()))?;
        let child = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("fixtures/{child}: {e}"))?;
        if kind.is_symlink() {
            problems.push(format!("symlink: fixtures/{child}"));
        } else if kind.is_dir() {
            walk(&entry.path(), &child, files, problems)?;
        } else if kind.is_file() {
            files.insert(child);
        } else {
            problems.push(format!("not a regular file: fixtures/{child}"));
        }
    }
    Ok(())
}

/// Runs the Node golden verifier with its built-in default golden directory
/// (`fixtures/golden`); no path is passed.
fn run_golden_drift_check(root: &Path) -> Result<ExitCode, String> {
    let script = root.join("scripts/golden/verify.mjs");
    let cwd = std::env::temp_dir().join(format!("hpm-xtask-fixtures-{}", std::process::id()));
    fs::create_dir_all(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    let mut cmd = Command::new("node");
    cmd.arg(&script).current_dir(&cwd);
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    let status = cmd.status();
    let _ = fs::remove_dir_all(&cwd);
    let status = status.map_err(|e| format!("node {}: {e}", script.display()))?;
    match status.code() {
        Some(0) => Ok(ExitCode::SUCCESS),
        Some(code) => {
            eprintln!("fixtures verify: golden drift check exited {code}");
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
        None => Err("golden drift check terminated by a signal".into()),
    }
}
