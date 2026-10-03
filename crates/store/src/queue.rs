//! Durable per-recipient relay responsibility. No sockets or automatic sending.
use crate::blob::{blob_path, valid_id};
use crate::verified::VerifiedReader;
use crate::{
    AcceptedMessage, FaultPoint, InstanceLock, PreparedMessage, StorageRuntime, Store, StoreError,
    unsigned_column,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use rustymail_core::Address;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    sync::{Arc, Weak},
};
use uuid::Uuid;

pub(crate) const BATCH: usize = 128;

struct StoredQueueAcceptance {
    id: String,
    sender: String,
    hash: String,
    size: u64,
    body: Option<String>,
    age: Option<i64>,
    imported: bool,
}
struct Candidate {
    id: String,
    message_id: String,
    recipient: String,
    attempts: i64,
    generation: i64,
    due: i64,
    expires: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum QueueBody {
    SevenBit,
    EightBitMime,
}
impl QueueBody {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SevenBit => "7bit",
            Self::EightBitMime => "8bitmime",
        }
    }
}

/// Trusted offline/internal API. Caller supplies the final immutable wire
/// representation. SMTP submission authorization is not implemented by this API.
pub struct QueuePlan {
    pub operation_id: String,
    pub sender: Option<Address>,
    pub recipients: Vec<Address>,
    pub body: QueueBody,
    pub max_age_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct QueuePolicy {
    pub concurrency: usize,
    pub per_domain: usize,
    pub lease_seconds: u64,
    pub retry_seconds: Vec<u64>,
    pub jitter_percent: u8,
}
impl Default for QueuePolicy {
    fn default() -> Self {
        Self {
            concurrency: 16,
            per_domain: 2,
            lease_seconds: 300,
            retry_seconds: vec![1800, 3600, 7200, 14400],
            jitter_percent: 20,
        }
    }
}
impl QueuePolicy {
    fn validate(&self) -> Result<(), StoreError> {
        if !(1..=BATCH).contains(&self.concurrency)
            || !(1..=self.concurrency).contains(&self.per_domain)
            || !(1..=3600).contains(&self.lease_seconds)
            || self.retry_seconds.is_empty()
            || self.retry_seconds.len() > 32
            || self.retry_seconds.iter().any(|n| !(1..=86400).contains(n))
            || self.jitter_percent > 50
        {
            return Err(StoreError::InvalidInput);
        }
        Ok(())
    }
    fn delay_ms(&self, id: &str, attempts: u64) -> i64 {
        let index = attempts
            .saturating_sub(1)
            .min(self.retry_seconds.len() as u64 - 1) as usize;
        let base = self.retry_seconds[index] * 1000;
        let spread = base * u64::from(self.jitter_percent) / 100;
        let mut digest = Sha256::new();
        digest.update(id);
        digest.update(attempts.to_be_bytes());
        let hash = digest.finalize();
        let random = u64::from_be_bytes(hash[..8].try_into().expect("eight bytes"));
        // Positive jitter keeps the configured minimum delay intact.
        (base + random % (spread + 1)) as i64
    }
}

#[derive(Debug, Serialize)]
pub struct QueueSummary {
    pub id: String,
    pub message_id: String,
    pub recipient: String,
    pub destination_domain: String,
    pub state: String,
    pub attempts: u64,
    pub generation: u64,
    pub next_attempt_at_ms: i64,
    pub expires_at_ms: i64,
    pub lease_until_ms: Option<i64>,
    pub last_smtp_code: Option<u16>,
    pub diagnostic: Option<String>,
    pub lifecycle_reason: Option<String>,
    pub possibly_delivered: bool,
    pub closed_at_ms: Option<i64>,
    pub notification_state: String,
    pub notification_error: Option<String>,
    pub notification_due_ms: i64,
    pub report_message_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueueAdminEvent {
    pub id: i64,
    pub delivery_id: String,
    pub action: String,
    pub before_state: String,
    pub after_state: String,
    pub note: String,
    pub allow_duplicate: bool,
    pub extend_expired: bool,
    pub created_at_ms: i64,
}
const SUMMARY: &str = "d.id,d.message_id,d.recipient,d.destination_domain,d.state,d.attempts,d.generation,d.next_attempt_at_ms,d.expires_at_ms,d.lease_until_ms,d.last_smtp_code,d.diagnostic,d.lifecycle_reason,d.possibly_delivered,d.closed_at_ms,d.notification_state,d.notification_error,d.notification_due_ms,(SELECT report_message_id FROM notification n WHERE n.delivery_id=d.id AND n.kind='failure')";
fn summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<QueueSummary> {
    Ok(QueueSummary {
        id: r.get(0)?,
        message_id: r.get(1)?,
        recipient: r.get(2)?,
        destination_domain: r.get(3)?,
        state: r.get(4)?,
        attempts: unsigned_column(r, 5)?,
        generation: unsigned_column(r, 6)?,
        next_attempt_at_ms: r.get(7)?,
        expires_at_ms: r.get(8)?,
        lease_until_ms: r.get(9)?,
        last_smtp_code: r.get(10)?,
        diagnostic: r.get(11)?,
        lifecycle_reason: r.get(12)?,
        possibly_delivered: r.get(13)?,
        closed_at_ms: r.get(14)?,
        notification_state: r.get(15)?,
        notification_error: r.get(16)?,
        notification_due_ms: r.get(17)?,
        report_message_id: r.get(18)?,
    })
}

struct LeaseGuard {
    _lock: Arc<InstanceLock>,
    domain: String,
}

/// Not serializable or forgeable by callers. Its lifetime keeps the process
/// lock and admission slot alive, including while a body reader still exists.
pub struct QueueLease {
    id: String,
    token: String,
    generation: i64,
    guard: Arc<LeaseGuard>,
    retry_ms: i64,
    message_id: String,
    recipient: Address,
    sender: Option<Address>,
    body: QueueBody,
    blob_id: String,
    stored_size: u64,
    submission: bool,
}
impl QueueLease {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn message_id(&self) -> &str {
        &self.message_id
    }
    pub fn recipient(&self) -> &Address {
        &self.recipient
    }
    pub fn sender(&self) -> Option<&Address> {
        self.sender.as_ref()
    }
    pub fn body(&self) -> QueueBody {
        self.body
    }
    pub fn stored_size(&self) -> u64 {
        self.stored_size
    }
    pub fn omitted_prefix(&self) -> String {
        if self.submission {
            format!(
                "Return-Path: <{}>\r\n",
                self.sender.as_ref().map_or("", Address::as_str)
            )
        } else {
            String::new()
        }
    }
}

pub struct QueuedReader {
    reader: VerifiedReader,
    _guard: Arc<LeaseGuard>,
}
impl Read for QueuedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

/// Only normalized classifications are persisted; arbitrary peer replies might
/// contain addresses, credentials or control sequences and never enter this API.
#[derive(Debug, Serialize)]
pub enum QueueResult {
    Delivered(u16),
    Temporary(u16),
    Permanent(u16),
    ConnectionLost,
    /// Local policy/content failure, without fabricating an SMTP reply code.
    Hold,
    /// Relay configuration, authentication or TLS failure; retain responsibility.
    Deferred,
}

#[derive(Default)]
pub(crate) struct QueueRuntime {
    active: BTreeMap<String, Weak<LeaseGuard>>,
    ready_cursor: Option<(i64, String)>,
    recovery_cursor: String,
    clock: Option<(i64, u64)>,
    last_wall: Option<i64>,
    clock_changed: bool,
}
impl QueueRuntime {
    pub(crate) fn active(&self) -> bool {
        self.active.values().any(|value| value.strong_count() > 0)
    }
    pub(crate) fn now(&mut self, runtime: &StorageRuntime) -> Result<i64, StoreError> {
        let wall = runtime.now_ms()?;
        let mono = runtime.monotonic_ms();
        if let Some((old_wall, old_mono)) = self.clock {
            let skew = (i128::from(wall) - i128::from(old_wall))
                - (i128::from(mono) - i128::from(old_mono));
            if self.last_wall.is_some_and(|old| wall < old)
                || mono < old_mono
                || skew.abs() > 60_000
            {
                self.clock_changed = true;
            }
        } else {
            self.clock = Some((wall, mono));
        }
        self.last_wall = Some(wall);
        if self.clock_changed {
            Err(StoreError::ClockChanged)
        } else {
            Ok(wall)
        }
    }
}

fn commit(transaction: Transaction<'_>, runtime: &StorageRuntime) -> Result<(), StoreError> {
    runtime.hit(FaultPoint::QueueBeforeCommit)?;
    transaction
        .commit()
        .map_err(|_| StoreError::QueueOutcomeUnknown)?;
    runtime
        .hit(FaultPoint::QueueAfterCommit)
        .map_err(|_| StoreError::QueueOutcomeUnknown)?;
    Ok(())
}
fn later(now: i64, milliseconds: i64) -> Result<i64, StoreError> {
    now.checked_add(milliseconds)
        .ok_or(StoreError::InvalidInput)
}
fn check_lease(
    connection: &rusqlite::Connection,
    lease: &QueueLease,
    lock: &Arc<InstanceLock>,
) -> Result<String, StoreError> {
    if !Arc::ptr_eq(lock, &lease.guard._lock) {
        return Err(StoreError::StaleLease);
    }
    connection.query_row("SELECT q.phase FROM delivery d JOIN queue_lease q ON q.delivery_id=d.id WHERE d.id=?1 AND d.route='relay' AND d.state='leased' AND d.lease_token=?2 AND d.generation=?3",
        params![lease.id,lease.token,lease.generation], |r| r.get(0)).optional()?.ok_or(StoreError::StaleLease)
}

pub(crate) fn insert_relay(
    tx: &Transaction<'_>,
    id: &str,
    recipients: &BTreeSet<String>,
    body: QueueBody,
    age: u64,
    now: i64,
) -> Result<(), StoreError> {
    let expires = later(now, (age * 1000) as i64)?;
    tx.execute(
        "INSERT INTO queue_message VALUES(?1,?2,?3)",
        params![id, body.name(), age as i64],
    )?;
    for recipient in recipients {
        let domain = recipient
            .rsplit_once('@')
            .ok_or(StoreError::InvalidInput)?
            .1;
        tx.execute("INSERT INTO delivery(id,message_id,recipient,route,destination_domain,state,next_attempt_at_ms,expires_at_ms) VALUES(?1,?2,?3,'relay',?4,'pending',?5,?6)",
            params![Uuid::new_v4().simple().to_string(),id,recipient,domain,now,expires])?;
    }
    Ok(())
}

impl Store {
    /// Atomically accept up to 100 remote responsibilities after blob durability.
    /// No local mailbox mutation, network access, or header rewriting occurs.
    pub fn enqueue(
        &mut self,
        message: PreparedMessage,
        plan: QueuePlan,
    ) -> Result<AcceptedMessage, StoreError> {
        if message.root != self.root
            || !valid_id(&plan.operation_id)
            || plan.recipients.is_empty()
            || plan.recipients.len() > 100
            || !(1..=604800).contains(&plan.max_age_seconds)
        {
            return Err(StoreError::InvalidInput);
        }
        // External local-parts are case-sensitive. Only Address's domain is normalized.
        let recipients: BTreeSet<String> = plan
            .recipients
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect();
        let sender = plan.sender.as_ref().map_or("", Address::as_str);
        let now = self.runtime.now_ms()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<StoredQueueAcceptance>=tx.query_row(
            "SELECT m.id,m.reverse_path,b.sha256,b.size_bytes,q.body_mode,q.max_age_seconds,m.source='import' FROM message m JOIN blob b ON b.id=m.blob_id LEFT JOIN queue_message q ON q.message_id=m.id WHERE m.ingest_key=?1",
            [&plan.operation_id], |r| Ok(StoredQueueAcceptance {id:r.get(0)?,sender:r.get(1)?,hash:r.get(2)?,size:unsigned_column(r,3)?,body:r.get(4)?,age:r.get(5)?,imported:r.get(6)?})).optional()?;
        if let Some(StoredQueueAcceptance {
            id,
            sender: old_sender,
            hash,
            size,
            body,
            age,
            imported,
        }) = previous
        {
            let old: BTreeSet<String> = tx
                .prepare("SELECT recipient FROM delivery WHERE message_id=?1 AND route='relay'")?
                .query_map([&id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            if !imported
                || old_sender != sender
                || hash != message.hash
                || size != message.size
                || old != recipients
                || body.as_deref() != Some(plan.body.name())
                || age != Some(plan.max_age_seconds as i64)
            {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(AcceptedMessage {
                message_id: id,
                already_committed: true,
            });
        }
        let id = Uuid::new_v4().simple().to_string();
        self.runtime.hit(FaultPoint::DatabaseWrite)?;
        tx.execute("INSERT INTO blob(id,size_bytes,sha256,mime_metadata,metadata_version,created_at_ms) VALUES(?1,?2,?3,?4,1,?5)",
            params![message.id,message.size as i64,message.hash,b"{}".as_slice(),now])?;
        tx.execute("INSERT INTO message(id,ingest_key,blob_id,source,reverse_path,accepted_at_ms) VALUES(?1,?2,?3,'import',?4,?5)",
            params![id,plan.operation_id,message.id,sender,now])?;
        insert_relay(&tx, &id, &recipients, plan.body, plan.max_age_seconds, now)?;
        self.runtime.hit(FaultPoint::BeforeCommit)?;
        self.runtime
            .hit(FaultPoint::Commit)
            .map_err(|_| StoreError::OutcomeUnknown)?;
        tx.commit().map_err(|_| StoreError::OutcomeUnknown)?;
        self.runtime
            .hit(FaultPoint::AfterCommit)
            .map_err(|_| StoreError::OutcomeUnknown)?;
        Ok(AcceptedMessage {
            message_id: id,
            already_committed: false,
        })
    }

    pub fn queue_list(
        &self,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<QueueSummary>, StoreError> {
        if (!after_id.is_empty() && !valid_id(after_id)) || !(1..=BATCH).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        Ok(self.connection.prepare(&format!("SELECT {SUMMARY} FROM delivery d INDEXED BY queue_list WHERE route='relay' AND d.id>?1 ORDER BY d.id LIMIT ?2"))?
            .query_map(params![after_id,limit as i64],summary)?.collect::<Result<_,_>>()?)
    }

    pub fn queue_show(&self, id: &str) -> Result<QueueSummary, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        self.connection
            .query_row(
                &format!("SELECT {SUMMARY} FROM delivery d WHERE d.id=?1"),
                [id],
                summary,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Separate expiry index: a distant next_attempt must not hide an expired job.
    pub fn queue_expire(&mut self, limit: usize) -> Result<usize, StoreError> {
        if !(1..=BATCH).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        let now = self.queue.now(&self.runtime)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids: Vec<String>=tx.prepare("SELECT id FROM delivery INDEXED BY queue_expiry WHERE route='relay' AND state IN ('pending','deferred') AND expires_at_ms<=?1 ORDER BY expires_at_ms,id LIMIT ?2")?
            .query_map(params![now,limit as i64],|r|r.get(0))?.collect::<Result<_,_>>()?;
        for id in &ids {
            tx.execute("UPDATE delivery SET state=CASE WHEN possibly_delivered=1 THEN 'uncertain' ELSE 'failed' END,lifecycle_reason='expired',diagnostic=CASE WHEN possibly_delivered=1 THEN 'expired retry; earlier outcome remains unknown' ELSE 'delivery lifetime exceeded' END,notification_due_ms=0 WHERE id=?1",[id])?;
        }
        if !ids.is_empty() {
            commit(tx, &self.runtime)?;
        }
        Ok(ids.len())
    }

    /// Recover only attempts whose actual owners/readers have gone away.
    /// Bounded cursor scan, retained across calls. Zero recovered does not prove
    /// the scan is complete: this batch may contain only still-active owners.
    pub fn queue_recover(&mut self, limit: usize) -> Result<usize, StoreError> {
        if !(1..=BATCH).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        self.queue.active.retain(|_, weak| weak.strong_count() > 0);
        let now = self.runtime.now_ms()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String,bool)>=tx.prepare("SELECT d.id,(q.phase='body' OR d.possibly_delivered=1) FROM delivery d INDEXED BY queue_recovery JOIN queue_lease q ON q.delivery_id=d.id WHERE d.route='relay' AND d.state='leased' AND d.id>?1 ORDER BY d.id LIMIT ?2")?
            .query_map(params![self.queue.recovery_cursor,limit as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
        let mut recovered = 0;
        for (id, unknown) in &rows {
            if self.queue.active.contains_key(id) {
                continue;
            }
            let state = if *unknown { "uncertain" } else { "deferred" };
            tx.execute("UPDATE delivery SET state=?2,lease_token=NULL,lease_until_ms=NULL,next_attempt_at_ms=?3,last_smtp_code=NULL,diagnostic='attempt owner disappeared',possibly_delivered=?4 WHERE id=?1",
                params![id,state,later(now,1_800_000)?,unknown])?;
            tx.execute("DELETE FROM queue_lease WHERE delivery_id=?1", [id])?;
            recovered += 1;
        }
        if recovered > 0 {
            commit(tx, &self.runtime)?;
        } else {
            tx.rollback()?;
        }
        self.queue.recovery_cursor = if rows.len() == limit {
            rows.last().expect("nonempty").0.clone()
        } else {
            String::new()
        };
        Ok(recovered)
    }

    pub fn queue_claim(
        &mut self,
        policy: &QueuePolicy,
        limit: usize,
    ) -> Result<Vec<QueueLease>, StoreError> {
        policy.validate()?;
        if !(1..=BATCH).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        let now = self.queue.now(&self.runtime)?;
        self.queue_recover(BATCH)?;
        self.queue_expire(BATCH)?;
        let mut domains = BTreeMap::<String, usize>::new();
        for weak in self.queue.active.values() {
            if let Some(guard) = weak.upgrade() {
                *domains.entry(guard.domain.clone()).or_default() += 1;
            }
        }
        let available = policy
            .concurrency
            .saturating_sub(self.queue.active.len())
            .min(limit);
        if available == 0 {
            return Ok(Vec::new());
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor = self
            .queue
            .ready_cursor
            .clone()
            .unwrap_or((-1, String::new()));
        let candidates: Vec<Candidate>=tx.prepare(
            "SELECT id,message_id,recipient,attempts,generation,next_attempt_at_ms,expires_at_ms FROM delivery INDEXED BY queue_ready WHERE route='relay' AND state IN ('pending','deferred') AND next_attempt_at_ms<=?1 AND (next_attempt_at_ms,id)>(?2,?3) ORDER BY next_attempt_at_ms,id LIMIT 128")?
            .query_map(params![now,cursor.0,cursor.1],|r|Ok(Candidate {id:r.get(0)?,message_id:r.get(1)?,recipient:r.get(2)?,attempts:r.get(3)?,generation:r.get(4)?,due:r.get(5)?,expires:r.get(6)?}))?.collect::<Result<_,_>>()?;
        let mut leases = Vec::with_capacity(available);
        let mut last = None;
        for Candidate {
            id,
            message_id,
            recipient,
            attempts,
            generation,
            due,
            expires,
        } in &candidates
        {
            last = Some((*due, id.clone()));
            if self.queue.active.contains_key(id) {
                continue;
            }
            if *expires <= now {
                // The separate bounded expiry batch may not yet have reached this row.
                continue;
            }
            if *attempts == i64::MAX || *generation == i64::MAX {
                tx.execute("UPDATE delivery SET state='hold',lifecycle_reason='counter',diagnostic='attempt counter exhausted' WHERE id=?1",[id])?;
                continue;
            }
            let recipient = Address::parse(recipient).map_err(|_| StoreError::Integrity)?;
            if domains.get(recipient.domain()).copied().unwrap_or(0) >= policy.per_domain {
                continue;
            }
            let (sender,body,blob_id,stored_size,submission):(String,String,String,u64,bool)=tx.query_row(
                "SELECT m.reverse_path,q.body_mode,m.blob_id,b.size_bytes,m.source='submission' FROM message m JOIN queue_message q ON q.message_id=m.id JOIN blob b ON b.id=m.blob_id WHERE m.id=?1",[message_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,unsigned_column(r,3)?,r.get(4)?)))?;
            let sender = if sender.is_empty() {
                None
            } else {
                Some(Address::parse(&sender).map_err(|_| StoreError::Integrity)?)
            };
            let body = match body.as_str() {
                "7bit" => QueueBody::SevenBit,
                "8bitmime" => QueueBody::EightBitMime,
                _ => return Err(StoreError::Integrity),
            };
            let token = Uuid::new_v4().simple().to_string();
            tx.execute("UPDATE delivery SET state='leased',attempts=attempts+1,generation=generation+1,lease_token=?2,lease_until_ms=?3 WHERE id=?1",params![id,token,later(now,(policy.lease_seconds*1000) as i64)?])?;
            tx.execute("INSERT INTO queue_lease VALUES(?1,'ready')", [id])?;
            *domains.entry(recipient.domain().to_owned()).or_default() += 1;
            leases.push(QueueLease {
                id: id.clone(),
                token,
                generation: generation + 1,
                guard: Arc::new(LeaseGuard {
                    _lock: self.lock.clone(),
                    domain: recipient.domain().to_owned(),
                }),
                retry_ms: policy.delay_ms(id, (*attempts + 1) as u64),
                message_id: message_id.clone(),
                recipient,
                sender,
                body,
                blob_id,
                stored_size,
                submission,
            });
            if leases.len() == available {
                break;
            }
        }
        commit(tx, &self.runtime)?;
        self.queue.ready_cursor = if candidates.len() < BATCH
            && last == candidates.last().map(|v| (v.due, v.id.clone()))
        {
            None
        } else {
            last
        };
        for lease in &leases {
            self.queue
                .active
                .insert(lease.id.clone(), Arc::downgrade(&lease.guard));
        }
        Ok(leases)
    }

    pub fn queue_renew(&mut self, lease: &QueueLease, seconds: u64) -> Result<(), StoreError> {
        if !(1..=3600).contains(&seconds) {
            return Err(StoreError::InvalidInput);
        }
        let now = self.queue.now(&self.runtime)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_lease(&tx, lease, &self.lock)?;
        tx.execute(
            "UPDATE delivery SET lease_until_ms=max(lease_until_ms,?2) WHERE id=?1",
            params![lease.id, later(now, (seconds * 1000) as i64)?],
        )?;
        commit(tx, &self.runtime)
    }

    /// Persist BEFORE writing any DATA body bytes to the remote peer. A crash
    /// after this point is conservatively uncertain, even before the final dot.
    pub fn queue_mark_body(&mut self, lease: &QueueLease) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_lease(&tx, lease, &self.lock)?;
        tx.execute(
            "UPDATE queue_lease SET phase='body' WHERE delivery_id=?1",
            [&lease.id],
        )?;
        commit(tx, &self.runtime)
    }

    pub fn queue_open_body(&self, lease: &QueueLease) -> Result<QueuedReader, StoreError> {
        check_lease(&self.connection, lease, &self.lock)?;
        let path = blob_path(&self.root, &lease.blob_id)?;
        let (expected_size, expected_hash) = self.connection.query_row(
            "SELECT size_bytes,sha256 FROM blob WHERE id=?1",
            [&lease.blob_id],
            |r| Ok((unsigned_column(r, 0)?, r.get(1)?)),
        )?;
        Ok(QueuedReader {
            reader: VerifiedReader::open(&path, expected_size, expected_hash)?,
            _guard: lease.guard.clone(),
        })
    }

    /// Call only after the actual network attempt has stopped. Durable token
    /// and generation fencing still apply even to a late success response.
    pub fn queue_finish(
        &mut self,
        lease: QueueLease,
        outcome: QueueResult,
    ) -> Result<(), StoreError> {
        let now = self.runtime.now_ms()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let phase = check_lease(&tx, &lease, &self.lock)?;
        let was_unknown: bool = tx.query_row(
            "SELECT possibly_delivered FROM delivery WHERE id=?1",
            [&lease.id],
            |r| r.get(0),
        )?;
        let reason = match &outcome {
            QueueResult::Permanent(_) => Some("smtp_permanent"),
            QueueResult::Hold => Some("content"),
            _ => None,
        };
        let (mut state, code, mut diagnostic) = match outcome {
            QueueResult::Delivered(code) if (200..300).contains(&code) && phase == "body" => {
                ("delivered", Some(code), "remote accepted")
            }
            QueueResult::Temporary(code) if (400..500).contains(&code) => {
                ("deferred", Some(code), "temporary SMTP rejection")
            }
            QueueResult::Permanent(code) if (500..600).contains(&code) => {
                ("failed", Some(code), "permanent SMTP rejection")
            }
            QueueResult::ConnectionLost if phase == "body" => {
                ("uncertain", None, "connection lost after body boundary")
            }
            QueueResult::ConnectionLost => {
                ("deferred", None, "connection lost before body boundary")
            }
            QueueResult::Hold => ("hold", None, "local outbound content or capability failure"),
            QueueResult::Deferred if phase == "ready" => (
                "deferred",
                None,
                "relay setup or authentication unavailable",
            ),
            _ => return Err(StoreError::InvalidInput),
        };
        if was_unknown && state != "delivered" {
            state = "uncertain";
            diagnostic = "retry ended; earlier outcome remains unknown";
        }
        // A clock discontinuity must not prevent recording a known remote result.
        tx.execute("UPDATE delivery SET state=?2,lease_token=NULL,lease_until_ms=NULL,last_smtp_code=?3,diagnostic=?4,next_attempt_at_ms=?5,lifecycle_reason=?6,possibly_delivered=?7 WHERE id=?1",
            params![lease.id,state,code,diagnostic,now.saturating_add(lease.retry_ms),reason,state=="uncertain"])?;
        tx.execute("DELETE FROM queue_lease WHERE delivery_id=?1", [&lease.id])?;
        commit(tx, &self.runtime)?;
        // Readers can keep the guard alive after finish; capacity remains held
        // until their real I/O ends. Weak entries are pruned on the next claim.
        Ok(())
    }

    pub fn queue_hold(&mut self, id: &str) -> Result<(), StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let now = self.runtime.now_ms()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: String = tx
            .query_row(
                "SELECT state FROM delivery WHERE id=?1 AND route='relay'",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if tx.execute("UPDATE delivery SET state='hold',lifecycle_reason='manual',possibly_delivered=(possibly_delivered=1 OR state='uncertain'),diagnostic='held by administrator' WHERE id=?1 AND route='relay' AND closed_at_ms IS NULL AND state IN ('pending','deferred','uncertain','hold')",[id])?!=1 {
            return Err(StoreError::InvalidInput);
        }
        admin_event(
            &tx,
            id,
            "hold",
            &old,
            "hold",
            "operator requested hold",
            false,
            false,
            now,
        )?;
        commit(tx, &self.runtime)
    }

    pub fn queue_retry(
        &mut self,
        id: &str,
        allow_duplicate: bool,
        extend_expired: bool,
    ) -> Result<(), StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let now = self.queue.now(&self.runtime)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state,expires,age,closed):(String,i64,i64,Option<i64>)=tx.query_row("SELECT d.state,d.expires_at_ms,q.max_age_seconds,d.closed_at_ms FROM delivery d JOIN queue_message q ON q.message_id=d.message_id WHERE d.id=?1 AND d.route='relay'",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or(StoreError::NotFound)?;
        // A hold can originate from uncertainty. The state alone cannot prove
        // no remote acceptance, so require duplicate consent for either state.
        if !matches!(state.as_str(), "deferred" | "uncertain" | "hold")
            || closed.is_some()
            || ((state == "uncertain" || state == "hold") && !allow_duplicate)
            || (expires <= now && !extend_expired)
        {
            return Err(StoreError::InvalidInput);
        }
        let expires = if expires <= now {
            later(now, age * 1000)?
        } else {
            expires
        };
        tx.execute("UPDATE delivery SET state='pending',next_attempt_at_ms=?2,expires_at_ms=?3,lifecycle_reason=NULL,diagnostic='explicit administrator retry' WHERE id=?1",params![id,now,expires])?;
        admin_event(
            &tx,
            id,
            "retry",
            &state,
            "pending",
            "operator requested retry",
            allow_duplicate,
            extend_expired,
            now,
        )?;
        commit(tx, &self.runtime)
    }

    /// Close the operational obligation without claiming delivery or non-delivery.
    pub fn queue_close_unknown(&mut self, id: &str, note: &str) -> Result<(), StoreError> {
        if !valid_id(id)
            || note.is_empty()
            || note.len() > 512
            || !note.bytes().all(|b| (32..=126).contains(&b))
        {
            return Err(StoreError::InvalidInput);
        }
        let now = self.runtime.now_ms()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: String = tx
            .query_row(
                "SELECT state FROM delivery WHERE id=?1 AND route='relay'",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if tx.execute("UPDATE delivery SET state='uncertain',closed_at_ms=?2,diagnostic='unknown outcome closed by administrator' WHERE id=?1 AND route='relay' AND state IN ('uncertain','hold') AND possibly_delivered=1 AND closed_at_ms IS NULL",params![id,now])?!=1 { return Err(StoreError::InvalidInput); }
        admin_event(
            &tx,
            id,
            "close_unknown",
            &old,
            "uncertain",
            note,
            false,
            false,
            now,
        )?;
        commit(tx, &self.runtime)
    }

    pub fn queue_history(
        &self,
        id: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<QueueAdminEvent>, StoreError> {
        if !valid_id(id) || after < 0 || !(1..=BATCH).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        Ok(self.connection.prepare("SELECT id,delivery_id,action,before_state,after_state,note,allow_duplicate,extend_expired,created_at_ms FROM queue_admin_event WHERE delivery_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?
            .query_map(params![id,after,limit as i64],|r|Ok(QueueAdminEvent{id:r.get(0)?,delivery_id:r.get(1)?,action:r.get(2)?,before_state:r.get(3)?,after_state:r.get(4)?,note:r.get(5)?,allow_duplicate:r.get(6)?,extend_expired:r.get(7)?,created_at_ms:r.get(8)?}))?.collect::<Result<_,_>>()?)
    }

    pub fn queue_report_deliveries(&self, id: &str) -> Result<Vec<QueueSummary>, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        Ok(self.connection.prepare(&format!("SELECT {SUMMARY} FROM delivery d WHERE message_id=(SELECT report_message_id FROM notification WHERE delivery_id=?1 AND kind='failure') ORDER BY recipient LIMIT 128"))?
            .query_map([id],summary)?.collect::<Result<_,_>>()?)
    }

    pub(crate) fn queue_integrity_mismatches(&self) -> Result<u64, StoreError> {
        Ok(self.connection.query_row("SELECT (SELECT count(*) FROM delivery d LEFT JOIN queue_message m ON m.message_id=d.message_id LEFT JOIN queue_lease q ON q.delivery_id=d.id WHERE (d.route='relay' AND (m.message_id IS NULL OR ((d.state='leased') != (q.delivery_id IS NOT NULL)) OR ((d.state='leased') != (d.lease_token IS NOT NULL)) OR (d.lease_token IS NOT NULL AND length(d.lease_token)!=32))) OR (d.route!='relay' AND q.delivery_id IS NOT NULL)) + (SELECT count(*) FROM queue_message q WHERE NOT EXISTS(SELECT 1 FROM delivery d WHERE d.message_id=q.message_id AND d.route='relay')) + (SELECT count(*) FROM delivery d WHERE ((d.notification_state='created') != EXISTS(SELECT 1 FROM notification n WHERE n.delivery_id=d.id AND n.kind='failure')) OR (d.closed_at_ms IS NOT NULL AND (d.state!='uncertain' OR d.possibly_delivered!=1)) OR (d.state='failed' AND d.possibly_delivered=1))",[],|r|unsigned_column(r,0))?)
    }
}

#[allow(clippy::too_many_arguments)]
fn admin_event(
    tx: &Transaction<'_>,
    id: &str,
    action: &str,
    old: &str,
    new: &str,
    note: &str,
    duplicate: bool,
    extend: bool,
    now: i64,
) -> Result<(), StoreError> {
    tx.execute("INSERT INTO queue_admin_event(delivery_id,action,before_state,after_state,note,allow_duplicate,extend_expired,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![id,action,old,new,note,duplicate,extend,now])?;
    Ok(())
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
