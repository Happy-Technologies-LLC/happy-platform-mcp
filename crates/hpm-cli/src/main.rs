fn main() -> std::process::ExitCode {
    hpm_cli::http::load_dotenv();
    hpm_cli::run()
}
