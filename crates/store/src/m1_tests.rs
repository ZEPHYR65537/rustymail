use super::*;
use std::{
    io,
    sync::atomic::{AtomicBool, AtomicI64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

const OP: &str = "11111111111111111111111111111111";
const RAW: &[u8] = b"Subject: M1\r\n\r\nPreserve these bytes.\r\n";
fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn account() -> Address {
    Address::parse("alice@example.com").unwrap()
}
fn plan() -> Acceptance {
    Acceptance {
        operation_id: OP.into(),
        sender: None,
        recipients: vec![account()],
    }
}
async fn prepared(store: &Store) -> PreparedMessage {
    let mut stage = store.stage().unwrap();
    stage.append(RAW).await.unwrap();
    stage.prepare().await.unwrap()
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
fn old(path: &Path) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
}
fn apply() -> GcOptions {
    GcOptions {
        apply: true,
        min_age_seconds: 0,
        limit: 1000,
    }
}

#[tokio::test]
async fn io_fault_boundaries_never_create_false_acceptance() {
    for point in [
        FaultPoint::StageCreate,
        FaultPoint::Append,
        FaultPoint::Flush,
        FaultPoint::FileSync,
        FaultPoint::FileSynced,
        FaultPoint::Rename,
        FaultPoint::Renamed,
        FaultPoint::BlobDirectorySync,
        FaultPoint::StagingDirectorySync,
        FaultPoint::DirectoriesSynced,
        FaultPoint::DatabaseWrite,
        FaultPoint::BeforeCommit,
        FaultPoint::Commit,
        FaultPoint::AfterCommit,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("mail");
        let fired = Arc::new(AtomicBool::new(false));
        let signal = fired.clone();
        let runtime = StorageRuntime::default().with_hook(move |at| {
            if at == point {
                signal.store(true, Ordering::SeqCst);
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "injected storage failure",
                ));
            }
            Ok(())
        });
        let mut store = Store::open_with_runtime(&root, options(), runtime).unwrap();
        store.create_account(&account(), 10000).unwrap();
        let result: Result<AcceptedMessage, StoreError> = async {
            let mut stage = store.stage()?;
            stage.append(RAW).await?;
            let message = stage.prepare().await?;
            store.accept(message, plan())
        }
        .await;
        assert!(result.is_err(), "{point:?}");
        assert!(fired.load(Ordering::SeqCst));
        if matches!(point, FaultPoint::Commit | FaultPoint::AfterCommit) {
            assert!(matches!(result, Err(StoreError::OutcomeUnknown)));
        }
        drop(store);
        let mut store = Store::open_existing(&root, options()).unwrap();
        assert!(store.check_integrity().unwrap().healthy());
        assert_eq!(
            store.operation(OP).unwrap().is_some(),
            point == FaultPoint::AfterCommit,
            "{point:?}"
        );
        assert_eq!(
            store.list_messages(&account(), 0, 10).unwrap().len(),
            usize::from(point == FaultPoint::AfterCommit)
        );
        store.gc(apply(), |_| Ok(())).unwrap();
        assert!(store.check_integrity().unwrap().healthy());
    }
}

#[tokio::test]
async fn gc_is_bounded_audited_and_preserves_all_retention_roots() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("mail");
    let clock = Arc::new(TestClock(AtomicI64::new(100_000)));
    let mut store =
        Store::open_with_runtime(&root, options(), StorageRuntime::with_clock(clock.clone()))
            .unwrap();
    store.create_account(&account(), 10000).unwrap();
    let message = prepared(&store).await;
    store.accept(message, plan()).unwrap();
    let accepted = store.operation(OP).unwrap().unwrap();
    assert_eq!(accepted.accepted_at_ms, 100_000);
    store
        .connection
        .execute(
            "INSERT INTO backup_pin VALUES('backup',?1,1)",
            [&accepted.blob_id],
        )
        .unwrap();
    let extra = prepared(&store).await;
    let retained = extra.id.clone();
    store
        .connection
        .execute(
            "INSERT INTO blob VALUES(?1,?2,?3,?4,1,1)",
            params![extra.id, extra.size as i64, extra.hash, b"{}".as_slice()],
        )
        .unwrap();
    store
        .connection
        .execute("INSERT INTO backup_pin VALUES('backup',?1,1)", [&retained])
        .unwrap();
    drop(extra);
    let orphan = prepared(&store).await;
    let orphan_path = blob_path(&store.root, &orphan.id).unwrap();
    drop(orphan);
    old(&orphan_path);
    let staging = root.join("staging/33333333333333333333333333333333.part");
    fs::write(&staging, b"partial").unwrap();
    old(&staging);
    let recent = prepared(&store).await;
    let recent_path = blob_path(&store.root, &recent.id).unwrap();
    drop(recent);
    File::options()
        .write(true)
        .open(&recent_path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(99))
        .unwrap();
    let settings = GcOptions {
        min_age_seconds: 10,
        limit: 1,
        apply: false,
    };
    let preview = store.gc(settings, |_| Ok(())).unwrap();
    assert_eq!(preview.candidates, 1);
    assert!(preview.more);
    assert_eq!(preview.deleted, 0);
    assert!(store.gc_history(10).unwrap().is_empty());
    let first = store
        .gc(
            GcOptions {
                apply: true,
                ..settings
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(first.deleted, 1);
    assert!(first.more);
    let second = store
        .gc(
            GcOptions {
                apply: true,
                ..settings
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(second.deleted, 1);
    assert!(recent_path.exists());
    assert!(!orphan_path.exists());
    assert!(!staging.exists());
    assert!(blob_path(&store.root, &retained).unwrap().exists());
    assert_eq!(
        store
            .gc_history(10)
            .unwrap()
            .iter()
            .map(|r| r.deleted)
            .sum::<u64>(),
        2
    );
    assert!(
        store
            .gc_history(10)
            .unwrap()
            .iter()
            .all(|r| r.status == "complete")
    );
    clock.0.store(50_000, Ordering::SeqCst);
    assert_eq!(
        store
            .gc(
                GcOptions {
                    apply: true,
                    ..settings
                },
                |_| Ok(())
            )
            .unwrap()
            .deleted,
        0
    );
    let mut bytes = Vec::new();
    store
        .export(&account(), &accepted.message_id, &mut bytes)
        .unwrap();
    assert_eq!(bytes, RAW);
}

#[tokio::test]
async fn maintenance_refuses_active_tokens_and_inconsistent_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("mail");
    let mut store = Store::open(&root, options()).unwrap();
    store.create_account(&account(), 10000).unwrap();
    let stage = store.stage().unwrap();
    assert!(matches!(
        store.gc(apply(), |_| Ok(())),
        Err(StoreError::MaintenanceBusy)
    ));
    drop(stage);
    let ready = prepared(&store).await;
    assert!(matches!(
        store.gc(apply(), |_| Ok(())),
        Err(StoreError::MaintenanceBusy)
    ));
    store.accept(ready, plan()).unwrap();
    store
        .connection
        .execute("UPDATE account SET used_bytes=used_bytes+1", [])
        .unwrap();
    assert_eq!(store.check_integrity().unwrap().quota_mismatches, 1);
    assert!(matches!(
        store.gc(apply(), |_| Ok(())),
        Err(StoreError::Integrity)
    ));
    store
        .connection
        .execute("UPDATE account SET used_bytes=used_bytes-1", [])
        .unwrap();
    store
        .connection
        .execute("UPDATE mailbox SET uidnext=1 WHERE name='INBOX'", [])
        .unwrap();
    assert_eq!(store.check_integrity().unwrap().uid_mismatches, 1);
    assert!(matches!(
        store.gc(apply(), |_| Ok(())),
        Err(StoreError::Integrity)
    ));
}

#[tokio::test]
async fn interrupted_gc_is_safe_to_rerun_without_replaying_an_old_plan() {
    for point in [
        FaultPoint::GcPlanned,
        FaultPoint::GcUnlinked,
        FaultPoint::GcDirectorySync,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("mail");
        let runtime = StorageRuntime::default().with_hook(move |at| {
            if at == point {
                Err(io::Error::other("fault"))
            } else {
                Ok(())
            }
        });
        let mut store = Store::open_with_runtime(&root, options(), runtime).unwrap();
        store.create_account(&account(), 10000).unwrap();
        let good = prepared(&store).await;
        store.accept(good, plan()).unwrap();
        let orphan = prepared(&store).await;
        let path = blob_path(&store.root, &orphan.id).unwrap();
        drop(orphan);
        old(&path);
        assert!(store.gc(apply(), |_| Ok(())).is_err());
        drop(store);
        let mut store = Store::open_existing(&root, options()).unwrap();
        let history = store.gc_history(10).unwrap();
        assert_eq!(history[0].status, "interrupted");
        assert_eq!(history[0].planned, 1);
        store.gc(apply(), |_| Ok(())).unwrap();
        assert!(!path.exists());
        assert!(store.check_integrity().unwrap().healthy());
        assert!(store.operation(OP).unwrap().is_some());
    }
}

#[tokio::test]
async fn recovery_accepts_only_a_matching_copy_and_never_overwrites() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("mail");
    let mut store = Store::open(&root, options()).unwrap();
    store.create_account(&account(), 10000).unwrap();
    let message = prepared(&store).await;
    store.accept(message, plan()).unwrap();
    let accepted = store.operation(OP).unwrap().unwrap();
    let path = blob_path(&store.root, &accepted.blob_id).unwrap();
    let source = directory.path().join("backup.eml");
    fs::write(&source, RAW).unwrap();
    assert!(matches!(
        store.recover_blob(&accepted.blob_id, &source),
        Err(StoreError::AlreadyExists)
    ));
    fs::remove_file(&path).unwrap();
    assert_eq!(store.check_integrity().unwrap().missing_blobs, 1);
    fs::write(&source, b"wrong").unwrap();
    assert!(matches!(
        store.recover_blob(&accepted.blob_id, &source),
        Err(StoreError::Integrity)
    ));
    assert!(!path.exists());
    fs::write(&source, RAW).unwrap();
    assert_eq!(
        store.recover_blob(&accepted.blob_id, &source).unwrap(),
        RAW.len() as u64
    );
    assert!(store.check_integrity().unwrap().healthy());
    assert_eq!(store.list_messages(&account(), 0, 10).unwrap()[0].uid, 1);
    assert_eq!(
        store.operation(OP).unwrap().unwrap().message_id,
        accepted.message_id
    );
    assert!(matches!(
        store.recover_blob("../../escape", &source),
        Err(StoreError::InvalidId)
    ));
}

fn legacy(root: &Path) {
    let store = Store::open(root, options()).unwrap();
    store.connection.execute_batch("DROP TABLE gc_action; DROP TABLE maintenance_run; DROP TABLE schema_migration; PRAGMA user_version=1;").unwrap();
}

#[test]
fn migration_is_atomic_adopts_only_the_exact_legacy_schema_and_checks_history() {
    for point in [FaultPoint::MigrationApplied, FaultPoint::MigrationCommitted] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("mail");
        legacy(&root);
        let runtime = StorageRuntime::default().with_hook(move |at| {
            if at == point {
                Err(io::Error::other("interrupted migration"))
            } else {
                Ok(())
            }
        });
        assert!(Store::open_with_runtime(&root, options(), runtime).is_err());
        {
            let connection = Connection::open(root.join("meta.sqlite")).unwrap();
            let version: u32 = connection
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(
                version,
                if point == FaultPoint::MigrationApplied {
                    1
                } else {
                    CURRENT_VERSION
                }
            );
            assert_eq!(migration::validate(&connection).unwrap(), version);
        }
        let store = Store::open_existing(&root, options()).unwrap();
        let history = store.migration_history().unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].adopted);
        assert!(!history[1].adopted);
        store
            .connection
            .execute(
                "UPDATE schema_migration SET sha256=?1 WHERE version=1",
                ["0".repeat(64)],
            )
            .unwrap();
        drop(store);
        assert!(matches!(
            Store::open_existing(&root, options()),
            Err(StoreError::SchemaVersion)
        ));
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("mail");
    legacy(&root);
    let connection = Connection::open(root.join("meta.sqlite")).unwrap();
    connection
        .execute("CREATE TABLE unrelated(id INTEGER)", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        Store::open_existing(&root, options()),
        Err(StoreError::SchemaVersion)
    ));
}

#[test]
fn inspection_never_initializes_a_missing_store() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("typo");
    assert!(matches!(
        Store::open_existing(&root, options()),
        Err(StoreError::NotFound)
    ));
    assert!(!root.exists());
}
