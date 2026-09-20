//! Single-writer, file-backed message storage.
mod blob;
pub use blob::{PreparedMessage, StagedMessage};

use blob::{blob_path, private_directory, private_file, reject_symlink, sync_directory, valid_id};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use rustymail_core::Address;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA: &str = include_str!("../migrations/0001.sql");

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("storage I/O failure: {0}")]
    Io(#[from] io::Error),
    #[error("database failure: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("data directory is already locked by another process")]
    Locked,
    #[error("unsafe storage path or symbolic link")]
    UnsafePath,
    #[error("data directory permissions must be private (0700)")]
    UnsafePermissions,
    #[error("invalid internal object identifier")]
    InvalidId,
    #[error("unsupported database schema; refusing to modify it")]
    SchemaVersion,
    #[error("database or referenced message integrity check failed")]
    Integrity,
    #[error("message exceeds the configured limit")]
    SizeLimit,
    #[error("insufficient disk space above reserve")]
    DiskReserve,
    #[error("staged writer is no longer usable")]
    PoisonedStage,
    #[error("recipient is missing, disabled, or not authorized for local delivery")]
    RecipientUnavailable,
    #[error("mailbox quota exceeded")]
    Quota,
    #[error("UID or event counter exhausted")]
    UidExhausted,
    #[error("internal retry key conflicts with another message")]
    IdempotencyConflict,
    #[error("acceptance outcome unknown; close the SMTP connection and recover by operation ID")]
    OutcomeUnknown,
    #[error("message not found")]
    NotFound,
    #[error("invalid storage arguments")]
    InvalidInput,
    #[error("storage worker unavailable")]
    WorkerUnavailable,
}

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub max_message_bytes: u64,
    pub stream_buffer_bytes: usize,
    pub disk_reserve_bytes: u64,
    pub disk_reserve_percent: u8,
    pub temporary_reserved_bytes: u64,
    pub cache_kib: u32,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 25 * 1024 * 1024,
            stream_buffer_bytes: 16384,
            disk_reserve_bytes: 2 * 1024 * 1024 * 1024,
            disk_reserve_percent: 10,
            temporary_reserved_bytes: 400 * 1024 * 1024,
            cache_kib: 8192,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Acceptance {
    pub operation_id: String,
    pub sender: Option<Address>,
    pub recipients: Vec<Address>,
}

#[derive(Clone, Debug)]
pub struct AcceptedMessage {
    pub message_id: String,
    pub already_committed: bool,
}

#[derive(Debug)]
pub struct MessageSummary {
    pub uid: u32,
    pub message_id: String,
    pub size_bytes: u64,
    pub accepted_at_ms: i64,
}

#[derive(Debug, Default)]
pub struct IntegrityReport {
    pub referenced_blobs: u64,
    pub missing_blobs: u64,
    pub corrupt_blobs: u64,
    pub orphan_blobs: u64,
    pub staging_files: u64,
}

impl IntegrityReport {
    pub fn healthy(&self) -> bool {
        self.missing_blobs == 0 && self.corrupt_blobs == 0
    }
}

/// Exclusive process lock plus one connection. Run this owner on a dedicated
/// thread, not a Tokio executor thread. An admin CLI must acquire the same lock.
pub struct Store {
    connection: Connection,
    root: Arc<PathBuf>,
    options: StoreOptions,
    lock: Arc<File>,
    reserved_bytes: Arc<Mutex<u64>>,
}

fn now_ms() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or(StoreError::InvalidInput)
}

fn unsigned_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

impl Store {
    pub fn open(path: impl AsRef<Path>, options: StoreOptions) -> Result<Self, StoreError> {
        if options.max_message_bytes == 0
            || options.max_message_bytes > 25 * 1024 * 1024
            || !(1024..=65536).contains(&options.stream_buffer_bytes)
            || options.disk_reserve_percent >= 100
            || options.cache_kib > 65536
            || options.temporary_reserved_bytes < options.max_message_bytes
        {
            return Err(StoreError::InvalidInput);
        }
        private_directory(path.as_ref())?;
        let root = Arc::new(fs::canonicalize(path)?);
        let lock_path = root.join("instance.lock");
        reject_symlink(&lock_path)?;
        let lock = private_file(&lock_path, false)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Err(StoreError::Locked),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        private_directory(&root.join("staging"))?;
        private_directory(&root.join("blobs"))?;
        for name in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm"] {
            reject_symlink(&root.join(name))?;
        }
        let database_path = root.join("meta.sqlite");
        // Create privately before handing it to SQLite (which has its own
        // default creation mode). The containing directory is also private.
        drop(private_file(&database_path, false)?);
        let mut connection = Connection::open(database_path)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            return Err(StoreError::SchemaVersion);
        }
        if version == 0 {
            let count: i64 = connection.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
            if count != 0 {
                return Err(StoreError::SchemaVersion);
            }
        }
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "cache_size", -i64::from(options.cache_kib))?;
        if version == 0 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(SCHEMA)?;
            transaction.pragma_update(None, "user_version", 1)?;
            transaction.commit()?;
        } else if version != 1 {
            return Err(StoreError::SchemaVersion);
        }
        let result: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result != "ok" {
            return Err(StoreError::Integrity);
        }
        let violations: i64 =
            connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            return Err(StoreError::Integrity);
        }
        sync_directory(&root)?;
        Ok(Self {
            connection,
            root,
            options,
            lock: Arc::new(lock),
            reserved_bytes: Arc::new(Mutex::new(0)),
        })
    }

    pub fn create_account(
        &mut self,
        address: &Address,
        quota_bytes: u64,
    ) -> Result<(), StoreError> {
        let quota = i64::try_from(quota_bytes).map_err(|_| StoreError::InvalidInput)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO account(login, quota_bytes, created_at_ms) VALUES(?1, ?2, ?3)",
            params![address.local_key(), quota, now_ms()?],
        )?;
        let account_id = transaction.last_insert_rowid();
        transaction.execute("INSERT INTO address(address, account_id, receive_enabled, send_enabled) VALUES(?1, ?2, 1, 0)", params![address.local_key(), account_id])?;
        for name in ["INBOX", "Sent", "Drafts", "Trash", "Archive", "Junk"] {
            let bytes = Uuid::new_v4().into_bytes();
            let uidvalidity = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).max(1);
            transaction.execute(
                "INSERT INTO mailbox(account_id, name, uidvalidity) VALUES(?1, ?2, ?3)",
                params![account_id, name, uidvalidity],
            )?;
            transaction.execute(
                "INSERT INTO subscription(account_id, mailbox_name) VALUES(?1, ?2)",
                params![account_id, name],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recipient_exists(&self, address: &Address) -> Result<bool, StoreError> {
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM address a JOIN account u ON u.id=a.account_id JOIN mailbox m ON m.account_id=u.id AND m.name='INBOX' WHERE a.address=?1 AND a.receive_enabled=1 AND u.status='active')", [address.local_key()], |r| r.get(0))?)
    }

    pub fn stage(&self) -> Result<StagedMessage, StoreError> {
        StagedMessage::create(
            self.root.clone(),
            self.lock.clone(),
            self.reserved_bytes.clone(),
            &self.options,
        )
    }

    pub fn accept(
        &mut self,
        message: PreparedMessage,
        plan: Acceptance,
    ) -> Result<AcceptedMessage, StoreError> {
        self.accept_with_hook(message, plan, |_| {})
    }

    pub(crate) fn accept_with_hook(
        &mut self,
        message: PreparedMessage,
        plan: Acceptance,
        hook: impl Fn(&str),
    ) -> Result<AcceptedMessage, StoreError> {
        if message.root != self.root
            || !valid_id(&plan.operation_id)
            || plan.recipients.is_empty()
            || plan.recipients.len() > 100
        {
            return Err(StoreError::InvalidInput);
        }
        let sender = plan.sender.as_ref().map_or("", Address::as_str);
        let recipients: BTreeSet<String> = plan.recipients.iter().map(Address::local_key).collect();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String, String, u64)> = transaction.query_row(
            "SELECT m.id,m.reverse_path,b.sha256,b.size_bytes FROM message m JOIN blob b ON b.id=m.blob_id WHERE ingest_key=?1", [&plan.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, unsigned_column(r,3)?))).optional()?;
        if let Some((id, original_sender, hash, size)) = previous {
            let old: BTreeSet<String> = transaction
                .prepare("SELECT recipient FROM delivery WHERE message_id=?1")?
                .query_map([&id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            if original_sender != sender
                || hash != message.hash
                || size != message.size
                || old != recipients
            {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(AcceptedMessage {
                message_id: id,
                already_committed: true,
            });
        }
        // Revalidate every recipient inside the committing transaction. RCPT
        // acceptance did not reserve the right to exceed quota or use a removed account.
        let mut targets = Vec::with_capacity(recipients.len());
        let mut quotas: BTreeMap<i64, (u64, u64, u64)> = BTreeMap::new();
        for recipient in &recipients {
            let target: Option<(i64, i64, u64, u64, u64, i64)> = transaction.query_row(
                "SELECT a.account_id,m.id,u.quota_bytes,u.used_bytes,m.uidnext,m.event_seq FROM address a JOIN account u ON u.id=a.account_id JOIN mailbox m ON m.account_id=u.id AND m.name='INBOX' WHERE a.address=?1 AND a.receive_enabled=1 AND u.status='active'",
                [recipient], |r| Ok((r.get(0)?, r.get(1)?, unsigned_column(r,2)?, unsigned_column(r,3)?, unsigned_column(r,4)?, r.get(5)?))).optional()?;
            let (account_id, mailbox_id, quota, used, uidnext, event_seq) =
                target.ok_or(StoreError::RecipientUnavailable)?;
            if uidnext > u64::from(u32::MAX) || event_seq == i64::MAX {
                return Err(StoreError::UidExhausted);
            }
            quotas
                .entry(account_id)
                .and_modify(|value| value.0 += 1)
                .or_insert((1, quota, used));
            targets.push((recipient.clone(), mailbox_id));
        }
        for &(count, quota, used) in quotas.values() {
            let next = message
                .size
                .checked_mul(count)
                .and_then(|n| used.checked_add(n))
                .ok_or(StoreError::Quota)?;
            if next > quota || next > i64::MAX as u64 {
                return Err(StoreError::Quota);
            }
        }
        let id = Uuid::new_v4().simple().to_string();
        let now = now_ms()?;
        transaction.execute("INSERT INTO blob(id,size_bytes,sha256,mime_metadata,metadata_version,created_at_ms) VALUES(?1,?2,?3,?4,1,?5)",
            params![message.id, message.size as i64, message.hash, b"{}".as_slice(), now])?;
        transaction.execute("INSERT INTO message(id,ingest_key,blob_id,source,reverse_path,accepted_at_ms) VALUES(?1,?2,?3,'smtp',?4,?5)",
            params![id, plan.operation_id, message.id, sender, now])?;
        for (recipient, mailbox_id) in targets {
            // Read again because distinct aliases may target the same mailbox.
            let (uid, event_seq): (u64, i64) = transaction.query_row(
                "SELECT uidnext,event_seq FROM mailbox WHERE id=?1",
                [mailbox_id],
                |r| Ok((unsigned_column(r, 0)?, r.get(1)?)),
            )?;
            if uid > u64::from(u32::MAX) || event_seq == i64::MAX {
                return Err(StoreError::UidExhausted);
            }
            let delivery_id = Uuid::new_v4().simple().to_string();
            let domain = recipient.rsplit_once('@').map_or("", |(_, d)| d);
            transaction.execute("INSERT INTO delivery(id,message_id,recipient,route,destination_domain,state,next_attempt_at_ms,expires_at_ms) VALUES(?1,?2,?3,'local',?4,'delivered',?5,?5)",
                params![delivery_id,id,recipient,domain,now])?;
            transaction.execute("INSERT INTO mailbox_message(mailbox_id,uid,message_id,delivery_id,internaldate_ms) VALUES(?1,?2,?3,?4,?5)",
                params![mailbox_id,uid as i64,id,delivery_id,now])?;
            transaction.execute(
                "UPDATE mailbox SET uidnext=uidnext+1,event_seq=event_seq+1 WHERE id=?1",
                [mailbox_id],
            )?;
            transaction.execute("INSERT INTO mailbox_event(mailbox_id,event_seq,kind,uid,payload,created_at_ms) VALUES(?1,?2,'append',?3,?4,?5)",
                params![mailbox_id,event_seq+1,uid as i64,b"{}".as_slice(),now])?;
        }
        for (account_id, (count, _, _)) in quotas {
            transaction.execute(
                "UPDATE account SET used_bytes=used_bytes+?1 WHERE id=?2",
                params![(message.size * count) as i64, account_id],
            )?;
        }
        hook("before_commit");
        // SQLite can fail while reporting a commit outcome. Never map this to
        // a definite SMTP 4xx while an accepted message might be visible.
        transaction
            .commit()
            .map_err(|_| StoreError::OutcomeUnknown)?;
        hook("after_commit");
        Ok(AcceptedMessage {
            message_id: id,
            already_committed: false,
        })
    }

    pub fn list_messages(
        &self,
        address: &Address,
        after_uid: u32,
        limit: usize,
    ) -> Result<Vec<MessageSummary>, StoreError> {
        if limit == 0 || limit > 1000 {
            return Err(StoreError::InvalidInput);
        }
        let mut statement = self.connection.prepare("SELECT mm.uid,m.id,b.size_bytes,m.accepted_at_ms FROM mailbox_message mm JOIN mailbox box ON box.id=mm.mailbox_id JOIN account a ON a.id=box.account_id JOIN message m ON m.id=mm.message_id JOIN blob b ON b.id=m.blob_id WHERE a.login=?1 AND box.name='INBOX' AND mm.uid>?2 ORDER BY mm.uid LIMIT ?3")?;
        Ok(statement
            .query_map(params![address.local_key(), after_uid, limit as u32], |r| {
                Ok(MessageSummary {
                    uid: r.get(0)?,
                    message_id: r.get(1)?,
                    size_bytes: unsigned_column(r, 2)?,
                    accepted_at_ms: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    /// Export requires both owning account and message ID; a global message ID
    /// alone is never sufficient to read another account's raw mail.
    pub fn export(
        &self,
        address: &Address,
        message_id: &str,
        destination: &mut impl Write,
    ) -> Result<u64, StoreError> {
        if !valid_id(message_id) {
            return Err(StoreError::InvalidId);
        }
        let id: Option<String> = self.connection.query_row("SELECT m.blob_id FROM message m JOIN mailbox_message mm ON mm.message_id=m.id JOIN mailbox box ON box.id=mm.mailbox_id JOIN account a ON a.id=box.account_id WHERE a.login=?1 AND m.id=?2 LIMIT 1", params![address.local_key(),message_id], |r| r.get(0)).optional()?;
        let path = blob_path(&self.root, &id.ok_or(StoreError::NotFound)?)?;
        reject_symlink(&path)?;
        Ok(io::copy(&mut File::open(path)?, destination)?)
    }

    pub fn check_integrity(&self) -> Result<IntegrityReport, StoreError> {
        let mut report = IntegrityReport::default();
        let mut statement = self
            .connection
            .prepare("SELECT id,size_bytes,sha256 FROM blob")?;
        let mut rows = statement.query([])?;
        let mut buffer = vec![0u8; 16384];
        while let Some(row) = rows.next()? {
            report.referenced_blobs += 1;
            let id: String = row.get(0)?;
            let size = unsigned_column(row, 1)?;
            let expected: String = row.get(2)?;
            let path = blob_path(&self.root, &id)?;
            reject_symlink(&path)?;
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    report.missing_blobs += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let mut hash = Sha256::new();
            let mut actual = 0u64;
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                actual += n as u64;
                hash.update(&buffer[..n]);
            }
            if actual != size || format!("{:x}", hash.finalize()) != expected {
                report.corrupt_blobs += 1;
            }
        }
        for entry in fs::read_dir(self.root.join("blobs"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".eml")) else {
                continue;
            };
            if !valid_id(id) {
                return Err(StoreError::InvalidId);
            }
            if entry.file_type()?.is_symlink() {
                return Err(StoreError::UnsafePath);
            }
            let present: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM blob WHERE id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if !present {
                report.orphan_blobs += 1;
            }
        }
        for entry in fs::read_dir(self.root.join("staging"))? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(StoreError::UnsafePath);
            }
            report.staging_files += 1;
        }
        Ok(report)
    }

    pub fn directory_sync_supported() -> bool {
        cfg!(unix)
    }
}

#[cfg(test)]
mod tests;
