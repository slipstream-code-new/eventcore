//! Store configuration and the resolved on-disk directory layout.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::FsEventStoreError;

/// Whether to fsync written files and their directory for durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// fsync the transaction file and the events directory on every append.
    #[default]
    Full,
    /// Skip fsync (faster; for tests or non-durable use).
    None,
}

/// Configuration for opening a [`crate::FileEventStore`].
#[derive(Debug, Clone)]
pub struct FsConfig {
    pub(crate) root: PathBuf,
    pub(crate) fsync: FsyncPolicy,
}

impl FsConfig {
    /// Create a config rooted at `root` with full fsync durability.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            fsync: FsyncPolicy::Full,
        }
    }

    /// Override the fsync policy.
    pub fn with_fsync(mut self, policy: FsyncPolicy) -> Self {
        self.fsync = policy;
        self
    }
}

/// Resolved paths under a store root. Only `events/` is committed to git;
/// `tmp/` and `.eventcore/` (and the `.lock` file) are derived/local.
#[derive(Debug, Clone)]
pub(crate) struct Roots {
    pub(crate) root: PathBuf,
    pub(crate) events: PathBuf,
    pub(crate) tmp: PathBuf,
    pub(crate) eventcore: PathBuf,
}

impl Roots {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            events: root.join("events"),
            tmp: root.join("tmp"),
            eventcore: root.join(".eventcore"),
        }
    }

    pub(crate) fn create_dirs(&self) -> Result<(), FsEventStoreError> {
        for dir in [&self.events, &self.tmp, &self.eventcore] {
            fs::create_dir_all(dir).map_err(|source| FsEventStoreError::InitFailed {
                path: dir.clone(),
                source,
            })?;
        }
        Ok(())
    }

    pub(crate) fn store_lock_path(&self) -> PathBuf {
        self.root.join(".lock")
    }

    pub(crate) fn replica_id_path(&self) -> PathBuf {
        self.eventcore.join("replica_id")
    }

    pub(crate) fn event_path(&self, transaction_id: Uuid) -> PathBuf {
        self.events.join(format!("{transaction_id}.jsonl"))
    }

    pub(crate) fn tmp_path(&self, transaction_id: Uuid) -> PathBuf {
        self.tmp.join(format!("{transaction_id}.jsonl.tmp"))
    }
}
