use super::*;
use crate::{Clock, GcOptions, StorageRuntime, StoreOptions};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

const RAW: &[u8] = b"From: sender@example.com\r\nSubject: queue lab\r\n\r\nimmutable\r\n";
struct TestClock {
    wall: AtomicI64,
    mono: AtomicU64,
}
impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            wall: AtomicI64::new(1_000_000),
            mono: AtomicU64::new(0),
        })
    }
    fn advance(&self, ms: u64) {
        self.wall.fetch_add(ms as i64, Ordering::SeqCst);
        self.mono.fetch_add(ms, Ordering::SeqCst);
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
fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn plan(id: &str, recipients: &[&str]) -> QueuePlan {
    QueuePlan {
        operation_id: id.into(),
        sender: None,
        recipients: recipients
            .iter()
            .map(|r| Address::parse(r).unwrap())
            .collect(),
        body: QueueBody::SevenBit,
        max_age_seconds: 86400,
    }
}
async fn prepared(store: &Store) -> PreparedMessage {
    let mut s = store.stage().unwrap();
    s.append(RAW).await.unwrap();
    s.prepare().await.unwrap()
}
async fn seed(store: &mut Store, recipients: &[&str]) -> String {
    let message = prepared(store).await;
    store
        .enqueue(
            message,
            plan(&Uuid::new_v4().simple().to_string(), recipients),
        )
        .unwrap()
        .message_id
}
fn policy() -> QueuePolicy {
    QueuePolicy {
        retry_seconds: vec![2, 4, 8],
        jitter_percent: 0,
        lease_seconds: 1,
        ..QueuePolicy::default()
    }
}

#[tokio::test]
async fn atomic_enqueue_preserves_external_case_and_replays_only_identical_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    let id = "11111111111111111111111111111111";
    let a = store
        .enqueue(
            prepared(&store).await,
            plan(id, &["A@remote.test", "a@REMOTE.TEST", "A@remote.test"]),
        )
        .unwrap();
    assert_eq!(store.queue_list("", 128).unwrap().len(), 2);
    let b = store
        .enqueue(
            prepared(&store).await,
            plan(id, &["a@remote.test", "A@remote.test"]),
        )
        .unwrap();
    assert!(b.already_committed);
    assert_eq!(a.message_id, b.message_id);
    assert!(matches!(
        store.enqueue(prepared(&store).await, plan(id, &["a@remote.test"])),
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.accept(
            prepared(&store).await,
            crate::Acceptance {
                operation_id: id.into(),
                sender: None,
                recipients: vec![Address::parse("a@remote.test").unwrap()]
            }
        ),
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(store.check_integrity().unwrap().healthy());
    store
        .gc(
            GcOptions {
                apply: true,
                min_age_seconds: 0,
                limit: 1000,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(store.check_integrity().unwrap().referenced_blobs, 1);
}

#[tokio::test]
async fn per_recipient_results_backoff_and_uncertainty_are_independent() {
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
        &["a@one.test", "b@two.test", "c@three.test", "d@four.test"],
    )
    .await;
    let leases = store.queue_claim(&policy(), 128).unwrap();
    assert_eq!(leases.len(), 4);
    for lease in leases {
        match lease.recipient().as_str() {
            "a@one.test" => {
                store.queue_mark_body(&lease).unwrap();
                store
                    .queue_finish(lease, QueueResult::Delivered(250))
                    .unwrap();
            }
            "b@two.test" => store
                .queue_finish(lease, QueueResult::Temporary(451))
                .unwrap(),
            "c@three.test" => store
                .queue_finish(lease, QueueResult::Permanent(550))
                .unwrap(),
            _ => {
                store.queue_mark_body(&lease).unwrap();
                store
                    .queue_finish(lease, QueueResult::ConnectionLost)
                    .unwrap();
            }
        }
    }
    let rows = store.queue_list("", 128).unwrap();
    let states: BTreeSet<_> = rows.iter().map(|r| r.state.as_str()).collect();
    assert_eq!(
        states,
        BTreeSet::from(["delivered", "deferred", "failed", "uncertain"])
    );
    assert!(store.queue_claim(&policy(), 128).unwrap().is_empty());
    clock.advance(2000);
    let retry = store.queue_claim(&policy(), 128).unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].recipient().as_str(), "b@two.test");
    store
        .queue_finish(
            retry.into_iter().next().unwrap(),
            QueueResult::Temporary(450),
        )
        .unwrap();
    assert_eq!(
        store
            .queue_list("", 128)
            .unwrap()
            .into_iter()
            .find(|r| r.recipient == "b@two.test")
            .unwrap()
            .next_attempt_at_ms,
        1_006_000
    );
    let uncertain = rows.iter().find(|r| r.state == "uncertain").unwrap();
    assert!(store.queue_retry(&uncertain.id, false, false).is_err());
    store.queue_hold(&uncertain.id).unwrap();
    assert!(store.queue_retry(&uncertain.id, false, false).is_err());
    store.queue_retry(&uncertain.id, true, false).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM notification", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn live_owner_and_reader_prevent_reclaim_gc_and_second_store_even_after_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("mail");
    let clock = TestClock::new();
    let mut store =
        Store::open_with_runtime(&root, options(), StorageRuntime::with_clock(clock.clone()))
            .unwrap();
    seed(&mut store, &["a@one.test"]).await;
    let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
    let mut reader = store.queue_open_body(&lease).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, RAW);
    clock.advance(2000);
    assert!(store.queue_claim(&policy(), 128).unwrap().is_empty());
    store.queue_renew(&lease, 2).unwrap();
    assert!(matches!(
        store.gc(GcOptions::default(), |_| Ok(())),
        Err(StoreError::MaintenanceBusy)
    ));
    drop(lease);
    assert_eq!(store.queue_recover(128).unwrap(), 0);
    drop(store);
    assert!(matches!(
        Store::open_existing(&root, options()),
        Err(StoreError::Locked)
    ));
    drop(reader);
    let mut store = Store::open_existing(&root, options()).unwrap();
    assert_eq!(store.queue_recover(128).unwrap(), 1);
    assert_eq!(store.queue_list("", 128).unwrap()[0].state, "deferred");
}

#[tokio::test]
async fn abandoned_body_is_uncertain_and_stale_generation_cannot_change_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    seed(&mut store, &["a@one.test"]).await;
    let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
    store.queue_mark_body(&lease).unwrap();
    // Fault fixture representing a late result from an obsolete generation.
    store
        .connection
        .execute(
            "UPDATE delivery SET generation=generation+1 WHERE id=?1",
            [lease.id()],
        )
        .unwrap();
    assert!(matches!(
        store.queue_finish(lease, QueueResult::Delivered(250)),
        Err(StoreError::StaleLease)
    ));
    assert_eq!(store.queue_recover(128).unwrap(), 1);
    assert_eq!(store.queue_list("", 128).unwrap()[0].state, "uncertain");
    assert!(store.queue_claim(&policy(), 128).unwrap().is_empty());
}

#[tokio::test]
async fn queue_transactions_recover_before_and_after_commit_without_false_success() {
    for boundary in [FaultPoint::QueueBeforeCommit, FaultPoint::QueueAfterCommit] {
        for action in ["claim", "body", "finish"] {
            let dir = tempfile::tempdir().unwrap();
            let armed = Arc::new(AtomicBool::new(false));
            let trigger = armed.clone();
            let runtime = StorageRuntime::default().with_hook(move |point| {
                if point == boundary && trigger.swap(false, Ordering::SeqCst) {
                    Err(io::Error::other("queue crash fixture"))
                } else {
                    Ok(())
                }
            });
            let mut store =
                Store::open_with_runtime(dir.path().join("mail"), options(), runtime).unwrap();
            seed(&mut store, &["a@one.test"]).await;
            if action == "claim" {
                armed.store(true, Ordering::SeqCst);
                assert!(store.queue_claim(&policy(), 1).is_err());
            } else {
                let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
                if action == "body" {
                    armed.store(true, Ordering::SeqCst);
                    assert!(store.queue_mark_body(&lease).is_err());
                    drop(lease);
                } else {
                    store.queue_mark_body(&lease).unwrap();
                    armed.store(true, Ordering::SeqCst);
                    assert!(
                        store
                            .queue_finish(lease, QueueResult::Delivered(250))
                            .is_err()
                    );
                }
            }
            assert!(!armed.load(Ordering::SeqCst));
            store.queue_recover(128).unwrap();
            let expected = match (action, boundary) {
                ("claim", FaultPoint::QueueBeforeCommit) => "pending",
                ("claim", _) | ("body", FaultPoint::QueueBeforeCommit) => "deferred",
                ("finish", FaultPoint::QueueAfterCommit) => "delivered",
                _ => "uncertain",
            };
            assert_eq!(
                store.queue_list("", 128).unwrap()[0].state,
                expected,
                "{action} {boundary:?}"
            );
            assert!(store.check_integrity().unwrap().healthy());
        }
    }
}

#[tokio::test]
async fn expired_tasks_hold_and_clock_steps_stop_claims_without_losing_known_results() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options(),
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    seed(&mut store, &["a@one.test"]).await;
    let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
    store.queue_mark_body(&lease).unwrap();
    clock.wall.fetch_add(90_000, Ordering::SeqCst);
    assert!(matches!(
        store.queue_claim(&policy(), 1),
        Err(StoreError::ClockChanged)
    ));
    store
        .queue_finish(lease, QueueResult::Delivered(250))
        .unwrap();
    assert_eq!(store.queue_list("", 128).unwrap()[0].state, "delivered");
    drop(store);
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options(),
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    seed(&mut store, &["b@two.test"]).await;
    clock.advance(86_400_001);
    assert!(store.queue_claim(&policy(), 1).unwrap().is_empty());
    let row = store
        .queue_list("", 128)
        .unwrap()
        .into_iter()
        .find(|r| r.state == "hold")
        .unwrap();
    assert!(store.queue_retry(&row.id, true, false).is_err());
    store.queue_retry(&row.id, true, true).unwrap();
    clock.wall.fetch_sub(1, Ordering::SeqCst);
    assert!(matches!(
        store.queue_claim(&policy(), 1),
        Err(StoreError::ClockChanged)
    ));
}

#[tokio::test]
async fn bounded_cursor_avoids_head_of_line_starvation_and_policy_caps_active_work() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let mut store = Store::open_with_runtime(
        dir.path().join("mail"),
        options(),
        StorageRuntime::with_clock(clock.clone()),
    )
    .unwrap();
    for _ in 0..130 {
        seed(&mut store, &["a@busy.test"]).await;
        clock.advance(1);
    }
    seed(&mut store, &["b@other.test"]).await;
    let policy = QueuePolicy {
        concurrency: 2,
        per_domain: 1,
        ..policy()
    };
    let first = store.queue_claim(&policy, 128).unwrap();
    assert_eq!(first.len(), 1);
    let second = store.queue_claim(&policy, 128).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].recipient().domain(), "other.test");
    assert!(store.queue_claim(&policy, 128).unwrap().is_empty());
    assert_eq!(store.queue_list("", 128).unwrap().len(), 128);
    assert!(store.queue_list("", 129).is_err());
    let details:Vec<String>=store.connection.prepare("EXPLAIN QUERY PLAN SELECT id FROM delivery INDEXED BY queue_ready WHERE route='relay' AND state IN ('pending','deferred') AND next_attempt_at_ms<=10000000 AND (next_attempt_at_ms,id)>(0,'') ORDER BY next_attempt_at_ms,id LIMIT 128").unwrap().query_map([],|r|r.get(3)).unwrap().collect::<Result<_,_>>().unwrap();
    assert!(details.iter().any(|s| s.contains("queue_ready")));
    assert!(!details.iter().any(|s| s.contains("TEMP B-TREE")));
}

#[tokio::test]
async fn queued_reader_detects_same_length_corruption_before_successful_eof() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    seed(&mut store, &["a@one.test"]).await;
    let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
    let mut changed = RAW.to_vec();
    changed[0] = b'X';
    std::fs::write(blob_path(&store.root, &lease.blob_id).unwrap(), changed).unwrap();
    let mut reader = store.queue_open_body(&lease).unwrap();
    assert_eq!(
        reader.read_to_end(&mut Vec::new()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn version_two_upgrade_is_atomic_and_preserves_existing_local_mail() {
    for boundary in [FaultPoint::MigrationApplied, FaultPoint::MigrationCommitted] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mail");
        let mut store = Store::open(&root, options()).unwrap();
        let address = Address::parse("alice@example.com").unwrap();
        store.create_account(&address, 10000).unwrap();
        let accepted = store
            .accept(
                prepared(&store).await,
                crate::Acceptance {
                    operation_id: "22222222222222222222222222222222".into(),
                    sender: None,
                    recipients: vec![address.clone()],
                },
            )
            .unwrap();
        store.connection.execute_batch("DROP INDEX queue_ready; DROP INDEX queue_recovery; DROP INDEX queue_list; DROP TABLE queue_lease; DROP TABLE queue_message; DELETE FROM schema_migration WHERE version=3; PRAGMA user_version=2;").unwrap();
        assert_eq!(crate::migration::validate(&store.connection).unwrap(), 2);
        drop(store);
        let runtime = StorageRuntime::default().with_hook(move |point| {
            if point == boundary {
                Err(io::Error::other("migration cut"))
            } else {
                Ok(())
            }
        });
        assert!(Store::open_with_runtime(&root, options(), runtime).is_err());
        let connection = rusqlite::Connection::open(root.join("meta.sqlite")).unwrap();
        assert_eq!(
            crate::migration::validate(&connection).unwrap(),
            if boundary == FaultPoint::MigrationApplied {
                2
            } else {
                3
            }
        );
        drop(connection);
        let store = Store::open_existing(&root, options()).unwrap();
        let messages = store.list_messages(&address, 0, 10).unwrap();
        assert_eq!(messages[0].uid, 1);
        assert_eq!(messages[0].message_id, accepted.message_id);
        assert!(store.check_integrity().unwrap().healthy());
        assert_eq!(store.migration_history().unwrap().len(), 3);
    }
}

#[test]
fn retry_jitter_and_configuration_have_finite_checked_bounds() {
    let p = QueuePolicy::default();
    p.validate().unwrap();
    for attempt in [1, 2, 3, 4, 100, u64::MAX] {
        let base = p.retry_seconds[(attempt - 1).min(3) as usize] * 1000;
        let delay = p.delay_ms("11111111111111111111111111111111", attempt) as u64;
        assert!((base..=base * 120 / 100).contains(&delay));
    }
    assert!(
        QueuePolicy {
            concurrency: 129,
            ..p.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        QueuePolicy {
            retry_seconds: vec![u64::MAX],
            ..p
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn enqueue_commit_faults_never_publish_partial_recipient_sets() {
    for boundary in [FaultPoint::BeforeCommit, FaultPoint::AfterCommit] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mail");
        let runtime = StorageRuntime::default().with_hook(move |point| {
            if point == boundary {
                Err(io::Error::other("enqueue cut"))
            } else {
                Ok(())
            }
        });
        let mut store = Store::open_with_runtime(&root, options(), runtime).unwrap();
        let operation = "33333333333333333333333333333333";
        let recipients = ["one@remote.test", "two@remote.test", "three@other.test"];
        let error = store
            .enqueue(prepared(&store).await, plan(operation, &recipients))
            .unwrap_err();
        assert_eq!(
            matches!(error, StoreError::OutcomeUnknown),
            boundary == FaultPoint::AfterCommit
        );
        drop(store);
        let mut store = Store::open_existing(&root, options()).unwrap();
        assert_eq!(
            store.queue_list("", 128).unwrap().len(),
            if boundary == FaultPoint::AfterCommit {
                3
            } else {
                0
            }
        );
        let accepted = store
            .enqueue(prepared(&store).await, plan(operation, &recipients))
            .unwrap();
        assert_eq!(
            accepted.already_committed,
            boundary == FaultPoint::AfterCommit
        );
        assert_eq!(store.queue_list("", 128).unwrap().len(), 3);
        assert!(store.check_integrity().unwrap().healthy());
    }
}

#[tokio::test]
async fn integrity_reports_cross_table_queue_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    seed(&mut store, &["target@remote.test"]).await;
    let lease = store.queue_claim(&policy(), 1).unwrap().pop().unwrap();
    // SQL foreign keys cannot express the bidirectional state/phase invariant.
    store
        .connection
        .execute("DELETE FROM queue_lease WHERE delivery_id=?1", [lease.id()])
        .unwrap();
    let report = store.check_integrity().unwrap();
    assert!(!report.healthy());
    assert_eq!(report.queue_mismatches, 1);
    assert!(matches!(
        store.queue_mark_body(&lease),
        Err(StoreError::StaleLease)
    ));
}
