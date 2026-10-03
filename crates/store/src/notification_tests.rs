use super::*;
use crate::{
    Acceptance, Clock, QueuePlan, QueuePolicy, QueueResult, RelayAcceptance, StoreOptions,
    SubmissionIdentity,
};
use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
};

struct TestClock {
    wall: AtomicI64,
    mono: AtomicU64,
}
impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            wall: AtomicI64::new(1_700_000_000_000),
            mono: AtomicU64::new(0),
        })
    }
    fn advance(&self, n: u64) {
        self.wall.fetch_add(n as i64, Ordering::SeqCst);
        self.mono.fetch_add(n, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.wall.load(Ordering::SeqCst))
    }
    fn monotonic_ms(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }
}
fn address(s: &str) -> Address {
    Address::parse(s).unwrap()
}
fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn policy() -> NotificationPolicy {
    NotificationPolicy {
        hostname: "mail.example.com".into(),
        local_domains: vec!["example.com".into()],
        max_age_seconds: 86400,
    }
}
fn scalar(store: &Store, sql: &str) -> i64 {
    store.connection.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn schema_three_retry_history_is_conservatively_preserved_as_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path(), options()).unwrap();
    seed(
        &mut store,
        "alice@example.com",
        &["target@remote.test"],
        86400,
    )
    .await;
    fail_all(&mut store);
    crate::migration::legacy_lab_fixture(&mut store.connection, 3).unwrap();
    // An old retry could have followed either a known failure or an unknown
    // final reply. Schema 3 retained neither the cause nor operator history.
    store
        .connection
        .execute("UPDATE delivery SET attempts=2 WHERE route='relay'", [])
        .unwrap();
    drop(store);
    let mut store = reopen(dir.path()).await;
    let row = store.queue_list("", 1).unwrap().pop().unwrap();
    assert_eq!(row.state, "uncertain");
    assert!(row.possibly_delivered);
    assert!(matches!(
        store.notification_next(&policy()).unwrap(),
        NotificationWork::Empty
    ));
    assert!(store.check_integrity().unwrap().healthy());
}
async fn seed(store: &mut Store, sender: &str, recipients: &[&str], age: u64) -> String {
    let login = address("alice@example.com");
    let sender = address(sender);
    if !store.recipient_exists(&login).unwrap() {
        store.create_account(&login, 100_000).unwrap();
    }
    store.set_send_as(&login, &sender, true).unwrap();
    store.set_send_as(&login, &login, true).unwrap();
    let selector = "12345678123456781234567812345678";
    if store.credential_lookup(&login, selector).unwrap().is_none() {
        store
            .create_credential(&login, selector, "lab", "mail", "$argon2id$v=19$fixture")
            .unwrap();
    }
    let principal = store
        .credential_lookup(&login, selector)
        .unwrap()
        .unwrap()
        .principal;
    let mut stage = store.stage().unwrap();
    stage.append(format!("Return-Path: <{}>\r\nFrom: forged-header@private.test\r\nBcc: private@hidden.test\r\nSubject: SECRET-CONTENT\r\n\r\nSECRET-BODY\r\n",sender.as_str()).as_bytes()).await.unwrap();
    store
        .accept_relay_submission(
            stage.prepare().await.unwrap(),
            Acceptance {
                operation_id: Uuid::new_v4().simple().to_string(),
                sender: Some(sender),
                recipients: vec![],
            },
            SubmissionIdentity {
                principal,
                author: login,
            },
            RelayAcceptance {
                recipients: recipients.iter().map(|r| address(r)).collect(),
                body: QueueBody::SevenBit,
                max_age_seconds: age,
            },
        )
        .unwrap()
        .message_id
}
fn fail_all(store: &mut Store) {
    let leases = store.queue_claim(&QueuePolicy::default(), 128).unwrap();
    assert!(!leases.is_empty());
    for lease in leases {
        store
            .queue_finish(lease, QueueResult::Permanent(550))
            .unwrap();
    }
}
fn next(store: &mut Store) -> Box<NotificationTask> {
    match store.notification_next(&policy()).unwrap() {
        NotificationWork::Prepare(t) => t,
        _ => panic!("missing report"),
    }
}
async fn notify(store: &mut Store) -> AcceptedMessage {
    let p = next(store).prepare().await.unwrap();
    store.notification_commit(p).unwrap()
}
async fn reopen(root: &Path) -> Store {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match Store::open_existing(root, options()) {
            Ok(s) => return s,
            Err(StoreError::Locked) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[tokio::test]
async fn local_report_is_private_atomic_and_idempotent_under_repeated_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    seed(
        &mut store,
        "alice@example.com",
        &["Case@remote.test", "other@other.test"],
        86400,
    )
    .await;
    let mut leases = store.queue_claim(&QueuePolicy::default(), 128).unwrap();
    for lease in leases.drain(..) {
        if lease.recipient().domain() == "remote.test" {
            store
                .queue_finish(lease, QueueResult::Permanent(550))
                .unwrap();
        } else {
            store.queue_mark_body(&lease).unwrap();
            store
                .queue_finish(lease, QueueResult::Delivered(250))
                .unwrap();
        }
    }
    let first = next(&mut store).prepare().await.unwrap();
    let second = next(&mut store).prepare().await.unwrap();
    let report = store.notification_commit(first).unwrap();
    let repeated = store.notification_commit(second).unwrap();
    assert!(repeated.already_committed);
    assert_eq!(report.message_id, repeated.message_id);
    assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 1);
    assert_eq!(scalar(&store, "SELECT count(*) FROM mailbox_message"), 1);
    assert_eq!(
        scalar(
            &store,
            "SELECT count(*) FROM message WHERE source='dsn' AND reverse_path=''"
        ),
        1
    );
    let mut bytes = Vec::new();
    store
        .export(
            &address("alice@example.com"),
            &report.message_id,
            &mut bytes,
        )
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    for private in [
        "SECRET",
        "Bcc:",
        "private@hidden.test",
        "forged-header@private.test",
        "other@other.test",
    ] {
        assert!(!text.contains(private));
    }
    assert!(
        text.contains(
            "Final-Recipient: rfc822; Case@remote.test\r\nAction: failed\r\nStatus: 5.0.0"
        )
    );
    assert!(text.starts_with("Return-Path: <>\r\n"));
    assert!(text.len() < MAX_REPORT);
    let failed = store
        .queue_list("", 128)
        .unwrap()
        .into_iter()
        .find(|q| q.state == "failed")
        .unwrap();
    assert!(store.queue_retry(&failed.id, true, true).is_err());
    assert_eq!(
        store.queue_report_deliveries(&failed.id).unwrap()[0].state,
        "delivered"
    );
    assert!(matches!(
        store.notification_next(&policy()).unwrap(),
        NotificationWork::Empty
    ));
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn quota_is_checked_before_file_creation_and_again_at_commit_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options(),
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    seed(
        &mut store,
        "alice@example.com",
        &["fail@remote.test"],
        86400,
    )
    .await;
    fail_all(&mut store);
    store
        .set_account_quota(&address("alice@example.com"), 0)
        .unwrap();
    let files = std::fs::read_dir(store.root.join("blobs")).unwrap().count();
    assert!(matches!(
        store.notification_next(&policy()),
        Err(StoreError::Quota)
    ));
    assert_eq!(
        std::fs::read_dir(store.root.join("blobs")).unwrap().count(),
        files
    );
    let row = store.queue_list("", 128).unwrap().pop().unwrap();
    assert_eq!(row.notification_error.as_deref(), Some("recipient_quota"));
    assert!(matches!(
        store.notification_next(&policy()).unwrap(),
        NotificationWork::Empty
    ));
    store
        .set_account_quota(&address("alice@example.com"), 100_000)
        .unwrap();
    clock.advance(60_000);
    let task = next(&mut store).prepare().await.unwrap();
    store
        .set_account_quota(&address("alice@example.com"), 0)
        .unwrap();
    let e = store.notification_commit(task).unwrap_err();
    assert!(matches!(e, StoreError::Quota));
    store.notification_failed(&row.id, &e).unwrap();
    assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 0);
    store
        .set_account_quota(&address("alice@example.com"), 100_000)
        .unwrap();
    clock.advance(60_000);
    notify(&mut store).await;
    assert_eq!(
        store
            .list_messages(&address("alice@example.com"), 0, 10)
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .set_account_quota(&address("alice@example.com"), 0)
            .is_err()
    );
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn disk_reservation_and_temporary_failure_do_not_lose_notification_duty() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let mut options = options();
    options.max_message_bytes = 16384;
    options.temporary_reserved_bytes = 16384;
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options,
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    seed(
        &mut store,
        "alice@example.com",
        &["fail@remote.test"],
        86400,
    )
    .await;
    fail_all(&mut store);
    let busy = store.stage().unwrap();
    assert!(matches!(
        store.notification_next(&policy()),
        Err(StoreError::DiskReserve)
    ));
    assert_eq!(
        store.queue_list("", 1).unwrap()[0].notification_state,
        "pending"
    );
    drop(busy);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while *store.reserved_bytes.lock().unwrap() != 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    clock.advance(60_000);
    notify(&mut store).await;
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn notification_faults_leave_zero_or_one_link_and_replay_without_duplicate_uid() {
    for boundary in [
        FaultPoint::FileSync,
        FaultPoint::DirectoriesSynced,
        FaultPoint::NotificationPrepared,
        FaultPoint::DatabaseWrite,
        FaultPoint::NotificationBeforeCommit,
        FaultPoint::NotificationAfterCommit,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mail");
        let armed = Arc::new(AtomicBool::new(false));
        let trigger = armed.clone();
        let runtime = StorageRuntime::default().with_hook(move |point| {
            if point == boundary && trigger.swap(false, Ordering::SeqCst) {
                Err(io::Error::new(io::ErrorKind::StorageFull, "report fault"))
            } else {
                Ok(())
            }
        });
        let mut store = Store::open_with_runtime(&root, options(), runtime).unwrap();
        seed(
            &mut store,
            "alice@example.com",
            &["fail@remote.test"],
            86400,
        )
        .await;
        fail_all(&mut store);
        let task = next(&mut store);
        armed.store(true, Ordering::SeqCst);
        let result = match task.prepare().await {
            Ok(p) => store.notification_commit(p),
            Err(e) => Err(e),
        };
        assert!(result.is_err(), "{boundary:?}");
        assert!(!armed.load(Ordering::SeqCst));
        drop(store);
        let mut store = reopen(&root).await;
        let committed = boundary == FaultPoint::NotificationAfterCommit;
        assert_eq!(
            scalar(&store, "SELECT count(*) FROM notification"),
            i64::from(committed)
        );
        assert_eq!(
            scalar(&store, "SELECT count(*) FROM mailbox_message"),
            i64::from(committed)
        );
        if !committed {
            notify(&mut store).await;
        }
        assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 1);
        assert_eq!(scalar(&store, "SELECT count(*) FROM mailbox_message"), 1);
        assert!(store.check_integrity().unwrap().healthy());
    }
}

#[tokio::test]
async fn remote_reports_have_null_sender_and_never_generate_another_report() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    seed(&mut store, "Owner@remote.test", &["fail@other.test"], 86400).await;
    fail_all(&mut store);
    let report = notify(&mut store).await;
    let lease = store
        .queue_claim(&QueuePolicy::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(lease.message_id(), report.message_id);
    assert!(lease.sender().is_none());
    assert_eq!(lease.recipient().as_str(), "Owner@remote.test");
    assert!(lease.omitted_prefix().is_empty());
    store
        .queue_finish(lease, QueueResult::Permanent(550))
        .unwrap();
    assert!(matches!(
        store.notification_next(&policy()).unwrap(),
        NotificationWork::Suppressed
    ));
    assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 1);
    let dsn = store
        .queue_list("", 128)
        .unwrap()
        .into_iter()
        .find(|r| r.message_id == report.message_id)
        .unwrap();
    assert_eq!(
        dsn.notification_error.as_deref(),
        Some("dsn_loop_suppressed")
    );
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn import_and_null_sender_are_suppressed_without_reading_original_headers() {
    for sender in [None, Some(address("forged@remote.test"))] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
        let mut stage = store.stage().unwrap();
        stage
            .append(b"From: attacker@remote.test\r\n\r\nbody\r\n")
            .await
            .unwrap();
        store
            .enqueue(
                stage.prepare().await.unwrap(),
                QueuePlan {
                    operation_id: Uuid::new_v4().simple().to_string(),
                    sender,
                    recipients: vec![address("x@other.test")],
                    body: QueueBody::SevenBit,
                    max_age_seconds: 86400,
                },
            )
            .unwrap();
        fail_all(&mut store);
        assert!(matches!(
            store.notification_next(&policy()).unwrap(),
            NotificationWork::Suppressed
        ));
        assert_eq!(scalar(&store, "SELECT count(*) FROM message"), 1);
        assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 0);
    }
}

#[tokio::test]
async fn expiry_is_independent_of_due_time_and_unknown_history_survives_retry_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options(),
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    seed(
        &mut store,
        "alice@example.com",
        &["later@one.test", "held@two.test", "lost@three.test"],
        10,
    )
    .await;
    let rows = store.queue_list("", 128).unwrap();
    let later = rows
        .iter()
        .find(|r| r.recipient.starts_with("later"))
        .unwrap();
    let held = rows
        .iter()
        .find(|r| r.recipient.starts_with("held"))
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE delivery SET next_attempt_at_ms=next_attempt_at_ms+86400000 WHERE id=?1",
            [&later.id],
        )
        .unwrap();
    store.queue_hold(&held.id).unwrap();
    let lease = store
        .queue_claim(&QueuePolicy::default(), 128)
        .unwrap()
        .pop()
        .unwrap();
    let lost = lease.id().to_owned();
    store.queue_mark_body(&lease).unwrap();
    store
        .queue_finish(lease, QueueResult::ConnectionLost)
        .unwrap();
    store.queue_retry(&lost, true, false).unwrap();
    let lease = store
        .queue_claim(&QueuePolicy::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    store
        .queue_finish(lease, QueueResult::Permanent(550))
        .unwrap();
    assert_eq!(store.queue_show(&lost).unwrap().state, "uncertain");
    clock.advance(10_000);
    assert_eq!(store.queue_expire(1).unwrap(), 1);
    assert_eq!(store.queue_show(&later.id).unwrap().state, "failed");
    assert_eq!(store.queue_show(&held.id).unwrap().state, "hold");
    let report = notify(&mut store).await;
    let mut bytes = Vec::new();
    store
        .export(
            &address("alice@example.com"),
            &report.message_id,
            &mut bytes,
        )
        .unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("Status: 5.4.7"));
    store
        .queue_close_unknown(&lost, "operator checked logs; delivery still unknown")
        .unwrap();
    assert!(store.queue_show(&lost).unwrap().closed_at_ms.is_some());
    assert!(store.queue_retry(&lost, true, true).is_err());
    assert!(store.queue_hold(&lost).is_err());
    let history = store.queue_history(&lost, 0, 128).unwrap();
    assert_eq!(history.len(), 2);
    assert!(history[0].allow_duplicate);
    assert_eq!(history[1].action, "close_unknown");
    assert!(store.check_integrity().unwrap().healthy());
    for (index, predicate, order) in [
        (
            "queue_expiry",
            "state IN ('pending','deferred') AND expires_at_ms<=100",
            "expires_at_ms,id",
        ),
        (
            "queue_notification",
            "state='failed' AND notification_state='pending' AND notification_due_ms<=100",
            "notification_due_ms,id",
        ),
    ] {
        let details:Vec<String>=store.connection.prepare(&format!("EXPLAIN QUERY PLAN SELECT id FROM delivery INDEXED BY {index} WHERE route='relay' AND {predicate} ORDER BY {order} LIMIT 128")).unwrap().query_map([],|r|r.get(3)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(details.iter().any(|d| d.contains(index)));
        assert!(!details.iter().any(|d| d.contains("TEMP B-TREE")));
    }
}

#[tokio::test]
async fn schema_three_upgrade_preserves_existing_notification_and_old_queue_atomically() {
    for fault in [FaultPoint::MigrationApplied, FaultPoint::MigrationCommitted] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mail");
        let mut store = Store::open(&root, options()).unwrap();
        seed(&mut store, "alice@example.com", &["first@one.test"], 86400).await;
        fail_all(&mut store);
        let report = notify(&mut store).await;
        seed(&mut store, "alice@example.com", &["second@two.test"], 86400).await;
        fail_all(&mut store);
        crate::legacy_lab_fixture(&mut store.connection, 3).unwrap();
        drop(store);
        let runtime = StorageRuntime::default().with_hook(move |p| {
            if p == fault {
                Err(io::Error::other("schema cut"))
            } else {
                Ok(())
            }
        });
        assert!(Store::open_with_runtime(&root, options(), runtime).is_err());
        let db = Connection::open(root.join("meta.sqlite")).unwrap();
        assert_eq!(
            crate::migration::validate(&db).unwrap(),
            if fault == FaultPoint::MigrationApplied {
                3
            } else {
                4
            }
        );
        drop(db);
        let mut store = reopen(&root).await;
        notify(&mut store).await;
        assert_eq!(scalar(&store, "SELECT count(*) FROM notification"), 2);
        let messages = store
            .list_messages(&address("alice@example.com"), 0, 10)
            .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message_id, report.message_id);
        assert_eq!(messages[0].uid, 1);
        assert!(matches!(
            store.notification_next(&policy()).unwrap(),
            NotificationWork::Empty
        ));
        assert!(store.check_integrity().unwrap().healthy());
    }
}
