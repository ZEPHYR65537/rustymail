use crate::blob::{reject_symlink, sync_directory, valid_id};
use crate::{FaultPoint, MigrationRecord, Store, StoreError, migration, unsigned_column};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::{fs, time::UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct OperationSummary {
    pub operation_id: String,
    pub message_id: String,
    pub blob_id: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub accepted_at_ms: i64,
    pub recipients: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct GcOptions {
    pub apply: bool,
    pub min_age_seconds: u64,
    pub limit: usize,
}
impl Default for GcOptions {
    fn default() -> Self {
        Self {
            apply: false,
            min_age_seconds: 86400,
            limit: 1000,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GcCandidate {
    pub run_id: Option<String>,
    pub kind: &'static str,
    pub object_id: String,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub run_id: Option<String>,
    pub examined: u64,
    pub candidates: u64,
    pub deleted: u64,
    pub deleted_bytes: u64,
    pub skipped_recent: u64,
    pub more: bool,
}

#[derive(Debug, Serialize)]
pub struct GcRun {
    pub id: String,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub min_age_seconds: u64,
    pub status: String,
    pub planned: u64,
    pub deleted: u64,
}

impl Store {
    fn require_idle(&self) -> Result<(), StoreError> {
        if *self
            .reserved_bytes
            .lock()
            .map_err(|_| StoreError::WorkerUnavailable)?
            != 0
        {
            return Err(StoreError::MaintenanceBusy);
        }
        Ok(())
    }

    pub fn migration_history(&self) -> Result<Vec<MigrationRecord>, StoreError> {
        migration::history(&self.connection)
    }

    pub fn operation(&self, id: &str) -> Result<Option<OperationSummary>, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        Ok(self.connection.query_row("SELECT m.id,b.id,b.size_bytes,b.sha256,m.accepted_at_ms,(SELECT count(*) FROM delivery d WHERE d.message_id=m.id) FROM message m JOIN blob b ON b.id=m.blob_id WHERE m.ingest_key=?1", [id], |r| Ok(OperationSummary {
            operation_id: id.into(), message_id: r.get(0)?, blob_id: r.get(1)?,
            size_bytes: unsigned_column(r,2)?, sha256: r.get(3)?,
            accepted_at_ms: r.get(4)?, recipients: unsigned_column(r,5)?,
        })).optional()?)
    }

    /// Only filesystem orphans and stale staging files are collected. A blob
    /// row is a conservative retention root, including history and backup pins.
    /// Existing mailbox/message/delivery rows are never expired by this command.
    pub fn gc(
        &mut self,
        options: GcOptions,
        mut emit: impl FnMut(&GcCandidate) -> Result<(), StoreError>,
    ) -> Result<GcReport, StoreError> {
        self.require_idle()?;
        if options.limit == 0
            || options.limit > 1000
            || options.min_age_seconds > (i64::MAX / 1000) as u64
        {
            return Err(StoreError::InvalidInput);
        }
        if !self.check_integrity()?.healthy() {
            return Err(StoreError::Integrity);
        }
        let now = self.runtime.now_ms()?;
        let cutoff = now.saturating_sub((options.min_age_seconds * 1000) as i64);
        let mut report = GcReport::default();
        if options.apply {
            let id = Uuid::new_v4().simple().to_string();
            self.connection.execute("INSERT INTO maintenance_run(id,started_at_ms,min_age_seconds,status) VALUES(?1,?2,?3,'running')", params![id,now,options.min_age_seconds as i64])?;
            report.run_id = Some(id);
        }
        for (directory, suffix, kind) in
            [("staging", ".part", "staging"), ("blobs", ".eml", "orphan")]
        {
            for entry in fs::read_dir(self.root.join(directory))? {
                let entry = entry?;
                report.examined += 1;
                let name = entry.file_name();
                let id = name
                    .to_str()
                    .and_then(|s| s.strip_suffix(suffix))
                    .filter(|id| valid_id(id))
                    .ok_or(StoreError::UnsafePath)?;
                let path = self.root.join(directory).join(format!("{id}{suffix}"));
                reject_symlink(&path)?;
                let metadata = fs::metadata(&path)?;
                if !metadata.is_file() {
                    return Err(StoreError::UnsafePath);
                }
                if directory == "blobs" && self.blob_retained(id)? {
                    continue;
                }
                let modified = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|time| i64::try_from(time.as_millis()).ok())
                    .ok_or(StoreError::UnsafePath)?;
                // A backward wall-clock jump preserves files instead of aging
                // them prematurely. New runs recompute all eligibility.
                if modified > cutoff {
                    report.skipped_recent += 1;
                    continue;
                }
                if report.candidates == options.limit as u64 {
                    report.more = true;
                    break;
                }
                let candidate = GcCandidate {
                    run_id: report.run_id.clone(),
                    kind,
                    object_id: id.into(),
                    size_bytes: metadata.len(),
                    modified_at_ms: modified,
                };
                emit(&candidate)?;
                report.candidates += 1;
                if let Some(run_id) = &report.run_id {
                    let size =
                        i64::try_from(metadata.len()).map_err(|_| StoreError::InvalidInput)?;
                    self.connection.execute(
                        "INSERT INTO gc_action VALUES(?1,?2,?3,?4,'planned')",
                        params![run_id, kind, id, size],
                    )?;
                    self.runtime.hit(FaultPoint::GcPlanned)?;
                    // Recheck after the durable audit record, before unlink.
                    if directory == "blobs" && self.blob_retained(id)? {
                        return Err(StoreError::Integrity);
                    }
                    reject_symlink(&path)?;
                    let current = fs::metadata(&path)?;
                    if !current.is_file()
                        || current.len() != metadata.len()
                        || current.modified()? != metadata.modified()?
                    {
                        return Err(StoreError::UnsafePath);
                    }
                    fs::remove_file(&path)?;
                    self.runtime.hit(FaultPoint::GcUnlinked)?;
                    self.runtime.hit(FaultPoint::GcDirectorySync)?;
                    sync_directory(&self.root.join(directory))?;
                    self.connection.execute("UPDATE gc_action SET status='deleted' WHERE run_id=?1 AND kind=?2 AND object_id=?3", params![run_id,kind,id])?;
                    report.deleted += 1;
                    report.deleted_bytes = report
                        .deleted_bytes
                        .checked_add(metadata.len())
                        .ok_or(StoreError::InvalidInput)?;
                }
            }
            if report.more {
                break;
            }
        }
        if let Some(id) = &report.run_id {
            self.connection.execute(
                "UPDATE maintenance_run SET status='complete',completed_at_ms=?1 WHERE id=?2",
                params![self.runtime.now_ms()?, id],
            )?;
        }
        Ok(report)
    }

    fn blob_retained(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM blob WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?)
    }

    pub fn gc_history(&self, limit: usize) -> Result<Vec<GcRun>, StoreError> {
        if limit == 0 || limit > 1000 {
            return Err(StoreError::InvalidInput);
        }
        Ok(self.connection.prepare("SELECT r.id,r.started_at_ms,r.completed_at_ms,r.min_age_seconds,r.status,(SELECT count(*) FROM gc_action a WHERE a.run_id=r.id),(SELECT count(*) FROM gc_action a WHERE a.run_id=r.id AND a.status='deleted') FROM maintenance_run r ORDER BY r.started_at_ms DESC,r.id DESC LIMIT ?1")?
            .query_map([limit as u32], |r| Ok(GcRun { id:r.get(0)?, started_at_ms:r.get(1)?, completed_at_ms:r.get(2)?, min_age_seconds:unsigned_column(r,3)?, status:r.get(4)?, planned:unsigned_column(r,5)?, deleted:unsigned_column(r,6)? }))?
            .collect::<Result<_,_>>()?)
    }

    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.require_idle()?;
        let busy: i64 = self
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(StoreError::MaintenanceBusy);
        }
        sync_directory(&self.root)?;
        Ok(())
    }

    /// Restore only a missing, already referenced blob from a byte-exact copy.
    /// Does not modify UIDs, quota, envelope or existing files, even if corrupt.
    pub fn recover_blob(
        &mut self,
        id: &str,
        source: impl AsRef<std::path::Path>,
    ) -> Result<u64, StoreError> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        self.require_idle()?;
        let target = crate::blob::blob_path(&self.root, id)?;
        reject_symlink(&target)?;
        if target.exists() {
            return Err(StoreError::AlreadyExists);
        }
        let expected: Option<(u64, String)> = self
            .connection
            .query_row(
                "SELECT size_bytes,sha256 FROM blob WHERE id=?1",
                [id],
                |r| Ok((unsigned_column(r, 0)?, r.get(1)?)),
            )
            .optional()?;
        let (size, digest) = expected.ok_or(StoreError::NotFound)?;
        // Existing responsibility survives later reductions of the ingress
        // size limit. Bound recovery by the recorded length and digest instead.
        let source = source.as_ref();
        reject_symlink(source)?;
        if !fs::metadata(source)?.is_file() {
            return Err(StoreError::UnsafePath);
        }
        let mut input = fs::File::open(source)?;
        let temporary = self
            .root
            .join("staging")
            .join(format!("{}.part", Uuid::new_v4().simple()));
        let mut output = crate::blob::private_file(&temporary, true)?;
        let result = (|| -> Result<u64, StoreError> {
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 16384];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(count as u64)
                    .ok_or(StoreError::SizeLimit)?;
                if bytes > size {
                    return Err(StoreError::Integrity);
                }
                hash.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            if bytes != size || format!("{:x}", hash.finalize()) != digest {
                return Err(StoreError::Integrity);
            }
            output.sync_all()?;
            // Link has no-overwrite semantics. Both names are on the store's
            // filesystem; a crash can leave a harmless extra staging link.
            fs::hard_link(&temporary, &target)?;
            sync_directory(&self.root.join("blobs"))?;
            Ok(bytes)
        })();
        drop(output);
        let cleanup = fs::remove_file(&temporary);
        if result.is_ok() {
            cleanup?;
            sync_directory(&self.root.join("staging"))?;
        }
        result
    }
}
