use clap::{Parser, Subcommand};
use rustymail_server::cli::daemon::Mode;
use rustymail_server::cli::{control, daemon, send};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    name = "rustymail",
    version,
    about = "SMTP client, laboratory mail server and local administration"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Submit existing mail over verified TLS; no automatic retries.
    Send(send::SendArgs),
    /// Run a server; production mode remains unavailable in this release.
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, value_enum, default_value = "production")]
        mode: Mode,
    },
    /// Validate server configuration without starting listeners.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Existing local/offline or Unix-socket administration.
    Admin(control::Args),
}
fn main() -> ExitCode {
    let args = Args::parse();
    // Parse first; a send/admin invocation never creates server workers.
    let mut builder = if matches!(&args.command, Command::Serve { .. }) {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let runtime = match builder.enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("runtime initialization failed");
            return ExitCode::from(2);
        }
    };
    runtime.block_on(async {
        match args.command {
            Command::Send(args) => send::entry(args).await,
            Command::Admin(args) => control::entry(args).await,
            Command::Check { config } => daemon::entry(config, None).await,
            Command::Serve { config, mode } => daemon::entry(config, Some(mode)).await,
        }
    })
}
