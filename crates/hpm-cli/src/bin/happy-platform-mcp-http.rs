//! HTTP server binary: the Rust counterpart of `node src/server.js` (`npm start`).

fn main() -> std::process::ExitCode {
    hpm_cli::http::load_dotenv();
    hpm_cli::http::main()
}
