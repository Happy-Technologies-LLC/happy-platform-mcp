//! Replays `fixtures/golden/tools/**` (captured from 6.2.0 by
//! `scripts/golden/stages/tools.mjs`) through the real single-user composition and
//! MCP stdio handler, against Rust ports of the loopback ServiceNow and raw-GitHub
//! stubs (see `support/harness.rs`; harness self-tests live in `golden_harness.rs`).
//!
//! Custom runner (`harness = false`): every golden is one case named
//! `<group>::<tool>__<case>`, e.g. `crud::sn_query_table__basic`. The group is the
//! fixture folder (`-` → `_`), except `SN-Natural-Language-Search` goldens, which
//! form group `nl` wherever they live. Filters work like libtest substring filters
//! and also match the fixture path, so `-- crud:: nl::`, `-- sn_query_table` and
//! `-- instances/SN-Query-Table` all select real work. A filter that selects no
//! golden fails.
//!
//! A golden whose tools are in [`NOT_PORTED`] must fail with `not ported:`; every
//! other golden must replay clean. A lane enables its tools by registering them in
//! its `families::<lane>::register` and deleting their names here: forgetting
//! either half turns the run red.

#![allow(
    clippy::print_stdout,
    reason = "custom test runner: reports to stdout like libtest"
)]

#[allow(dead_code, reason = "support is shared with golden_harness.rs")]
mod support;

use std::process::ExitCode;
use std::sync::Arc;

use support::harness::{self, World};

/// Tools no family registers yet. Lanes delete names as they port them.
const NOT_PORTED: &[&str] = &[
    "SN-Clone-Update-Set",
    "SN-Create-Activity",
    "SN-Create-Fix-Script",
    "SN-Create-Transition",
    "SN-Create-Workflow",
    "SN-Execute-Background-Script",
    "SN-Get-Current-Update-Set",
    "SN-List-Update-Sets",
    "SN-Move-Records-To-Update-Set",
    "SN-Publish-Workflow",
    "SN-Set-Current-Application",
    "SN-Set-Update-Set",
];

/// Cases run at once (each has its own stubs and sandbox).
const PARALLEL: usize = 8;

struct Case {
    /// `<group>::<tool>__<case>`.
    name: String,
    /// `<family>/<file>.json`.
    golden: String,
    tools: Vec<String>,
}

fn snake(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn cases() -> Vec<Case> {
    harness::all_goldens()
        .into_iter()
        .map(|golden| {
            let (family, file) = golden.split_once('/').expect("<family>/<file>");
            let stem = file.trim_end_matches(".json");
            let (tool, case) = stem.split_once("__").expect("<tool>__<case>.json");
            let text =
                std::fs::read_to_string(harness::tools_dir().join(&golden)).expect("golden file");
            let json: serde_json::Value = serde_json::from_str(&text).expect("golden json");
            let mut tools: Vec<String> = json["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["request"]["method"] == "tools/call")
                .filter_map(|s| s["request"]["params"]["name"].as_str().map(str::to_owned))
                .collect();
            tools.dedup();
            let group = if tool == "SN-Natural-Language-Search" {
                "nl".to_owned()
            } else {
                snake(family)
            };
            Case {
                name: format!("{group}::{}__{}", snake(tool), snake(case)),
                golden,
                tools,
            }
        })
        .collect()
}

/// `Ok`: the outcome matches the golden's port status.
async fn run(case: &Case) -> Result<(), String> {
    let world = World::start().await;
    let unported: Vec<&str> = case
        .tools
        .iter()
        .map(String::as_str)
        .filter(|t| NOT_PORTED.contains(t))
        .collect();
    match harness::replay(&world, &case.golden).await {
        Ok(()) if unported.is_empty() => Ok(()),
        Ok(()) => Err(format!(
            "{}: replays clean but {} still listed in NOT_PORTED",
            case.golden,
            unported.join(", ")
        )),
        Err(e) if !unported.is_empty() && e.contains(": not ported: ") => Ok(()),
        Err(e) if !unported.is_empty() => Err(format!(
            "{e}\n(listed in NOT_PORTED: {}; a registered tool must leave the list)",
            unported.join(", ")
        )),
        Err(e) => Err(e),
    }
}

struct Args {
    filters: Vec<String>,
    exact: bool,
    list: bool,
}

/// libtest-compatible enough for `cargo test`: filters, `--exact`, `--list`; other
/// flags (and the value of `--test-threads`, `--format`, `--logfile`, ...) are
/// accepted and ignored.
fn parse_args() -> Args {
    let mut args = Args {
        filters: Vec::new(),
        exact: false,
        list: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--exact" => args.exact = true,
            "--list" => args.list = true,
            "--test-threads" | "--format" | "--logfile" | "--skip" | "--color" | "-Z" => {
                it.next();
            }
            f if f.starts_with('-') => {}
            f => args.filters.push(f.to_owned()),
        }
    }
    args
}

fn selected(case: &Case, args: &Args) -> bool {
    args.filters.is_empty()
        || args.filters.iter().any(|f| {
            if args.exact {
                case.name == *f
            } else {
                case.name.contains(f.as_str()) || case.golden.contains(f.as_str())
            }
        })
}

fn main() -> ExitCode {
    let args = parse_args();
    let all = cases();
    let unknown: Vec<&&str> = NOT_PORTED
        .iter()
        .filter(|t| !all.iter().any(|c| c.tools.iter().any(|x| x == **t)))
        .collect();
    let chosen: Vec<Case> = all.into_iter().filter(|c| selected(c, &args)).collect();
    if args.list {
        for c in &chosen {
            println!("{}: test", c.name);
        }
        return ExitCode::SUCCESS;
    }
    if !unknown.is_empty() {
        eprintln!("NOT_PORTED names no golden tool: {unknown:?}");
        return ExitCode::FAILURE;
    }
    if chosen.is_empty() {
        eprintln!("golden_tools: filter {:?} selects no golden", args.filters);
        return ExitCode::FAILURE;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let chosen = Arc::new(chosen);
    println!("\nrunning {} goldens", chosen.len());
    let results: Vec<Result<(), String>> = runtime.block_on(async {
        let mut results = Vec::with_capacity(chosen.len());
        for start in (0..chosen.len()).step_by(PARALLEL) {
            let batch: Vec<_> = (start..(start + PARALLEL).min(chosen.len()))
                .map(|i| {
                    let chosen = chosen.clone();
                    tokio::spawn(async move { run(&chosen[i]).await })
                })
                .collect();
            for task in batch {
                results.push(task.await.unwrap_or_else(|e| Err(format!("panicked: {e}"))));
            }
        }
        results
    });
    let mut failures = Vec::new();
    for (case, result) in chosen.iter().zip(results) {
        match result {
            Ok(()) => println!("test {} ... ok", case.name),
            Err(e) => {
                println!("test {} ... FAILED", case.name);
                failures.push(format!("---- {} ----\n{e}", case.name));
            }
        }
    }
    if failures.is_empty() {
        println!("\ntest result: ok. {} passed; 0 failed\n", chosen.len());
        ExitCode::SUCCESS
    } else {
        println!("\nfailures:\n\n{}\n", failures.join("\n\n"));
        println!(
            "test result: FAILED. {} passed; {} failed\n",
            chosen.len() - failures.len(),
            failures.len()
        );
        ExitCode::FAILURE
    }
}
