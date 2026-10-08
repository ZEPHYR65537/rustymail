//! Local delivery invariants shared by acceptance and generated notifications.
use crate::{StoreError, unsigned_column};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

struct Target {
    account: i64,
    mailbox: i64,
    uid: u64,
    event: i64,
}

fn target(db: &Connection, recipient: &str, size: u64) -> Result<Target, StoreError> {
    let row: Option<(i64, i64, u64, i64, u64, u64)> = db
        .prepare_cached(
            "SELECT a.account_id,m.id,m.uidnext,m.event_seq,u.quota_bytes,u.used_bytes
         FROM address a JOIN account u ON u.id=a.account_id
         JOIN mailbox m ON m.account_id=u.id AND m.name='INBOX'
         WHERE a.address=?1 AND a.receive_enabled=1 AND u.status='active'",
        )?
        .query_row([recipient], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                unsigned_column(r, 2)?,
                r.get(3)?,
                unsigned_column(r, 4)?,
                unsigned_column(r, 5)?,
            ))
        })
        .optional()?;
    let (account, mailbox, uid, event, quota, used) =
        row.ok_or(StoreError::RecipientUnavailable)?;
    if uid > u64::from(u32::MAX) || event == i64::MAX {
        return Err(StoreError::UidExhausted);
    }
    if used
        .checked_add(size)
        .is_none_or(|n| n > quota || n > i64::MAX as u64)
    {
        return Err(StoreError::Quota);
    }
    Ok(Target {
        account,
        mailbox,
        uid,
        event,
    })
}

/// Avoid publishing a report blob while its mailbox is already unavailable.
pub(crate) fn preflight(db: &Connection, recipient: &str, size: u64) -> Result<(), StoreError> {
    target(db, recipient, size).map(|_| ())
}

/// Read current counters in the caller's transaction, including earlier aliases
/// in the same acceptance. Any failure rolls back the whole acceptance/report.
pub(crate) fn deliver(
    tx: &Transaction<'_>,
    recipient: &str,
    message: &str,
    size: u64,
    now: i64,
) -> Result<(), StoreError> {
    let t = target(tx, recipient, size)?;
    let delivery = Uuid::new_v4().simple().to_string();
    let domain = recipient
        .rsplit_once('@')
        .ok_or(StoreError::InvalidInput)?
        .1;
    tx.prepare_cached("INSERT INTO delivery(id,message_id,recipient,route,destination_domain,state,next_attempt_at_ms,expires_at_ms)
        VALUES(?1,?2,?3,'local',?4,'delivered',?5,?5)")?
        .execute(params![delivery, message, recipient, domain, now])?;
    tx.prepare_cached("INSERT INTO mailbox_message(mailbox_id,uid,message_id,delivery_id,internaldate_ms) VALUES(?1,?2,?3,?4,?5)")?
        .execute(params![t.mailbox, t.uid as i64, message, delivery, now])?;
    tx.prepare_cached("UPDATE mailbox SET uidnext=uidnext+1,event_seq=event_seq+1 WHERE id=?1")?
        .execute([t.mailbox])?;
    tx.prepare_cached("INSERT INTO mailbox_event(mailbox_id,event_seq,kind,uid,payload,created_at_ms) VALUES(?1,?2,'append',?3,?4,?5)")?
        .execute(params![t.mailbox, t.event + 1, t.uid as i64, b"{}".as_slice(), now])?;
    tx.prepare_cached("UPDATE account SET used_bytes=used_bytes+?2 WHERE id=?1")?
        .execute(params![t.account, size as i64])?;
    Ok(())
}
