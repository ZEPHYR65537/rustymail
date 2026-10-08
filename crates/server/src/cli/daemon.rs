use crate::{LabServer, flush_logs, log_event, start_logging};
use clap::ValueEnum;
use rustymail_core::config::Config;
use std::{path::PathBuf, process::ExitCode};

#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    Production,
    Lab,
    LabTls,
    LabSmtp,
    LabRelay,
}

pub async fn entry(config: PathBuf, mode: Option<Mode>) -> ExitCode {
    if let Err(error) = start_logging() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    let status = match run(config, mode).await {
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

async fn run(path: PathBuf, mode: Option<Mode>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?;
    match mode {
        None => {
            println!("{}",serde_json::json!({"configuration":"valid","check":"structural_only",
                "production_ready":false,"version":env!("CARGO_PKG_VERSION")}));
        }
        Some(Mode::Production) => return Err("production serve is not implemented; use serve --mode lab with an explicit loopback lab configuration".into()),
        Some(Mode::Lab) => {
            let server = LabServer::bind(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
        Some(Mode::LabTls)=>{
            let server=LabServer::bind_tls(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
        Some(Mode::LabSmtp) => {
            let server = LabServer::bind_smtp(config).await?;
            server.serve_until(shutdown_signal()).await?;
        }
        Some(Mode::LabRelay) => {
            LabServer::bind_relay(config).await?.serve_until(shutdown_signal()).await?;
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
