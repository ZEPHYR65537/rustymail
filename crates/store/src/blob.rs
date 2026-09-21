use crate::{FaultPoint, InstanceLock, StorageRuntime, StoreError, StoreOptions};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
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
    resources: Arc<StageResources>,
    buffer: Vec<u8>,
    buffer_limit: usize,
    hash: Sha256,
    size: u64,
    max_size: u64,
    poisoned: bool,
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
    _resources: Arc<StageResources>,
}

/// Every actual disk job owns this token, including after its waiter is gone.
/// At most one write/prepare job per stage can be outstanding: cancellation
/// poisons the stage before it can submit another job.
struct StageResources {
    disk: Mutex<StageDisk>,
    path: PathBuf,
    reservation: Option<DiskReservation>,
    lock: Option<Arc<InstanceLock>>,
}

struct StageDisk {
    file: Option<File>,
    staged: bool,
}

impl Drop for StageResources {
    fn drop(&mut self) {
        let disk = self.disk.get_mut().unwrap_or_else(|e| e.into_inner());
        let file = disk.file.take();
        let path = disk.staged.then(|| std::mem::take(&mut self.path));
        let reservation = self.reservation.take();
        let lock = self.lock.take();
        if file.is_none() && path.is_none() {
            return; // Prepared: closed file, no staging name to clean up.
        }
        let cleanup = move || {
            drop(file);
            if let Some(path) = path {
                let _ = fs::remove_file(path);
            }
            // Keep both tokens until close/unlink really finish. Cleanup jobs
            // are bounded by the same reservation budget as active stages.
            drop(lock);
            drop(reservation);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(cleanup);
            }
            Err(_) => cleanup(), // Store owner/offline caller, not a reactor.
        }
    }
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
        Ok(Self {
            root,
            id,
            resources: Arc::new(StageResources {
                disk: Mutex::new(StageDisk {
                    file: Some(file),
                    staged: true,
                }),
                path,
                reservation: Some(reservation),
                lock: Some(lock),
            }),
            buffer: Vec::with_capacity(options.stream_buffer_bytes),
            buffer_limit: options.stream_buffer_bytes,
            hash: Sha256::new(),
            size: 0,
            max_size: options.max_message_bytes,
            poisoned: false,
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
        // A cancelled write can leave partial bytes on disk. Only a completed
        // write restores this token, so later prepare cannot accept a stale hash.
        self.poisoned = true;
        self.runtime.hit(FaultPoint::Append)?;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = remaining.len().min(self.buffer_limit - self.buffer.len());
            self.buffer.extend_from_slice(&remaining[..count]);
            remaining = &remaining[count..];
            if self.buffer.len() == self.buffer_limit {
                let mut buffer = std::mem::take(&mut self.buffer);
                let resources = self.resources.clone();
                self.buffer = tokio::task::spawn_blocking(move || {
                    let mut disk = resources
                        .disk
                        .lock()
                        .map_err(|_| StoreError::PoisonedStage)?;
                    disk.file
                        .as_mut()
                        .ok_or(StoreError::PoisonedStage)?
                        .write_all(&buffer)?;
                    buffer.clear();
                    Ok::<_, StoreError>(buffer)
                })
                .await
                .map_err(|_| StoreError::WorkerUnavailable)??;
            }
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
        let root = self.root.clone();
        let id = self.id.clone();
        let size = self.size;
        let hash = format!("{:x}", self.hash.clone().finalize());
        let buffer = std::mem::take(&mut self.buffer);
        let resources = self.resources.clone();
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let mut disk = resources
                .disk
                .lock()
                .map_err(|_| StoreError::PoisonedStage)?;
            let writer = disk.file.as_mut().ok_or(StoreError::PoisonedStage)?;
            runtime.hit(FaultPoint::Flush)?;
            writer.write_all(&buffer)?;
            runtime.hit(FaultPoint::FileSync)?;
            writer.sync_all()?;
            drop(disk.file.take()); // Close before rename, including on Windows.
            hook("file_synced");
            runtime.hit(FaultPoint::FileSynced)?;
            let destination = blob_path(&root, &id)?;
            if destination.exists() {
                return Err(StoreError::InvalidId);
            }
            runtime.hit(FaultPoint::Rename)?;
            fs::rename(&resources.path, &destination)?;
            disk.staged = false;
            drop(disk);
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
                _resources: resources,
            })
        })
        .await
        .map_err(|_| StoreError::WorkerUnavailable)?
    }
}
