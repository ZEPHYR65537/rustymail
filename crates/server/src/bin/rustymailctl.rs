use clap::Parser;
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    rustymail_server::cli::control::entry(rustymail_server::cli::control::Args::parse()).await
}
