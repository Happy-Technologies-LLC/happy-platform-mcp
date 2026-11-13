use std::io::{BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("happy-platform-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // Proves the npm shim passes argv through and reports the right platform binary.
        Some("--s3-info") => {
            let mut info = hpm_s3::build_info();
            info["argv"] = serde_json::json!(args);
            println!("{info}");
            ExitCode::SUCCESS
        }
        // Proves the shim forwards stdin/stdout unbuffered (MCP stdio transport shape).
        Some("--s3-echo") => {
            let stdin = std::io::stdin();
            let mut line = String::new();
            if stdin.lock().read_line(&mut line).is_err() {
                return ExitCode::from(1);
            }
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "echo:{line}");
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        // Proves exit codes propagate through the shim (CLI parity item).
        Some("--s3-exit") => {
            let code = args.get(1).and_then(|c| c.parse::<u8>().ok()).unwrap_or(0);
            ExitCode::from(code)
        }
        _ => {
            eprintln!("happy-platform-mcp (S3 packaging spike): try --version | --s3-info | --s3-echo | --s3-exit N");
            ExitCode::from(2)
        }
    }
}
