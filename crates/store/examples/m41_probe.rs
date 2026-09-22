//! Synthetic process-kill queue laboratory. Never opens network connections.
use rustymail_core::Address;
use rustymail_store::{
    FaultPoint, QueueBody, QueuePlan, QueuePolicy, QueueResult, StorageRuntime, Store, StoreOptions,
};
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, action, boundary] = args.as_slice() else {
        return Err(
            "usage: m41_probe ROOT seed|enqueue|claim|body|finish|recover none|before|after".into(),
        );
    };
    let armed = Arc::new(AtomicBool::new(false));
    let flag = armed.clone();
    let stop = boundary.clone();
    let enqueue = action == "enqueue";
    let runtime = StorageRuntime::default().with_hook(move |point| {
        let matches = match stop.as_str() {
            "before" if enqueue => point == FaultPoint::BeforeCommit,
            "after" if enqueue => point == FaultPoint::AfterCommit,
            "before" => point == FaultPoint::QueueBeforeCommit,
            "after" => point == FaultPoint::QueueAfterCommit,
            _ => false,
        };
        if matches && flag.load(Ordering::SeqCst) {
            println!("RUSTYMAIL_QUEUE_CUT");
            std::io::stdout().flush()?;
            loop {
                std::thread::park();
            }
        }
        Ok(())
    });
    let options = StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    };
    let mut store = Store::open_with_runtime(root, options, runtime)?;
    let policy = QueuePolicy::default();
    match action.as_str() {
        "seed" | "enqueue" => {
            let mut stage = store.stage()?;
            stage
                .append(b"Subject: disposable queue fixture\r\n\r\nbody\r\n")
                .await?;
            armed.store(true, Ordering::SeqCst);
            let result = store.enqueue(
                stage.prepare().await?,
                QueuePlan {
                    operation_id: "11111111111111111111111111111111".into(),
                    sender: None,
                    recipients: vec![Address::parse("target@remote.test")?],
                    body: QueueBody::SevenBit,
                    max_age_seconds: 86400,
                },
            )?;
            println!("{}", serde_json::json!({"message_id":result.message_id}));
        }
        "claim" | "body" | "finish" => {
            if action == "claim" {
                armed.store(true, Ordering::SeqCst);
            }
            let mut leases = store.queue_claim(&policy, 1)?;
            let lease = leases.pop().ok_or("missing ready task")?;
            if action == "body" {
                armed.store(true, Ordering::SeqCst);
            }
            if action != "claim" {
                store.queue_mark_body(&lease)?;
            }
            if action == "finish" {
                armed.store(true, Ordering::SeqCst);
                store.queue_finish(lease, QueueResult::Delivered(250))?;
            }
        }
        "recover" => {
            let recovered = store.queue_recover(128)?;
            let integrity = store.check_integrity()?;
            if !integrity.healthy() {
                return Err("queue integrity failed".into());
            }
            println!(
                "{}",
                serde_json::json!({"recovered":recovered,"queue":store.queue_list("",128)?,"integrity":integrity})
            );
        }
        _ => return Err("unknown laboratory action".into()),
    }
    Ok(())
}
