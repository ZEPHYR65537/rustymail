use clap::Parser;
#[tokio::main]
async fn main() -> std::process::ExitCode {
    rustymail_server::cli::daemon::entry(rustymail_server::cli::daemon::Args::parse()).await
}
