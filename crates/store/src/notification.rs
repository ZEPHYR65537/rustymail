//! A failed delivery is a durable outbox item until a report is committed or
//! notification is explicitly suppressed. Network delivery is a separate duty.
use crate::{
    AcceptedMessage, FaultPoint, PreparedMessage, QueueBody, StagedMessage, StorageRuntime, Store,
    StoreError, blob, local_delivery, queue,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use rustymail_core::{Address, valid_domain};
use std::collections::BTreeSet;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use uuid::Uuid;

const MAX_REPORT: usize = 16384;
const RETRY_MS: i64 = 60_000;

#[derive(Clone)]
pub struct NotificationPolicy {
    pub hostname: String,
    pub local_domains: Vec<String>,
    pub max_age_seconds: u64,
}
impl NotificationPolicy {
    /// Validate before starting a worker; return the generated report author.
    pub fn validate(&self) -> Result<Address, StoreError> {
        if !valid_domain(&self.hostname)
            || self.local_domains.is_empty()
            || self.local_domains.len() > 100
            || self.local_domains.iter().any(|d| !valid_domain(d))
            || !(1..=604800).contains(&self.max_age_seconds)
        {
            return Err(StoreError::InvalidInput);
        }
        Address::parse(&format!("postmaster@{}", self.local_domains[0]))
            .map_err(|_| StoreError::InvalidInput)
    }
}

pub enum NotificationWork {
    Empty,
    Suppressed,
    Prepare(Box<NotificationTask>),
}

pub struct NotificationTask {
    plan: NotificationPlan,
    stage: StagedMessage,
    bytes: Vec<u8>,
    runtime: StorageRuntime,
}
impl NotificationTask {
    pub fn delivery_id(&self) -> &str {
        &self.plan.delivery_id
    }
    pub async fn prepare(mut self) -> Result<PreparedNotification, StoreError> {
        self.stage.append(&self.bytes).await?;
        let message = self.stage.prepare().await?;
        self.runtime.hit(FaultPoint::NotificationPrepared)?;
        Ok(PreparedNotification {
            plan: self.plan,
            message,
        })
    }
}
struct NotificationPlan {
    delivery_id: String,
    report_id: String,
    recipient: Address,
    local: bool,
    max_age_seconds: u64,
}
pub struct PreparedNotification {
    plan: NotificationPlan,
    message: PreparedMessage,
}
struct Failure {
    id: String,
    message_id: String,
    sender: String,
    recipient: String,
    source: String,
    authenticated: bool,
    reason: Option<String>,
    code: Option<u16>,
}

fn render(
    f: &Failure,
    plan: &NotificationPlan,
    policy: &NotificationPolicy,
    postmaster: &Address,
    now: i64,
) -> Result<Vec<u8>, StoreError> {
    let recipient = Address::parse(&f.recipient).map_err(|_| StoreError::Integrity)?;
    let date = OffsetDateTime::from_unix_timestamp(now / 1000)
        .map_err(|_| StoreError::InvalidInput)?
        .format(&Rfc2822)
        .map_err(|_| StoreError::InvalidInput)?;
    let expired = f.reason.as_deref() == Some("expired");
    let status = if expired { "5.4.7" } else { "5.0.0" };
    let diagnostic = if expired {
        "x-rustymail; delivery lifetime exceeded".to_owned()
    } else if let Some(code) = f.code.filter(|c| (500..600).contains(c)) {
        format!("smtp; {code} (remote text omitted)")
    } else {
        "x-rustymail; permanent delivery failure".to_owned()
    };
    let boundary = format!("rustymail-{}", plan.report_id);
    let local_trace = if plan.local {
        "Return-Path: <>\r\n"
    } else {
        ""
    };
    // All substituted fields are typed ASCII addresses, checked domains, UUIDs,
    // or generated diagnostic classes. No original header/body/peer text is used.
    let bytes=format!(concat!(
        "{local_trace}From: Mail Delivery Subsystem <{postmaster}>\r\n",
        "To: {target}\r\nDate: {date}\r\nMessage-ID: <{report}@{hostname}>\r\n",
        "Subject: Delivery failure\r\nAuto-Submitted: auto-generated\r\n",
        "X-Rustymail-Original-Message: {original}\r\nMIME-Version: 1.0\r\n",
        "Content-Type: multipart/report; report-type=delivery-status;\r\n boundary=\"{boundary}\"\r\n\r\n",
        "--{boundary}\r\nContent-Type: text/plain; charset=us-ascii\r\nContent-Transfer-Encoding: 7bit\r\n\r\n",
        "Delivery failed for {recipient}.\r\nOriginal acceptance ID: {original}\r\n",
        "Status: {status}. Other recipients, if any, have independent outcomes.\r\n",
        "Original headers and content are omitted for privacy.\r\n\r\n",
        "--{boundary}\r\nContent-Type: message/delivery-status\r\n\r\n",
        "Reporting-MTA: dns; {hostname}\r\n\r\nFinal-Recipient: rfc822; {recipient}\r\n",
        "Action: failed\r\nStatus: {status}\r\nDiagnostic-Code: {diagnostic}\r\n",
        "Final-Log-ID: {delivery}\r\n\r\n--{boundary}--\r\n"),
        local_trace=local_trace,postmaster=postmaster.as_str(),target=plan.recipient.as_str(),date=date,
        report=plan.report_id,hostname=policy.hostname,original=f.message_id,boundary=boundary,
        recipient=recipient.as_str(),status=status,diagnostic=diagnostic,delivery=f.id).into_bytes();
    if bytes.len() > MAX_REPORT {
        return Err(StoreError::SizeLimit);
    }
    Ok(bytes)
}

impl Store {
    /// Select ONE due failure using a partial index. Caller runs preparation
    /// outside the database owner; repeated calls cannot grow an in-memory list.
    pub fn notification_next(
        &mut self,
        policy: &NotificationPolicy,
    ) -> Result<NotificationWork, StoreError> {
        let postmaster = policy.validate()?;
        let now = self.queue.now(&self.runtime)?;
        let failure: Option<Failure>=self.connection.query_row(
            "SELECT d.id,m.id,m.reverse_path,d.recipient,m.source,m.authenticated_account_id IS NOT NULL,d.lifecycle_reason,d.last_smtp_code FROM delivery d INDEXED BY queue_notification JOIN message m ON m.id=d.message_id WHERE d.route='relay' AND d.state='failed' AND d.notification_state='pending' AND d.notification_due_ms<=?1 ORDER BY d.notification_due_ms,d.id LIMIT 1",
            [now],|r|Ok(Failure{id:r.get(0)?,message_id:r.get(1)?,sender:r.get(2)?,recipient:r.get(3)?,source:r.get(4)?,authenticated:r.get(5)?,reason:r.get(6)?,code:r.get(7)?})).optional()?;
        let Some(failure) = failure else {
            return Ok(NotificationWork::Empty);
        };
        let suppressed = if failure.source == "dsn" {
            Some("dsn_loop_suppressed")
        } else if failure.sender.is_empty() {
            Some("null_reverse_path")
        } else if failure.source != "submission" || !failure.authenticated {
            Some("source_not_eligible")
        } else {
            None
        };
        if let Some(reason) = suppressed {
            self.connection.execute("UPDATE delivery SET notification_state='suppressed',notification_error=?2 WHERE id=?1 AND notification_state='pending'",params![failure.id,reason])?;
            return Ok(NotificationWork::Suppressed);
        }
        let id = failure.id.clone();
        let result = (|| {
            let recipient = Address::parse(&failure.sender).map_err(|_| StoreError::Integrity)?;
            let local = policy
                .local_domains
                .iter()
                .any(|d| d.eq_ignore_ascii_case(recipient.domain()));
            let plan = NotificationPlan {
                delivery_id: id.clone(),
                report_id: Uuid::new_v4().simple().to_string(),
                recipient,
                local,
                max_age_seconds: policy.max_age_seconds,
            };
            let bytes = render(&failure, &plan, policy, &postmaster, now)?;
            if local {
                local_delivery::preflight(
                    &self.connection,
                    &plan.recipient.local_key(),
                    bytes.len() as u64,
                )?;
            }
            let mut options = self.options.clone();
            options.max_message_bytes = options.max_message_bytes.min(MAX_REPORT as u64);
            options.stream_buffer_bytes = options.stream_buffer_bytes.min(MAX_REPORT);
            if bytes.len() as u64 > options.max_message_bytes {
                return Err(StoreError::SizeLimit);
            }
            let stage = StagedMessage::create(
                self.root.clone(),
                self.lock.clone(),
                self.reserved_bytes.clone(),
                &options,
                self.runtime.clone(),
            )?;
            Ok(NotificationWork::Prepare(Box::new(NotificationTask {
                plan,
                stage,
                bytes,
                runtime: self.runtime.clone(),
            })))
        })();
        if let Err(error) = &result {
            self.notification_failed(&id, error)?;
        }
        result
    }

    /// Keep a bounded diagnostic and back off. If a commit actually succeeded,
    /// this conditional update cannot turn its created report back into pending.
    pub fn notification_failed(&mut self, id: &str, error: &StoreError) -> Result<(), StoreError> {
        if !blob::valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let reason = match error {
            StoreError::Quota => "recipient_quota",
            StoreError::RecipientUnavailable => "recipient_unavailable",
            StoreError::UidExhausted => "recipient_counter_exhausted",
            StoreError::DiskReserve => "disk_or_temporary_budget",
            StoreError::SizeLimit => "report_size_limit",
            _ => "storage_unavailable",
        };
        let due = self.queue.now(&self.runtime)?.saturating_add(RETRY_MS);
        self.connection.execute("UPDATE delivery SET notification_error=?2,notification_due_ms=?3 WHERE id=?1 AND state='failed' AND notification_state='pending'",params![id,reason,due])?;
        Ok(())
    }

    /// One transaction links failure, report, delivery, and any local UID/quota.
    /// PreparedNotification is opaque so callers cannot substitute arbitrary mail.
    pub fn notification_commit(
        &mut self,
        prepared: PreparedNotification,
    ) -> Result<AcceptedMessage, StoreError> {
        let PreparedNotification { plan, message } = prepared;
        if message.root != self.root {
            return Err(StoreError::InvalidInput);
        }
        let now = self.queue.now(&self.runtime)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id)=tx.query_row("SELECT report_message_id FROM notification WHERE delivery_id=?1 AND kind='failure'",[&plan.delivery_id],|r|r.get::<_,String>(0)).optional()? {
            return Ok(AcceptedMessage {message_id:id,already_committed:true});
        }
        let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM delivery d JOIN message m ON m.id=d.message_id WHERE d.id=?1 AND d.state='failed' AND d.route='relay' AND d.possibly_delivered=0 AND d.notification_state='pending' AND m.source='submission' AND m.authenticated_account_id IS NOT NULL AND m.reverse_path=?2)",params![plan.delivery_id,plan.recipient.as_str()],|r|r.get(0))?;
        if !eligible {
            return Err(StoreError::InvalidInput);
        }
        self.runtime.hit(FaultPoint::DatabaseWrite)?;
        tx.execute(
            "INSERT INTO blob(id,size_bytes,sha256,created_at_ms) VALUES(?1,?2,?3,?4)",
            params![message.id, message.size as i64, message.hash, now],
        )?;
        tx.execute("INSERT INTO message(id,ingest_key,blob_id,source,reverse_path,accepted_at_ms) VALUES(?1,?2,?3,'dsn','',?4)",params![plan.report_id,format!("dsn:{}",plan.delivery_id),message.id,now])?;
        if plan.local {
            local_delivery::deliver(
                &tx,
                &plan.recipient.local_key(),
                &plan.report_id,
                message.size,
                now,
            )?;
        } else {
            queue::insert_relay(
                &tx,
                &plan.report_id,
                &BTreeSet::from([plan.recipient.as_str().to_owned()]),
                QueueBody::SevenBit,
                plan.max_age_seconds,
                now,
            )?;
        }
        tx.execute("INSERT INTO notification(delivery_id,kind,report_message_id,created_at_ms) VALUES(?1,'failure',?2,?3)",params![plan.delivery_id,plan.report_id,now])?;
        tx.execute(
            "UPDATE delivery SET notification_state='created',notification_error=NULL WHERE id=?1",
            [&plan.delivery_id],
        )?;
        self.runtime.hit(FaultPoint::NotificationBeforeCommit)?;
        tx.commit().map_err(|_| StoreError::QueueOutcomeUnknown)?;
        self.runtime
            .hit(FaultPoint::NotificationAfterCommit)
            .map_err(|_| StoreError::QueueOutcomeUnknown)?;
        Ok(AcceptedMessage {
            message_id: plan.report_id,
            already_committed: false,
        })
    }
}

#[cfg(test)]
#[path = "notification_tests.rs"]
mod tests;
