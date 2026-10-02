use clap::Args;
use rustymail_client::{
    config::ClientConfig,
    message,
    smtp::{AttemptResult, Message, SmtpClient},
};
use rustymail_core::Address;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Args)]
pub struct SendArgs {
    /// Client-only TOML. Relative CA/password paths use its directory.
    #[arg(long)]
    config: PathBuf,
    /// Override the configured SMTP envelope sender; does not rewrite header From.
    #[arg(long)]
    from: Option<String>,
    /// Envelope recipient; repeat for multiple recipients (maximum 100).
    #[arg(long, required = true)]
    to: Vec<String>,
    /// Raw CRLF mail. Omit or use '-' for stdin; EOF is required before connecting.
    #[arg(long)]
    file: Option<PathBuf>,
}

pub async fn entry(args: SendArgs) -> ExitCode {
    match run(args).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"error":error.to_string(),"retry_automatically":false})
            );
            ExitCode::from(2)
        }
    }
}
fn emit(recipient: &Address, status: &str, code: Option<u16>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{}",
        serde_json::json!({"recipient":recipient.as_str(),"status":status,"smtp_code":code})
    )?;
    out.flush()
}
async fn run(args: SendArgs) -> Result<u8, Box<dyn std::error::Error>> {
    if args.to.is_empty() || args.to.len() > 100 {
        return Err("send requires 1..100 recipients".into());
    }
    let config = ClientConfig::load(&args.config)?;
    let sender = Address::parse(args.from.as_deref().unwrap_or(&config.sender))?;
    let mut seen = BTreeSet::new();
    let mut recipients = Vec::new();
    for raw in args.to {
        let address = Address::parse(&raw)?;
        if seen.insert(address.as_str().to_owned()) {
            recipients.push(address);
        }
    }
    let limits = config.limits.clone();
    let snapshot = tokio::task::spawn_blocking(move || match args.file {
        Some(path) if path.as_os_str() != "-" => {
            if !std::fs::metadata(&path)?.is_file() {
                return Err(io::Error::other("input must be a regular file"));
            }
            message::prepare(File::open(path)?, &limits)
        }
        _ => message::prepare(io::stdin().lock(), &limits),
    })
    .await??;
    let client = SmtpClient::load(&config.smtp).await?;
    let mut exit = 0;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut cancelled = false;
    for recipient in recipients {
        if cancelled {
            emit(&recipient, "not_attempted", None)?;
            continue;
        }
        let reader = snapshot.reader()?;
        let m = Message {
            sender: Some(sender.clone()),
            recipient: recipient.clone(),
            body: snapshot.body,
            stored_size: snapshot.size,
            omit_prefix: String::new(),
        };
        let body = Arc::new(AtomicBool::new(false));
        let marked = body.clone();
        let result = tokio::select! {
            result=client.attempt(m,reader,move ||async move {marked.store(true,Ordering::SeqCst);Ok(())})=>Some(result),
            signal=&mut interrupt=>{signal?;cancelled=true;None},
        };
        let (status, code) = match result {
            Some(AttemptResult::Delivered(code)) => ("accepted", Some(code)),
            Some(AttemptResult::Temporary(code)) => ("temporary_failure", Some(code)),
            Some(AttemptResult::Permanent(code)) => ("permanent_failure", Some(code)),
            Some(AttemptResult::ConnectionLost) | None if body.load(Ordering::SeqCst) => {
                ("uncertain", None)
            }
            None => ("cancelled", None),
            _ => ("not_submitted", None),
        };
        if status == "uncertain" {
            exit = 3;
        } else if exit != 3 && cancelled {
            exit = 130;
        } else if exit == 0 && status != "accepted" {
            exit = 1;
        }
        emit(&recipient, status, code)?;
    }
    Ok(exit)
}
