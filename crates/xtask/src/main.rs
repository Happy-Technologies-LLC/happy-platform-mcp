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
//!
//! `cargo xtask check-trusted-root [--offline]` (review H1): feature unification lets
//! any crate in a build with hpm-cli see `hpm-types/trusted-root`, so the gate checks
//! each crate alone.
//! 1. `cargo check -p <crate> --lib --locked` (bins for bin-only crates) for every
//!    workspace crate except hpm-cli, one invocation each, no extra features;
//! 2. a grep over `Cargo.toml` and `src/` (not `tests/`; dev-dependencies are not
//!    unified into the lib or release builds) of every crate except hpm-cli, the
//!    defining hpm-types and xtask, rejecting the trusted-root constructors and the
//!    feature name. Comment lines are ignored.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

const USAGE: &str =
    "usage: cargo xtask fixtures verify | cargo xtask check-trusted-root [--offline]";

/// The composition root: the only crate allowed to use `trusted-root`.
const TRUSTED_ROOT_CRATE: &str = "hpm-cli";
/// Skipped by the grep only: hpm-types defines the items, xtask names the patterns.
const GREP_EXEMPT: &[&str] = &["hpm-cli", "hpm-types", "xtask"];
/// Rejected in non-root `src/` and `Cargo.toml`.
const TRUSTED_ROOT_PATTERNS: &[&str] = &[
    "mint(",
    "single_user_stdio(",
    "streamable_http(",
    "legacy_sse(",
    "trusted-root",
];

/// Files under `fixtures/` that are not golden captures listed in MANIFEST.json.
const FIXTURE_FILES_OUTSIDE_MANIFEST: &[&str] = &[
    "compat-manifest.json",
    "tools-list-6.2.0.json",
    "tools-list-7.0.json",
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
        ["check-trusted-root", rest @ ..] if rest.iter().all(|a| *a == "--offline") => {
            match check_trusted_root(!rest.is_empty()) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("check-trusted-root: {e}");
                    ExitCode::FAILURE
                }
            }
        }
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

struct Member {
    name: String,
    dir: PathBuf,
    has_lib: bool,
}

fn workspace_members(root: &Path, offline: bool) -> Result<Vec<Member>, String> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .current_dir(root);
    if offline {
        cmd.arg("--offline");
    }
    let out = cmd.output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    let packages = meta["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?;
    let mut members = Vec::new();
    for p in packages {
        let name = p["name"].as_str().ok_or("package without name")?.to_owned();
        let manifest = p["manifest_path"]
            .as_str()
            .ok_or("package without manifest_path")?;
        let dir = Path::new(manifest)
            .parent()
            .ok_or("manifest without parent")?
            .to_path_buf();
        let has_lib = p["targets"].as_array().into_iter().flatten().any(|t| {
            t["kind"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k.as_str().is_some_and(|k| k == "lib" || k.ends_with("lib")))
        });
        members.push(Member { name, dir, has_lib });
    }
    Ok(members)
}

fn check_trusted_root(offline: bool) -> Result<ExitCode, String> {
    let root = repo_root()?;
    let members = workspace_members(&root, offline)?;
    let mut problems = Vec::new();

    for m in members.iter().filter(|m| m.name != TRUSTED_ROOT_CRATE) {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["check", "--locked", "-p", &m.name])
            .arg(if m.has_lib { "--lib" } else { "--bins" })
            .current_dir(&root);
        if offline {
            cmd.arg("--offline");
        }
        eprintln!("check-trusted-root: cargo check -p {} alone", m.name);
        let status = cmd.status().map_err(|e| format!("cargo check: {e}"))?;
        if !status.success() {
            problems.push(format!("cargo check -p {} failed in isolation", m.name));
        }
    }

    for m in members
        .iter()
        .filter(|m| !GREP_EXEMPT.contains(&m.name.as_str()))
    {
        grep_file(&m.dir.join("Cargo.toml"), "#", &mut problems)?;
        let src = m.dir.join("src");
        if src.is_dir() {
            grep_rust_tree(&src, &mut problems)?;
        }
    }

    if problems.is_empty() {
        eprintln!("check-trusted-root: OK");
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("check-trusted-root: {} problem(s):", problems.len());
    for p in &problems {
        eprintln!("  {p}");
    }
    Ok(ExitCode::FAILURE)
}

fn grep_rust_tree(dir: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            grep_rust_tree(&path, problems)?;
        } else if path.extension().is_some_and(|x| x == "rs") {
            grep_file(&path, "//", problems)?;
        }
    }
    Ok(())
}

/// Flags pattern hits outside comments (`comment` starts a line comment).
fn grep_file(path: &Path, comment: &str, problems: &mut Vec<String>) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for (n, line) in text.lines().enumerate() {
        let code = line.split(comment).next().unwrap_or("");
        for pat in TRUSTED_ROOT_PATTERNS {
            if code.contains(pat) {
                problems.push(format!("{}:{}: uses `{pat}`", path.display(), n + 1));
            }
        }
    }
    Ok(())
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
        let got_sha: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if want_len != Some(bytes.len() as u64) || want_sha != Some(got_sha.as_str()) {
            problems.push(format!("hash/size mismatch: fixtures/golden/{rel}"));
        }
    }

    // Tool-list fixtures are pinned by `fixtures/compat-manifest.json`.
    let compat_text = fs::read_to_string(fixtures.join("compat-manifest.json"))
        .map_err(|e| format!("fixtures/compat-manifest.json: {e}"))?;
    let compat: serde_json::Value = serde_json::from_str(&compat_text)
        .map_err(|e| format!("fixtures/compat-manifest.json: {e}"))?;
    for (rel, key) in [
        ("tools-list-6.2.0.json", "tools"),
        ("tools-list-7.0.json", "tools70"),
    ] {
        let pin = &compat[key]["fixture"];
        let Ok(bytes) = fs::read(fixtures.join(rel)) else {
            continue; // reported as missing above
        };
        let got_sha: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if pin["path"].as_str() != Some(format!("fixtures/{rel}").as_str())
            || pin["sha256"].as_str() != Some(got_sha.as_str())
        {
            problems.push(format!(
                "compat-manifest {key}.fixture does not pin fixtures/{rel}"
            ));
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
