use clap::{Parser, Subcommand};
use rustymail_core::config::Config;
use rustymail_server::{LabServer, flush_logs, log_event, start_logging};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "rustymail: laboratory mail receiver (not a production release)"
)]
struct Args {
    #[arg(long, default_value = "deploy/rustymail.lab.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the complete configuration schema, without binding or writing.
    Check,
    /// Receive raw mail on the loopback SMTP listener only; no AUTH/TLS/IMAP.
    ServeLab,
    /// Loopback implicit TLS submission and local administration (Unix socket on Linux).
    ServeLabTls,
    /// Three loopback roles: SMTP receive, implicit TLS and STARTTLS submission.
    ServeLabSmtp,
    /// Reserved production entry point; always refuses in this release.
    Serve,
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = start_logging() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    let status = match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log_event(
                "startup_or_service_error",
                serde_json::json!({"error":error.to_string()}),
            );
            ExitCode::FAILURE
        }
    };
    flush_logs().await;
    status
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(args.config)?;
    match args.command {
        Command::Check => {
            println!("{}",serde_json::json!({"configuration":"valid","check":"structural_only",
                "production_ready":false,"version":env!("CARGO_PKG_VERSION")}));
        }
        Command::Serve => return Err("production serve is not implemented; use serve-lab with the explicit loopback lab configuration".into()),
        Command::ServeLab => {
            let server = LabServer::bind(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
        Command::ServeLabTls=>{
            let server=LabServer::bind_tls(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
        Command::ServeLabSmtp => {
            let server = LabServer::bind_smtp(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut termination) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => (),
                _ = termination.recv() => (),
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
