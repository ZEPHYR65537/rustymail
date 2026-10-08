use crate::{admin::AdminRequest, store_options};
use clap::{Parser, Subcommand};
use rustymail_core::{Address, config::Config};
use rustymail_store::{GcOptions, QueueBody, QueuePlan, Store};
use std::{
    fs::OpenOptions,
    io::{IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Local lab administration: offline store lock or private Unix --socket"
)]
pub struct Args {
    #[arg(long, default_value = "deploy/rustymail.lab.toml")]
    config: PathBuf,
    /// Use the daemon's private Unix socket instead of offline administration.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    ReloadTls,
    Credential {
        #[command(subcommand)]
        command: CredentialCommand,
    },
    SendAs {
        login: String,
        address: String,
        #[arg(long)]
        disable: bool,
    },
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
    Operation {
        operation_id: String,
    },
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
    /// Offline relay queue laboratories and administration; never sends mail.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
}

#[derive(Subcommand)]
enum QueueCommand {
    Show {
        delivery_id: String,
    },
    /// Expire one bounded batch and create due notifications offline; no sockets.
    Maintain {
        #[arg(long, default_value_t = 16)]
        limit: usize,
    },
    /// Resolve the operational duty while retaining an unknown delivery outcome.
    CloseUnknown {
        delivery_id: String,
        #[arg(long)]
        reason: String,
    },
    History {
        delivery_id: String,
        #[arg(long, default_value_t = 0)]
        after_id: i64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    List {
        #[arg(long, default_value = "")]
        after_id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Store synthetic outbound bytes. No SMTP authorization or transmission.
    ImportLab {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        sender: Option<String>,
        #[arg(long = "recipient", required = true)]
        recipients: Vec<String>,
        #[arg(long,default_value="7bit",value_parser=["7bit","8bitmime"])]
        body: String,
        #[arg(long, default_value_t = 432000)]
        max_age_seconds: u64,
    },
    Recover {
        #[arg(long, default_value_t = 128)]
        limit: usize,
    },
    Hold {
        delivery_id: String,
    },
    Retry {
        delivery_id: String,
        /// A held/uncertain attempt may already have reached the remote mailbox.
        #[arg(long)]
        allow_duplicate: bool,
        /// Explicitly renew an expired task's configured lifetime.
        #[arg(long)]
        extend_expired: bool,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Change quota offline; cannot reduce below currently used bytes.
    Quota {
        address: String,
        bytes: u64,
    },
    Disable {
        address: String,
    },
    Add {
        address: String,
        #[arg(long, default_value_t = 1073741824)]
        quota_bytes: u64,
    },
}

#[derive(Subcommand)]
enum CredentialCommand {
    /// Generate a random application password; display once on a terminal or save privately.
    Create {
        login: String,
        #[arg(long)]
        label: String,
        #[arg(long,default_value="mail",value_parser=["mail","read_only"])]
        scope: String,
        #[arg(long)]
        secret_output: Option<PathBuf>,
    },
    List {
        login: String,
        #[arg(long, default_value_t = 0)]
        after_id: i64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Revoke {
        selector: String,
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

pub async fn entry(args: Args) -> ExitCode {
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(args.config)?;
    config.validate()?;
    if let Some((request, secret_path, creates_secret)) = management_request(&args.command) {
        if creates_secret && secret_path.is_none() && !std::io::stdout().is_terminal() {
            return Err("credential creation needs a terminal or --secret-output; secrets are never command arguments".into());
        }
        let mut secret_file = if let Some(path) = &secret_path {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            Some(options.open(path)?)
        } else {
            None
        };
        let result = match args.socket {
            Some(path) => online_management(&path, &request).await,
            None => offline_management(&config, request),
        };
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                drop(secret_file);
                if let Some(path) = secret_path {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        };
        if creates_secret {
            let secret = result
                .get_mut("application_password")
                .ok_or("missing generated credential")?
                .take();
            let serde_json::Value::String(secret) = secret else {
                return Err("invalid generated credential".into());
            };
            let secret = zeroize::Zeroizing::new(secret);
            if let Some(file) = &mut secret_file {
                writeln!(file, "{}", &*secret)?;
                file.sync_all()?;
            } else {
                println!("application_password: {}", &*secret);
            }
            result
                .as_object_mut()
                .ok_or("invalid credential response")?
                .remove("application_password");
            result["secret_saved"] = serde_json::json!(secret_path.is_some());
        }
        println!("{result}");
        return Ok(());
    }
    if args.socket.is_some() {
        return Err(
            "this command requires offline access; stop the daemon and omit --socket".into(),
        );
    }
    let mut store = if matches!(&args.command, Command::Account { .. }) {
        Store::open(&config.data_dir, store_options(&config))?
    } else {
        Store::open_existing(&config.data_dir, store_options(&config))?
    };
    match args.command {
        Command::Status
        | Command::ReloadTls
        | Command::Credential { .. }
        | Command::SendAs { .. }
        | Command::Account {
            command: AccountCommand::Disable { .. },
        } => unreachable!("management command handled above"),
        Command::Operation { operation_id } => {
            println!(
                "{}",
                serde_json::json!({"operation":store.operation(&operation_id)?})
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
        Command::Queue { command } => match command {
            QueueCommand::Show { delivery_id } => {
                println!(
                    "{}",
                    serde_json::json!({"delivery":store.queue_show(&delivery_id)?,"report_deliveries":store.queue_report_deliveries(&delivery_id)?})
                );
            }
            QueueCommand::Maintain { limit } => {
                if !(1..=128).contains(&limit) {
                    return Err("limit must be 1..128".into());
                }
                let expired = store.queue_expire(limit)?;
                let policy = crate::relay::notification_policy(&config);
                let (mut created, mut suppressed, mut pending) = (0, 0, 0);
                for _ in 0..limit {
                    match store.notification_next(&policy) {
                        Ok(rustymail_store::NotificationWork::Empty) => break,
                        Ok(rustymail_store::NotificationWork::Suppressed) => suppressed += 1,
                        Ok(rustymail_store::NotificationWork::Prepare(task)) => {
                            let id = task.delivery_id().to_owned();
                            let result = match task.prepare().await {
                                Ok(p) => store.notification_commit(p),
                                Err(e) => Err(e),
                            };
                            match result {
                                Ok(_) => created += 1,
                                Err(e) => {
                                    store.notification_failed(&id, &e)?;
                                    pending += 1;
                                }
                            }
                        }
                        Err(rustymail_store::StoreError::ClockChanged) => {
                            return Err(rustymail_store::StoreError::ClockChanged.into());
                        }
                        Err(_) => {
                            pending += 1;
                            break;
                        }
                    }
                }
                println!(
                    "{}",
                    serde_json::json!({"expired":expired,"reports_created":created,"suppressed":suppressed,"pending_errors":pending,"scan_limit":limit,"transmitted":false})
                );
                if pending > 0 {
                    return Err("some notifications remain pending; inspect queue show".into());
                }
            }
            QueueCommand::CloseUnknown {
                delivery_id,
                reason,
            } => {
                store.queue_close_unknown(&delivery_id, &reason)?;
                println!(
                    "{}",
                    serde_json::json!({"closed":delivery_id,"outcome":"unknown"})
                );
            }
            QueueCommand::History {
                delivery_id,
                after_id,
                limit,
            } => {
                for event in store.queue_history(&delivery_id, after_id, limit)? {
                    println!("{}", serde_json::to_string(&event)?);
                }
            }
            QueueCommand::List { after_id, limit } => {
                for row in store.queue_list(&after_id, limit)? {
                    println!("{}", serde_json::to_string(&row)?);
                }
            }
            QueueCommand::ImportLab {
                source,
                operation_id,
                sender,
                recipients,
                body,
                max_age_seconds,
            } => {
                use tokio::io::AsyncReadExt;
                let sender = sender.as_deref().map(Address::parse).transpose()?;
                let recipients = recipients
                    .iter()
                    .map(|r| Address::parse(r))
                    .collect::<Result<Vec<_>, _>>()?;
                if recipients.iter().any(|r| {
                    config
                        .local_domains
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(r.domain()))
                }) {
                    return Err("queue import-lab requires remote recipient domains".into());
                }
                let mut file = tokio::fs::File::open(source).await?;
                if !file.metadata().await?.is_file() {
                    return Err("source must be a regular file".into());
                }
                let mut stage = store.stage()?;
                let mut buffer = vec![0; config.limits.stream_buffer_bytes];
                loop {
                    let count = file.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    stage.append(&buffer[..count]).await?;
                }
                let result = store.enqueue(
                    stage.prepare().await?,
                    QueuePlan {
                        operation_id,
                        sender,
                        recipients,
                        body: if body == "7bit" {
                            QueueBody::SevenBit
                        } else {
                            QueueBody::EightBitMime
                        },
                        max_age_seconds,
                    },
                )?;
                println!(
                    "{}",
                    serde_json::json!({"message_id":result.message_id,"already_committed":result.already_committed,"transmitted":false})
                );
            }
            QueueCommand::Recover { limit } => {
                println!(
                    "{}",
                    serde_json::json!({"recovered":store.queue_recover(limit)?,"scan_limit":limit})
                );
            }
            QueueCommand::Hold { delivery_id } => {
                store.queue_hold(&delivery_id)?;
                println!("{}", serde_json::json!({"held":delivery_id}));
            }
            QueueCommand::Retry {
                delivery_id,
                allow_duplicate,
                extend_expired,
            } => {
                store.queue_retry(&delivery_id, allow_duplicate, extend_expired)?;
                println!(
                    "{}",
                    serde_json::json!({"scheduled":delivery_id,"transmitted":false})
                );
            }
        },
        Command::Account {
            command: AccountCommand::Quota { address, bytes },
        } => {
            let address = Address::parse(&address)?;
            store.set_account_quota(&address, bytes)?;
            println!(
                "{}",
                serde_json::json!({"account":address.local_key(),"quota_bytes":bytes})
            );
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
            let result = store
                .export(&address, &message_id, &mut file)
                .and_then(|bytes| {
                    file.sync_all()?;
                    Ok(bytes)
                });
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    drop(file);
                    let _ = std::fs::remove_file(&output);
                    return Err(error.into());
                }
            };
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
                "queue_mismatches":report.queue_mismatches,
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

fn management_request(command: &Command) -> Option<(AdminRequest, Option<PathBuf>, bool)> {
    let request = match command {
        Command::Status => AdminRequest::Status,
        Command::ReloadTls => AdminRequest::ReloadTls,
        Command::Account {
            command: AccountCommand::Disable { address },
        } => AdminRequest::AccountDisable {
            login: address.clone(),
        },
        Command::SendAs {
            login,
            address,
            disable,
        } => AdminRequest::SendAs {
            login: login.clone(),
            address: address.clone(),
            enabled: !*disable,
        },
        Command::Credential { command } => match command {
            CredentialCommand::Create {
                login,
                label,
                scope,
                secret_output,
            } => {
                return Some((
                    AdminRequest::CredentialCreate {
                        login: login.clone(),
                        label: label.clone(),
                        scope: scope.clone(),
                    },
                    secret_output.clone(),
                    true,
                ));
            }
            CredentialCommand::List {
                login,
                after_id,
                limit,
            } => AdminRequest::CredentialList {
                login: login.clone(),
                after_id: *after_id,
                limit: *limit,
            },
            CredentialCommand::Revoke { selector } => AdminRequest::CredentialRevoke {
                selector: selector.clone(),
            },
        },
        _ => return None,
    };
    Some((request, None, false))
}

fn offline_management(
    config: &Config,
    request: AdminRequest,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if matches!(request, AdminRequest::ReloadTls) {
        return Err("reload-tls needs the running daemon's --socket".into());
    }
    let mut store = Store::open_existing(&config.data_dir, store_options(config))?;
    let credential = if request.needs_credential(config)? {
        Some(crate::auth::generate_credential(&config.authentication)?)
    } else {
        None
    };
    Ok(request.apply(&mut store, config, credential, "offline")?)
}

#[cfg(unix)]
async fn online_management(
    path: &std::path::Path,
    request: &AdminRequest,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use crate::admin::{FrameKind, read_frame, write_frame};
    use std::{
        os::unix::fs::{FileTypeExt, MetadataExt},
        time::Duration,
    };
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.mode() & 0o077 != 0 {
        return Err("unsafe management socket".into());
    }
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::UnixStream::connect(path),
    )
    .await??;
    let peer = stream.peer_cred()?;
    if peer.uid() != metadata.uid() || peer.gid() != metadata.gid() {
        return Err("management daemon identity mismatch".into());
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        write_frame(
            &mut stream,
            &serde_json::to_value(request)?,
            FrameKind::Request,
        ),
    )
    .await??;
    let bytes = tokio::time::timeout(
        Duration::from_secs(60),
        read_frame(&mut stream, FrameKind::Response),
    )
    .await??;
    let mut response: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "invalid management response")?;
    if response["ok"] != true {
        return Err("management request failed; inspect the daemon's redacted audit events".into());
    }
    Ok(response["result"].take())
}
#[cfg(not(unix))]
async fn online_management(
    _path: &std::path::Path,
    _request: &AdminRequest,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Err("online management uses Unix sockets; Windows supports offline management only".into())
}
