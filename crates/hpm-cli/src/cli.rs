//! Top-level argument dispatch (port of `src/cli.js`).

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use hpm_core::config::Env;
use hpm_core::keychain::{LazySecretStore, SecretStore};

use crate::instance_cli::{
    BIND_USAGE_LINE, CliContext, SECRET_ARGUMENT_ERROR, USAGE, contains_secret_argument,
    run_instance_cli,
};

/// What `dispatch` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Run the stdio MCP server.
    Serve,
    /// `instance …` subcommand.
    Instance,
    /// Finished with this exit code.
    Exit(i32),
}

/// Pre-instance dispatch: secrets check, serve, help, unknown.
pub fn route(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> Dispatch {
    if contains_secret_argument(args) {
        let _ = writeln!(stderr, "{SECRET_ARGUMENT_ERROR}");
        return Dispatch::Exit(2);
    }
    // `src/cli.js:34`: `--docs-only` only routes to the server; the mode itself is
    // decided from env and config (`crate::stdio_docs_only`, `src/stdio-server.js:47-100`).
    if args.is_empty() || (args.len() == 1 && args[0] == "--docs-only") {
        return Dispatch::Serve;
    }
    if args[0] == "instance" {
        return Dispatch::Instance;
    }
    if matches!(args[0].as_str(), "--help" | "-h" | "help") {
        let _ = writeln!(stdout, "{USAGE}\n{BIND_USAGE_LINE}");
        return Dispatch::Exit(0);
    }
    let _ = writeln!(stderr, "Unknown command. Use --help for usage.");
    Dispatch::Exit(2)
}

/// `dispatch(argv, dependencies)` with injected context for `instance`.
pub async fn dispatch_instance(args: &[String], ctx: &mut CliContext<'_>) -> i32 {
    run_instance_cli(args, ctx).await
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Process entry: routes, then serves or runs the instance CLI.
pub fn main(args: &[String]) -> ExitCode {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    match route(args, &mut stdout, &mut stderr) {
        Dispatch::Serve => {
            // A-6: like 6.2.0, only the servers read `.env`, not `instance …`/help.
            crate::http::load_dotenv();
            crate::serve_stdio()
        }
        Dispatch::Exit(code) => exit(code),
        Dispatch::Instance => {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = writeln!(stderr, "{e}");
                    return ExitCode::FAILURE;
                }
            };
            // Opened on first credential operation, never for help/list/usage errors.
            let secrets: Arc<dyn SecretStore> = Arc::new(LazySecretStore::os());
            let env = Env::process();
            let mut ctx = CliContext::from_env(&env, secrets, &mut stdout, &mut stderr);
            exit(runtime.block_on(dispatch_instance(args, &mut ctx)))
        }
    }
}
