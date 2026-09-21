use clap::{Parser, Subcommand};
use rustymail_core::{Address, config::Config};
use rustymail_server::store_options;
use rustymail_store::{GcOptions, Store};
use std::{fs::OpenOptions, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Offline lab administration; stop rustymaild before use"
)]
struct Args {
    #[arg(long, default_value = "deploy/rustymail.lab.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a local receive-only account and its initial folders.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// List or export raw mail while holding the exclusive data-directory lock.
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
    /// Check database/foreign keys and stream-verify every referenced blob.
    CheckStore,
    /// Query an internal acceptance ID after an unknown commit outcome.
    Operation { operation_id: String },
    /// Show validated migration history (opens/upgrades a supported legacy store).
    Migrations,
    /// Preview stale temporary files and unreferenced blobs; --apply deletes.
    Gc {
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 86400)]
        min_age_seconds: u64,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Inspect durable maintenance runs, including interrupted runs.
    GcHistory {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Restore a missing referenced blob from an exact copy; never overwrites.
    RecoverBlob {
        blob_id: String,
        #[arg(long)]
        source: PathBuf,
    },
    /// Perform an offline WAL checkpoint, preserving FULL durability.
    Checkpoint,
}

#[derive(Subcommand)]
enum AccountCommand {
    Add {
        address: String,
        #[arg(long, default_value_t = 1073741824)]
        quota_bytes: u64,
    },
}

#[derive(Subcommand)]
enum MailCommand {
    List {
        address: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        after_uid: u32,
    },
    Export {
        address: String,
        message_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(args.config)?;
    config.require_lab_receiver()?;
    let mut store = if matches!(&args.command, Command::Account { .. }) {
        Store::open(&config.data_dir, store_options(&config))?
    } else {
        Store::open_existing(&config.data_dir, store_options(&config))?
    };
    match args.command {
        Command::Operation { operation_id } => {
            println!(
                "{}",
                serde_json::json!({"operation":store.operation(&operation_id)?})
            );
        }
        Command::Migrations => {
            println!(
                "{}",
                serde_json::json!({"schema_version":rustymail_store::CURRENT_VERSION,"history":store.migration_history()?})
            );
        }
        Command::Gc {
            apply,
            min_age_seconds,
            limit,
        } => {
            use std::io::Write;
            let output = std::io::stdout();
            let mut output = output.lock();
            let report = store.gc(
                GcOptions {
                    apply,
                    min_age_seconds,
                    limit,
                },
                |candidate| {
                    writeln!(output, "{}", serde_json::json!({"gc_candidate":candidate}))?;
                    output.flush()?;
                    Ok(())
                },
            )?;
            writeln!(output, "{}", serde_json::json!({"gc_report":report}))?;
        }
        Command::GcHistory { limit } => {
            println!("{}", serde_json::json!({"runs":store.gc_history(limit)?}));
        }
        Command::RecoverBlob { blob_id, source } => {
            let bytes = store.recover_blob(&blob_id, source)?;
            println!(
                "{}",
                serde_json::json!({"recovered_blob":blob_id,"bytes":bytes})
            );
        }
        Command::Checkpoint => {
            store.checkpoint()?;
            println!("{}", serde_json::json!({"checkpoint":"complete"}));
        }
        Command::Account {
            command:
                AccountCommand::Add {
                    address,
                    quota_bytes,
                },
        } => {
            let address = Address::parse(&address)?;
            if !config
                .local_domains
                .iter()
                .any(|domain| domain.eq_ignore_ascii_case(address.domain()))
            {
                return Err("account domain is not configured as local".into());
            }
            store.create_account(&address, quota_bytes)?;
            println!(
                "{}",
                serde_json::json!({"account":address.local_key(),"receive_only":true,"quota_bytes":quota_bytes})
            );
        }
        Command::Mail {
            command:
                MailCommand::List {
                    address,
                    limit,
                    after_uid,
                },
        } => {
            let address = Address::parse(&address)?;
            let messages = store.list_messages(&address, after_uid, limit)?;
            for message in messages {
                println!(
                    "{}",
                    serde_json::json!({"uid":message.uid,"message_id":message.message_id,
                    "size_bytes":message.size_bytes,"accepted_at_ms":message.accepted_at_ms})
                );
            }
        }
        Command::Mail {
            command:
                MailCommand::Export {
                    address,
                    message_id,
                    output,
                },
        } => {
            let address = Address::parse(&address)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&output)?;
            let result = store.export(&address, &message_id, &mut file);
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    drop(file);
                    let _ = std::fs::remove_file(&output);
                    return Err(error.into());
                }
            };
            file.sync_all()?;
            println!(
                "{}",
                serde_json::json!({"exported_bytes":bytes,"message_id":message_id})
            );
        }
        Command::CheckStore => {
            let report = store.check_integrity()?;
            println!(
                "{}",
                serde_json::json!({"healthy":report.healthy(),"referenced_blobs":report.referenced_blobs,
                "missing_blobs":report.missing_blobs,"corrupt_blobs":report.corrupt_blobs,
                "orphan_blobs":report.orphan_blobs,"staging_files":report.staging_files,
                "unexpected_files":report.unexpected_files,"quota_mismatches":report.quota_mismatches,
                "uid_mismatches":report.uid_mismatches,"delivery_mismatches":report.delivery_mismatches,
                "directory_sync_supported":Store::directory_sync_supported()})
            );
            if !report.healthy() {
                return Err(
                    "referenced message integrity failed; preserve data and recover".into(),
                );
            }
        }
    }
    Ok(())
}
