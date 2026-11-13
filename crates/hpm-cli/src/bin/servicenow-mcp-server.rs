//! 6.x binary name, kept as an alias of `happy-platform-mcp`.

fn main() -> std::process::ExitCode {
    hpm_cli::http::load_dotenv();
    hpm_cli::run()
}
