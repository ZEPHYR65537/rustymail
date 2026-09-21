use crate::{FaultPoint, InstanceLock, StorageRuntime, StoreError, StoreOptions};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncWriteExt, BufWriter};
use uuid::Uuid;

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    // There is no portable std directory fsync on Windows. This backend is for
    // laboratory development only; never claim Linux power-loss guarantees.
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) fn reject_symlink(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::UnsafePath),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<(), StoreError> {
    reject_symlink(path)?;
    if !path.exists() {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.exists()
        {
            private_directory(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir(path)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_directory(parent)?;
        }
    }
    if !path.is_dir() {
        return Err(StoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(StoreError::UnsafePermissions);
        }
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn blob_path(root: &Path, id: &str) -> Result<PathBuf, StoreError> {
    if !valid_id(id) {
        return Err(StoreError::InvalidId);
    }
    Ok(root.join("blobs").join(format!("{id}.eml")))
}

/// Owns a capped, temporary raw-message file. Dropping never creates a database
/// reference. An interrupted async write may leave a .part for offline recovery.
pub struct StagedMessage {
    root: Arc<PathBuf>,
    id: String,
    path: PathBuf,
    writer: Option<BufWriter<tokio::fs::File>>,
    hash: Sha256,
    size: u64,
    max_size: u64,
    poisoned: bool,
    reservation: Option<DiskReservation>,
    lock: Arc<InstanceLock>,
    runtime: StorageRuntime,
}

/// Only `StagedMessage::prepare` can construct this durability token.
/// Unreferenced final files are retained for diagnosis, never automatically
/// removed after an outcome-unknown database commit.
pub struct PreparedMessage {
    pub(crate) root: Arc<PathBuf>,
    pub(crate) id: String,
    pub(crate) size: u64,
    pub(crate) hash: String,
    _reservation: DiskReservation,
    _lock: Arc<InstanceLock>,
}

struct DiskReservation {
    reserved: Arc<Mutex<u64>>,
    bytes: u64,
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        if let Ok(mut count) = self.reserved.lock() {
            *count -= self.bytes;
        }
    }
}

impl StagedMessage {
    pub(crate) fn create(
        root: Arc<PathBuf>,
        lock: Arc<InstanceLock>,
        reserved: Arc<Mutex<u64>>,
        options: &StoreOptions,
        runtime: StorageRuntime,
    ) -> Result<Self, StoreError> {
        let total = fs2::total_space(root.as_ref())?;
        let available = fs2::available_space(root.as_ref())?;
        let percent = (u128::from(total) * u128::from(options.disk_reserve_percent) / 100) as u64;
        let mut in_flight = reserved.lock().map_err(|_| StoreError::WorkerUnavailable)?;
        let next = in_flight
            .checked_add(options.max_message_bytes)
            .ok_or(StoreError::DiskReserve)?;
        if next > options.temporary_reserved_bytes {
            return Err(StoreError::DiskReserve);
        }
        let required = options
            .disk_reserve_bytes
            .max(percent)
            .checked_add(next)
            .ok_or(StoreError::SizeLimit)?;
        if available < required {
            return Err(StoreError::DiskReserve);
        }
        *in_flight = next;
        drop(in_flight);
        let reservation = DiskReservation {
            reserved,
            bytes: options.max_message_bytes,
        };
        let id = Uuid::new_v4().simple().to_string();
        let path = root.join("staging").join(format!("{id}.part"));
        runtime.hit(FaultPoint::StageCreate)?;
        let file = private_file(&path, true)?;
        let mut async_file = tokio::fs::File::from_std(file);
        async_file.set_max_buf_size(options.stream_buffer_bytes);
        Ok(Self {
            root,
            id,
            path,
            writer: Some(BufWriter::with_capacity(
                options.stream_buffer_bytes,
                async_file,
            )),
            hash: Sha256::new(),
            size: 0,
            max_size: options.max_message_bytes,
            poisoned: false,
            reservation: Some(reservation),
            lock,
            runtime,
        })
    }

    pub async fn append(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::PoisonedStage);
        }
        let Some(size) = self
            .size
            .checked_add(bytes.len() as u64)
            .filter(|&v| v <= self.max_size)
        else {
            self.poisoned = true;
            return Err(StoreError::SizeLimit);
        };
        let writer = self.writer.as_mut().ok_or(StoreError::PoisonedStage)?;
        // A cancelled write can leave partial bytes on disk. Only a completed
        // write restores this token, so later prepare cannot accept a stale hash.
        self.poisoned = true;
        self.runtime.hit(FaultPoint::Append)?;
        if let Err(error) = writer.write_all(bytes).await {
            return Err(error.into());
        }
        self.hash.update(bytes);
        self.size = size;
        self.poisoned = false;
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub async fn prepare(self) -> Result<PreparedMessage, StoreError> {
        self.prepare_with_hook(|_| {}).await
    }

    pub(crate) async fn prepare_with_hook(
        mut self,
        hook: impl Fn(&str) + Send + 'static,
    ) -> Result<PreparedMessage, StoreError> {
        if self.poisoned {
            return Err(StoreError::PoisonedStage);
        }
        let mut writer = self.writer.take().ok_or(StoreError::PoisonedStage)?;
        self.runtime.hit(FaultPoint::Flush)?;
        writer.flush().await?;
        self.runtime.hit(FaultPoint::FileSync)?;
        writer.get_ref().sync_all().await?;
        // Close before renaming on Windows, and wait for all Tokio file work.
        let file = writer.into_inner().into_std().await;
        drop(file);
        hook("file_synced");
        self.runtime.hit(FaultPoint::FileSynced)?;
        let root = self.root.clone();
        let id = self.id.clone();
        let path = self.path.clone();
        let size = self.size;
        let hash = format!("{:x}", self.hash.clone().finalize());
        let reservation = self.reservation.take().ok_or(StoreError::PoisonedStage)?;
        let lock = self.lock.clone();
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let destination = blob_path(&root, &id)?;
            if destination.exists() {
                return Err(StoreError::InvalidId);
            }
            runtime.hit(FaultPoint::Rename)?;
            fs::rename(&path, &destination)?;
            hook("renamed");
            runtime.hit(FaultPoint::Renamed)?;
            runtime.hit(FaultPoint::BlobDirectorySync)?;
            sync_directory(&root.join("blobs"))?;
            runtime.hit(FaultPoint::StagingDirectorySync)?;
            sync_directory(&root.join("staging"))?;
            hook("directories_synced");
            runtime.hit(FaultPoint::DirectoriesSynced)?;
            Ok(PreparedMessage {
                root,
                id,
                size,
                hash,
                _reservation: reservation,
                _lock: lock,
            })
        })
        .await
        .map_err(|_| StoreError::WorkerUnavailable)?
    }
}

impl Drop for StagedMessage {
    fn drop(&mut self) {
        // Best effort cleanup only. Never delete a renamed/final blob here.
        drop(self.writer.take());
        let _ = fs::remove_file(&self.path);
    }
}
