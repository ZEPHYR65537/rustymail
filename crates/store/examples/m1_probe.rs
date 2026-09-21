//! Explicit laboratory binary; built only with the test-support feature.
use rustymail_core::Address;
use rustymail_store::{Acceptance, GcOptions, StorageRuntime, Store, StoreOptions};
use std::{io::Write, path::Path};

const BASE: &str = "11111111111111111111111111111111";
const CANDIDATE: &str = "22222222222222222222222222222222";
const RAW: &[u8] = b"From: sender@example.com\r\nTo: alice@example.com\r\nSubject: M1 fault probe\r\n\r\nDurable laboratory message.\r\n";

fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 0,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn address() -> Address {
    Address::parse("alice@example.com").unwrap()
}
async fn deliver(store: &mut Store, operation: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut stage = store.stage()?;
    stage.append(RAW).await?;
    let message = stage.prepare().await?;
    store.accept(
        message,
        Acceptance {
            operation_id: operation.into(),
            sender: None,
            recipients: vec![address()],
        },
    )?;
    Ok(())
}
fn stop_at(point: &str) -> ! {
    println!("RUSTYMAIL_POINT:{point}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn fill_disk(root: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    require_disposable_volume(root)?;
    use std::fs::OpenOptions;
    let path = root
        .parent()
        .ok_or("missing lab parent")?
        .join("fault-filler");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let chunk = [0xabu8; 65536];
    let mut total = 0u64;
    loop {
        match file.write_all(&chunk) {
            Ok(()) => {
                total += chunk.len() as u64;
                if total > 512 * 1024 * 1024 {
                    return Err("lab volume was not bounded to 512 MiB".into());
                }
            }
            Err(error) if error.raw_os_error() == Some(28) => {
                let _ = file.sync_all();
                println!(
                    "RUSTYMAIL_ENOSPC:{}",
                    serde_json::json!({"errno":28,"bytes_written_before_failure":total})
                );
                return Ok(path);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn require_disposable_volume(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let command_line = std::fs::read_to_string("/proc/cmdline")?;
    if root != Path::new("/data/mail")
        || !command_line
            .split_ascii_whitespace()
            .any(|item| item == "rustymail_disposable_vm=1")
        || fs2::total_space("/data")? > 512 * 1024 * 1024
    {
        return Err("disk-fill probes require the dedicated, bounded rustymail QEMU guest".into());
    }
    Ok(())
}

fn smtp_transaction(root: &Path, full: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        io::{BufRead, BufReader},
        net::TcpStream,
        time::{Duration, Instant},
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        match TcpStream::connect("127.0.0.1:2525") {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.into()),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    fn response(reader: &mut BufReader<TcpStream>) -> Result<u16, Box<dyn std::error::Error>> {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err("SMTP EOF".into());
            }
            if line.len() < 4 {
                return Err("invalid SMTP response".into());
            }
            if line.as_bytes()[3] == b' ' {
                return Ok(line[..3].parse()?);
            }
        }
    }
    if response(&mut reader)? != 220 {
        return Err("SMTP banner".into());
    }
    for (command, expected) in [
        ("EHLO test\r\n", 250),
        ("MAIL FROM:<>\r\n", 250),
        ("RCPT TO:<alice@example.com>\r\n", 250),
        ("DATA\r\n", 354),
    ] {
        writer.write_all(command.as_bytes())?;
        if response(&mut reader)? != expected {
            return Err("SMTP phase failed".into());
        }
    }
    let filler = if full { Some(fill_disk(root)?) } else { None };
    writer.write_all(RAW)?;
    writer.write_all(b".\r\n")?;
    let code = response(&mut reader)?;
    if let Some(filler) = filler {
        std::fs::remove_file(filler)?;
        if code != 451 {
            return Err(format!("ENOSPC returned {code}, expected temporary failure").into());
        }
        println!(
            "RUSTYMAIL_SMTP_ENOSPC:{}",
            serde_json::json!({"final_reply":code,"accepted":false})
        );
    } else {
        if code != 250 {
            return Err("expected final SMTP 250".into());
        }
        stop_at("acknowledged");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?.as_str();
    let root = Path::new(args.get(2).ok_or("new lab directory required")?);
    let point = args.get(3).map_or("", String::as_str);
    if matches!(mode, "smtp-full" | "sqlite-full") {
        require_disposable_volume(root)?;
    }
    match mode {
        "seed" | "seed-legacy" | "crash" => {
            if root.exists() {
                return Err("probe seed refuses an existing directory".into());
            }
            let mut store = Store::open(root, options())?;
            store.create_account(&address(), 100 * 1024 * 1024)?;
            deliver(&mut store, BASE).await?;
            drop(store);
            if mode == "seed-legacy" || point.starts_with("migration_") {
                let connection = rusqlite::Connection::open(root.join("meta.sqlite"))?;
                connection.execute_batch("DROP TABLE gc_action; DROP TABLE maintenance_run; DROP TABLE schema_migration; PRAGMA user_version=1;")?;
            }
            if mode == "crash" {
                let fault = point.to_owned();
                let runtime = StorageRuntime::default().with_hook(move |at| {
                    if at.name() == fault {
                        stop_at(at.name());
                    }
                    Ok(())
                });
                let mut store = Store::open_with_runtime(root, options(), runtime)?;
                if point.starts_with("gc_") {
                    let mut stage = store.stage()?;
                    stage.append(RAW).await?;
                    drop(stage.prepare().await?);
                    store.gc(
                        GcOptions {
                            apply: true,
                            min_age_seconds: 0,
                            limit: 1000,
                        },
                        |_| Ok(()),
                    )?;
                    return Err("GC fault point was not reached".into());
                }
                let mut stage = store.stage()?;
                stage.append(RAW).await?;
                if point == "staged" {
                    stop_at(point);
                }
                let message = stage.prepare().await?;
                store.accept(
                    message,
                    Acceptance {
                        operation_id: CANDIDATE.into(),
                        sender: None,
                        recipients: vec![address()],
                    },
                )?;
                stop_at("acknowledged");
            }
            println!("RUSTYMAIL_SEEDED");
        }
        "verify" => {
            let version_before_reopen: u32 = {
                let connection = rusqlite::Connection::open(root.join("meta.sqlite"))?;
                connection.pragma_query_value(None, "user_version", |r| r.get(0))?
            };
            let expected_version = if point == "migration_applied" {
                1
            } else {
                rustymail_store::CURRENT_VERSION
            };
            if version_before_reopen != expected_version {
                return Err("migration did not recover atomically".into());
            }
            let store = Store::open_existing(root, options())?;
            let report = store.check_integrity()?;
            if !report.healthy() {
                return Err("recovered store is inconsistent".into());
            }
            let expected = if matches!(point, "after_commit" | "acknowledged") {
                2
            } else {
                1
            };
            let messages = store.list_messages(&address(), 0, 10)?;
            if messages.len() != expected {
                return Err("unexpected recovered message count".into());
            }
            for message in messages {
                let mut bytes = Vec::new();
                store.export(&address(), &message.message_id, &mut bytes)?;
                if bytes != RAW {
                    return Err("recovered raw bytes differ".into());
                }
            }
            println!(
                "RUSTYMAIL_VERIFIED:{}",
                serde_json::json!({"point":point,"messages":expected,"integrity":report,"schema":rustymail_store::CURRENT_VERSION,"version_before_reopen":version_before_reopen})
            );
        }
        "smtp-ack" => smtp_transaction(root, false)?,
        "smtp-full" => smtp_transaction(root, true)?,
        "sqlite-full" => {
            let mut store = Store::open_existing(root, options())?;
            store.checkpoint()?;
            let mut stage = store.stage()?;
            stage.append(RAW).await?;
            let ready = stage.prepare().await?;
            let filler = fill_disk(root)?;
            let result = store.accept(
                ready,
                Acceptance {
                    operation_id: CANDIDATE.into(),
                    sender: None,
                    recipients: vec![address()],
                },
            );
            std::fs::remove_file(filler)?;
            if result.is_ok() {
                return Err("SQLite unexpectedly committed on full lab volume".into());
            }
            drop(store);
            let store = Store::open_existing(root, options())?;
            if store.operation(CANDIDATE)?.is_some() {
                return Err("SQLite full transaction became visible".into());
            }
            println!(
                "RUSTYMAIL_SQLITE_ENOSPC:{}",
                serde_json::json!({"acceptance_error":result.unwrap_err().to_string(),"candidate_visible":false})
            );
        }
        _ => return Err("unknown laboratory mode".into()),
    }
    Ok(())
}
