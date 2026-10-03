//! Disposable store-only notification process-kill fixture; no network or AUTH.
use rustymail_core::Address;
use rustymail_store::{
    Acceptance, FaultPoint, NotificationPolicy, NotificationWork, QueueBody, QueuePolicy,
    QueueResult, RelayAcceptance, StorageRuntime, Store, StoreOptions, SubmissionIdentity,
};
use std::io::Write;

fn cut() -> std::io::Result<()> {
    println!("RUSTYMAIL_NOTIFICATION_CUT");
    std::io::stdout().flush()?;
    loop {
        std::thread::park();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, action, boundary] = args.as_slice() else {
        return Err("usage: m43_probe ROOT seed|notify none|file|directories|prepared|before|after|returned".into());
    };
    let stop = boundary.clone();
    let runtime = StorageRuntime::default().with_hook(move |point| {
        let matches = match stop.as_str() {
            "file" => point == FaultPoint::FileSynced,
            "directories" => point == FaultPoint::DirectoriesSynced,
            "prepared" => point == FaultPoint::NotificationPrepared,
            "before" => point == FaultPoint::NotificationBeforeCommit,
            "after" => point == FaultPoint::NotificationAfterCommit,
            _ => false,
        };
        if matches {
            cut()?;
        }
        Ok(())
    });
    let options = StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    };
    let mut store = Store::open_with_runtime(root, options, runtime)?;
    match action.as_str() {
        "seed" => {
            let login = Address::parse("alice@example.com")?;
            store.create_account(&login, 100_000)?;
            store.set_send_as(&login, &login, true)?;
            let selector = "12345678123456781234567812345678";
            // A storage-authority fixture, never used to authenticate a socket.
            store.create_credential(&login, selector, "lab", "mail", "$argon2id$v=19$fixture")?;
            let principal = store
                .credential_lookup(&login, selector)?
                .ok_or("missing principal")?
                .principal;
            let mut stage = store.stage()?;
            stage.append(b"Return-Path: <alice@example.com>\r\nFrom: alice@example.com\r\n\r\nfixture\r\n").await?;
            store.accept_relay_submission(
                stage.prepare().await?,
                Acceptance {
                    operation_id: "11111111111111111111111111111111".into(),
                    sender: Some(login.clone()),
                    recipients: vec![],
                },
                SubmissionIdentity {
                    principal,
                    author: login,
                },
                RelayAcceptance {
                    recipients: vec![Address::parse("target@remote.test")?],
                    body: QueueBody::SevenBit,
                    max_age_seconds: 86400,
                },
            )?;
            let lease = store
                .queue_claim(&QueuePolicy::default(), 1)?
                .pop()
                .ok_or("missing task")?;
            store.queue_finish(lease, QueueResult::Permanent(550))?;
        }
        "notify" => {
            let policy = NotificationPolicy {
                hostname: "mail.example.com".into(),
                local_domains: vec!["example.com".into()],
                max_age_seconds: 86400,
            };
            if let NotificationWork::Prepare(task) = store.notification_next(&policy)? {
                let prepared = task.prepare().await?;
                store.notification_commit(prepared)?;
            }
            if boundary == "returned" {
                cut()?;
            }
            let integrity = store.check_integrity()?;
            if !integrity.healthy() {
                return Err("notification integrity failed".into());
            }
            println!(
                "{}",
                serde_json::json!({"queue":store.queue_list("",128)?,"integrity":integrity})
            );
        }
        _ => return Err("unknown laboratory action".into()),
    }
    Ok(())
}
