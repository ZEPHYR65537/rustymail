use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SELECTOR: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const RAW:&[u8]=b"Return-Path: <alice@example.com>\r\nReceived: from lab\r\nFrom: alice@example.com\r\n\r\nbody\r\n";
fn address(value: &str) -> Address {
    Address::parse(value).unwrap()
}
fn options() -> StoreOptions {
    StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    }
}
fn identity(store: &mut Store) -> SubmissionIdentity {
    let alice = address("alice@example.com");
    store.create_account(&alice, 10000).unwrap();
    store.set_send_as(&alice, &alice, true).unwrap();
    // Storage validates authority, not the password hash; no authentication is
    // claimed by this fixture (independent SMTP tests perform the real hash).
    store
        .create_credential(&alice, SELECTOR, "lab", "mail", "$argon2id$v=19$fixture")
        .unwrap();
    SubmissionIdentity {
        principal: store
            .credential_lookup(&alice, SELECTOR)
            .unwrap()
            .unwrap()
            .principal,
        author: alice,
    }
}
fn local() -> Acceptance {
    Acceptance {
        operation_id: KEY.into(),
        sender: Some(address("alice@example.com")),
        recipients: vec![address("bob@example.com")],
    }
}
fn remote() -> RelayAcceptance {
    RelayAcceptance {
        recipients: vec![address("Case@remote.test"), address("case@REMOTE.TEST")],
        body: QueueBody::SevenBit,
        max_age_seconds: 432000,
    }
}
async fn prepared(store: &Store) -> PreparedMessage {
    let mut stage = store.stage().unwrap();
    stage.append(RAW).await.unwrap();
    stage.prepare().await.unwrap()
}

#[tokio::test]
async fn mixed_acceptance_is_atomic_replayable_and_authorized_at_commit() {
    for fault in [
        None,
        Some(FaultPoint::BeforeCommit),
        Some(FaultPoint::AfterCommit),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mail");
        let armed = Arc::new(AtomicBool::new(false));
        let hook = armed.clone();
        let runtime = StorageRuntime::default().with_hook(move |point| {
            if hook.load(Ordering::SeqCst) && Some(point) == fault {
                Err(std::io::Error::other("mixed cut"))
            } else {
                Ok(())
            }
        });
        let mut store = Store::open_with_runtime(&root, options(), runtime).unwrap();
        let identity = identity(&mut store);
        let bob = address("bob@example.com");
        store.create_account(&bob, 10000).unwrap();
        armed.store(true, Ordering::SeqCst);
        let result = store.accept_relay_submission(
            prepared(&store).await,
            local(),
            identity.clone(),
            remote(),
        );
        assert_eq!(result.is_ok(), fault.is_none());
        drop(store);
        let mut store = Store::open_existing(&root, options()).unwrap();
        let committed = fault != Some(FaultPoint::BeforeCommit);
        assert_eq!(
            store.list_messages(&bob, 0, 10).unwrap().len(),
            usize::from(committed)
        );
        assert_eq!(
            store.queue_list("", 128).unwrap().len(),
            if committed { 2 } else { 0 }
        );
        let accepted = store
            .accept_relay_submission(prepared(&store).await, local(), identity.clone(), remote())
            .unwrap();
        assert_eq!(accepted.already_committed, committed);
        assert_eq!(store.check_integrity().unwrap().referenced_blobs, 1);
        assert!(store.check_integrity().unwrap().healthy());
        assert!(matches!(
            store.accept_submission(prepared(&store).await, local(), identity.clone()),
            Err(StoreError::IdempotencyConflict)
        ));
        let leases = store.queue_claim(&QueuePolicy::default(), 128).unwrap();
        assert_eq!(leases.len(), 2);
        for lease in &leases {
            assert_eq!(lease.stored_size(), RAW.len() as u64);
            assert_eq!(
                lease.omitted_prefix(),
                "Return-Path: <alice@example.com>\r\n"
            );
        }
        store.revoke_credential(SELECTOR).unwrap();
        assert!(matches!(
            store.accept_relay_submission(prepared(&store).await, local(), identity, remote()),
            Err(StoreError::PermissionDenied)
        ));
    }
}

#[tokio::test]
async fn mixed_quota_failure_commits_neither_local_mail_nor_remote_responsibility() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("mail"), options()).unwrap();
    let identity = identity(&mut store);
    let bob = address("bob@example.com");
    store.create_account(&bob, 1).unwrap();
    assert!(matches!(
        store.accept_relay_submission(prepared(&store).await, local(), identity.clone(), remote()),
        Err(StoreError::Quota)
    ));
    assert!(store.list_messages(&bob, 0, 10).unwrap().is_empty());
    assert!(store.queue_list("", 128).unwrap().is_empty());
    // A remote-only submission still needs identity and a distinct operation.
    let mut plan = local();
    plan.recipients.clear();
    store
        .accept_relay_submission(prepared(&store).await, plan, identity, remote())
        .unwrap();
    let mut offline = crate::QueuePlan {
        operation_id: KEY.into(),
        sender: Some(address("alice@example.com")),
        recipients: remote().recipients,
        body: QueueBody::SevenBit,
        max_age_seconds: 432000,
    };
    assert!(matches!(
        store.enqueue(prepared(&store).await, offline),
        Err(StoreError::IdempotencyConflict)
    ));
    offline = crate::QueuePlan {
        operation_id: "cccccccccccccccccccccccccccccccc".into(),
        sender: None,
        recipients: vec![address("a@remote.test")],
        body: QueueBody::SevenBit,
        max_age_seconds: 432000,
    };
    store.enqueue(prepared(&store).await, offline).unwrap();
    assert!(store.check_integrity().unwrap().healthy());
}
