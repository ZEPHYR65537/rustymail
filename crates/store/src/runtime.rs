use crate::StoreError;
use std::{
    io,
    sync::{Arc, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Wall-clock persistence and monotonic elapsed time for discontinuity checks.
/// Network deadlines separately use Tokio's monotonic clock.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Result<i64, StoreError>;
    fn monotonic_ms(&self) -> u64 {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> Result<i64, StoreError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|time| i64::try_from(time.as_millis()).ok())
            .ok_or(StoreError::InvalidInput)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    StageCreate,
    Append,
    Flush,
    FileSync,
    FileSynced,
    Rename,
    Renamed,
    BlobDirectorySync,
    StagingDirectorySync,
    DirectoriesSynced,
    DatabaseWrite,
    BeforeCommit,
    Commit,
    AfterCommit,
    MigrationApplied,
    MigrationCommitted,
    GcPlanned,
    GcUnlinked,
    GcDirectorySync,
    QueueBeforeCommit,
    QueueAfterCommit,
}

impl FaultPoint {
    pub fn name(self) -> &'static str {
        match self {
            Self::StageCreate => "stage_create",
            Self::Append => "append",
            Self::Flush => "flush",
            Self::FileSync => "file_sync",
            Self::FileSynced => "file_synced",
            Self::Rename => "rename",
            Self::Renamed => "renamed",
            Self::BlobDirectorySync => "blob_directory_sync",
            Self::StagingDirectorySync => "staging_directory_sync",
            Self::DirectoriesSynced => "directories_synced",
            Self::DatabaseWrite => "database_write",
            Self::BeforeCommit => "before_commit",
            Self::Commit => "commit",
            Self::AfterCommit => "after_commit",
            Self::MigrationApplied => "migration_applied",
            Self::MigrationCommitted => "migration_committed",
            Self::GcPlanned => "gc_planned",
            Self::GcUnlinked => "gc_unlinked",
            Self::GcDirectorySync => "gc_directory_sync",
            Self::QueueBeforeCommit => "queue_before_commit",
            Self::QueueAfterCommit => "queue_after_commit",
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
type Hook = dyn Fn(FaultPoint) -> io::Result<()> + Send + Sync;

/// Injectable clock and opt-in fault boundaries. Normal binaries have no hook
/// field or setter and never read a fault switch from the environment/config.
#[derive(Clone)]
pub struct StorageRuntime {
    clock: Arc<dyn Clock>,
    #[cfg(any(test, feature = "test-support"))]
    hook: Option<Arc<Hook>>,
}

impl Default for StorageRuntime {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }
}

impl StorageRuntime {
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            #[cfg(any(test, feature = "test-support"))]
            hook: None,
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_hook(
        mut self,
        hook: impl Fn(FaultPoint) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }
    pub(crate) fn now_ms(&self) -> Result<i64, StoreError> {
        let value = self.clock.now_ms()?;
        if value < 0 {
            return Err(StoreError::InvalidInput);
        }
        Ok(value)
    }
    pub(crate) fn monotonic_ms(&self) -> u64 {
        self.clock.monotonic_ms()
    }
    pub(crate) fn hit(&self, point: FaultPoint) -> io::Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hook) = &self.hook {
            return hook(point);
        }
        let _ = point;
        Ok(())
    }
}
