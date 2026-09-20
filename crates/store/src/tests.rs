use super::*;
use std::process::Command;

fn private_test_directory() -> tempfile::TempDir {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder.prefix("rustymail-store-").tempdir().unwrap()
}

fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn address(name: &str) -> Address {
    Address::parse(&format!("{name}@example.com")).unwrap()
}
fn plan(key: &str, names: &[&str]) -> Acceptance {
    Acceptance {
        operation_id: key.into(),
        sender: Some(address("sender")),
        recipients: names.iter().map(|name| address(name)).collect(),
    }
}
async fn prepared(store: &Store, data: &[u8]) -> PreparedMessage {
    let mut stage = store.stage().unwrap();
    stage.append(data).await.unwrap();
    stage.prepare().await.unwrap()
}
const KEY: &str = "11111111111111111111111111111111";
const RAW: &[u8] =
    b"From: sender@example.com\r\nTo: alice@example.com\r\nSubject: test\r\n\r\nHello\r\n";

#[tokio::test]
async fn reservations_and_instance_lock_live_until_prepared_token_is_dropped() {
    let directory = private_test_directory();
    let mut opts = options();
    opts.temporary_reserved_bytes = opts.max_message_bytes;
    let store = Store::open(directory.path(), opts.clone()).unwrap();
    let stage = store.stage().unwrap();
    assert!(matches!(store.stage(), Err(StoreError::DiskReserve)));
    let prepared = stage.prepare().await.unwrap();
    assert!(matches!(store.stage(), Err(StoreError::DiskReserve)));
    drop(store);
    assert!(matches!(
        Store::open(directory.path(), opts.clone()),
        Err(StoreError::Locked)
    ));
    drop(prepared);
    let store = Store::open(directory.path(), opts).unwrap();
    assert!(store.stage().is_ok());
}

#[test]
fn cancelling_a_pending_append_poisoned_the_stage() {
    use std::{future::Future, task::Poll, time::Duration};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let directory = private_test_directory();
    let store = Store::open(directory.path(), options()).unwrap();
    let mut stage = store.stage().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (ready, started) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        let _ = ready.send(());
        let _ = wait.recv();
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    // Exceed Tokio File's internal write buffer as well as our BufWriter;
    // write_all must wait for at least one queued filesystem operation.
    let data = vec![b'x'; 4 * 1024 * 1024];
    let mut append = Box::pin(stage.append(&data));
    let pending = runtime.block_on(std::future::poll_fn(|cx| {
        Poll::Ready(append.as_mut().poll(cx).is_pending())
    }));
    drop(append);
    release.send(()).unwrap();
    runtime.block_on(blocker).unwrap();
    assert!(
        pending,
        "occupied blocking worker must suspend the file write"
    );
    assert!(matches!(
        runtime.block_on(stage.prepare()),
        Err(StoreError::PoisonedStage)
    ));
}

#[tokio::test]
async fn persists_complete_acceptance_and_account_scoped_export() {
    let directory = private_test_directory();
    let id;
    {
        let mut store = Store::open(directory.path(), options()).unwrap();
        store.create_account(&address("alice"), 1_000_000).unwrap();
        store.create_account(&address("bob"), 1_000_000).unwrap();
        let message = prepared(&store, RAW).await;
        id = store
            .accept(message, plan(KEY, &["alice"]))
            .unwrap()
            .message_id;
        let mut output = Vec::new();
        assert!(matches!(
            store.export(&address("bob"), &id, &mut output),
            Err(StoreError::NotFound)
        ));
        assert!(output.is_empty());
    }
    let store = Store::open(directory.path(), options()).unwrap();
    let list = store.list_messages(&address("alice"), 0, 10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uid, 1);
    assert_eq!(list[0].message_id, id);
    let mut output = Vec::new();
    assert_eq!(
        store.export(&address("alice"), &id, &mut output).unwrap(),
        RAW.len() as u64
    );
    assert_eq!(output, RAW);
    let report = store.check_integrity().unwrap();
    assert!(report.healthy());
    assert_eq!(report.referenced_blobs, 1);
    assert_eq!(report.orphan_blobs, 0);
}

#[tokio::test]
async fn quota_and_missing_recipient_roll_back_every_recipient() {
    let directory = private_test_directory();
    let mut store = Store::open(directory.path(), options()).unwrap();
    store.create_account(&address("alice"), 1_000_000).unwrap();
    store.create_account(&address("bob"), 1).unwrap();
    for recipients in [&["alice", "bob"][..], &["alice", "missing"][..]] {
        let message = prepared(&store, RAW).await;
        assert!(store.accept(message, plan(KEY, recipients)).is_err());
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM message", [], |r| r.get(0))
            .unwrap();
        let used: i64 = store
            .connection
            .query_row("SELECT sum(used_bytes) FROM account", [], |r| r.get(0))
            .unwrap();
        assert_eq!((count, used), (0, 0));
        assert!(
            store
                .list_messages(&address("alice"), 0, 10)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(store.check_integrity().unwrap().orphan_blobs, 2);
}

#[tokio::test]
async fn internal_retry_is_idempotent_but_conflicting_key_is_rejected() {
    let directory = private_test_directory();
    let mut store = Store::open(directory.path(), options()).unwrap();
    store.create_account(&address("alice"), 1_000_000).unwrap();
    let original = prepared(&store, RAW).await;
    let first = store.accept(original, plan(KEY, &["alice"])).unwrap();
    let retry = prepared(&store, RAW).await;
    let second = store.accept(retry, plan(KEY, &["alice", "Alice"])).unwrap();
    assert_eq!(first.message_id, second.message_id);
    assert!(second.already_committed);
    let different = prepared(&store, b"different\r\n").await;
    assert!(matches!(
        store.accept(different, plan(KEY, &["alice"])),
        Err(StoreError::IdempotencyConflict)
    ));
    assert_eq!(
        store
            .list_messages(&address("alice"), 0, 100)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn alias_uid_exhaustion_rolls_back_without_wraparound() {
    let directory = private_test_directory();
    let mut store = Store::open(directory.path(), options()).unwrap();
    store.create_account(&address("alice"), 1_000_000).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO address(address,account_id) VALUES('alias@example.com',1)",
            [],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE mailbox SET uidnext=4294967295 WHERE name='INBOX'",
            [],
        )
        .unwrap();
    let message = prepared(&store, RAW).await;
    assert!(matches!(
        store.accept(message, plan(KEY, &["alice", "alias"])),
        Err(StoreError::UidExhausted)
    ));
    assert!(
        store
            .list_messages(&address("alice"), 0, 10)
            .unwrap()
            .is_empty()
    );
    let next: i64 = store
        .connection
        .query_row("SELECT uidnext FROM mailbox WHERE name='INBOX'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(next, i64::from(u32::MAX));
}

#[test]
fn exclusive_lock_and_future_schema_are_fail_closed() {
    let directory = private_test_directory();
    let store = Store::open(directory.path(), options()).unwrap();
    assert!(matches!(
        Store::open(directory.path(), options()),
        Err(StoreError::Locked)
    ));
    store
        .connection
        .pragma_update(None, "user_version", 99)
        .unwrap();
    drop(store);
    assert!(matches!(
        Store::open(directory.path(), options()),
        Err(StoreError::SchemaVersion)
    ));
    let connection = Connection::open(directory.path().join("meta.sqlite")).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 99);
}

#[tokio::test]
async fn poisoned_stage_cannot_be_accepted_and_missing_blob_is_detected() {
    let directory = private_test_directory();
    let mut opts = options();
    opts.max_message_bytes = 1024;
    let mut store = Store::open(directory.path(), opts).unwrap();
    store.create_account(&address("alice"), 1_000_000).unwrap();
    let mut stage = store.stage().unwrap();
    stage.append(&[b'x'; 1024]).await.unwrap();
    assert!(matches!(
        stage.append(b"x").await,
        Err(StoreError::SizeLimit)
    ));
    assert!(matches!(
        stage.prepare().await,
        Err(StoreError::PoisonedStage)
    ));
    let message = prepared(&store, RAW).await;
    let path = blob_path(&store.root, &message.id).unwrap();
    store.accept(message, plan(KEY, &["alice"])).unwrap();
    fs::write(&path, b"tampered").unwrap();
    assert_eq!(store.check_integrity().unwrap().corrupt_blobs, 1);
    fs::remove_file(path).unwrap();
    assert_eq!(store.check_integrity().unwrap().missing_blobs, 1);
}

#[tokio::test]
async fn streams_the_full_size_limit_without_a_full_message_buffer() {
    let directory = private_test_directory();
    let store = Store::open(directory.path(), options()).unwrap();
    let mut stage = store.stage().unwrap();
    let chunk = [b'x'; 16 * 1024];
    for _ in 0..(25 * 1024 * 1024 / chunk.len()) {
        stage.append(&chunk).await.unwrap();
    }
    assert_eq!(stage.size(), 25 * 1024 * 1024);
    let message = stage.prepare().await.unwrap();
    assert_eq!(
        fs::metadata(blob_path(&store.root, &message.id).unwrap())
            .unwrap()
            .len(),
        message.size
    );
}

/// Child entry for the parent crash matrix. The child exits without running
/// Rust destructors, which exercises SQLite recovery but is NOT a power cut.
#[tokio::test]
async fn crash_child_entry() {
    let Ok(root) = std::env::var("RUSTYMAIL_TEST_CRASH_ROOT") else {
        return;
    };
    let point = std::env::var("RUSTYMAIL_TEST_CRASH_POINT").unwrap();
    let mut store = Store::open(root, options()).unwrap();
    let mut stage = store.stage().unwrap();
    stage.append(RAW).await.unwrap();
    if point == "staged" {
        std::process::exit(86);
    }
    let preparation_point = point.clone();
    let message = stage
        .prepare_with_hook(move |current| {
            if current == preparation_point {
                std::process::exit(86);
            }
        })
        .await
        .unwrap();
    store
        .accept_with_hook(message, plan(KEY, &["alice"]), |current| {
            if current == point {
                std::process::exit(86);
            }
        })
        .unwrap();
    panic!("fault point was not reached");
}

#[test]
fn child_process_crash_matrix_preserves_only_committed_messages() {
    for point in [
        "staged",
        "file_synced",
        "renamed",
        "directories_synced",
        "before_commit",
        "after_commit",
    ] {
        let directory = private_test_directory();
        let mut store = Store::open(directory.path(), options()).unwrap();
        store.create_account(&address("alice"), 1_000_000).unwrap();
        drop(store);
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::crash_child_entry", "--nocapture"])
            .env("RUSTYMAIL_TEST_CRASH_ROOT", directory.path())
            .env("RUSTYMAIL_TEST_CRASH_POINT", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(86),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let store = Store::open(directory.path(), options()).unwrap();
        let report = store.check_integrity().unwrap();
        assert!(report.healthy(), "{point}: {report:?}");
        assert_eq!(
            store.list_messages(&address("alice"), 0, 10).unwrap().len(),
            usize::from(point == "after_commit"),
            "{point}"
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_non_private_storage_directory() {
    use std::os::unix::fs::PermissionsExt;
    let directory = private_test_directory();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        Store::open(directory.path(), options()),
        Err(StoreError::UnsafePermissions)
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_storage_directories() {
    use std::os::unix::fs::symlink;
    let directory = private_test_directory();
    let elsewhere = private_test_directory();
    symlink(elsewhere.path(), directory.path().join("blobs")).unwrap();
    assert!(matches!(
        Store::open(directory.path(), options()),
        Err(StoreError::UnsafePath)
    ));
}
