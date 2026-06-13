//! Error types for the file backend.

use std::path::PathBuf;

/// Errors from opening or operating a [`crate::FileEventStore`].
#[derive(Debug, thiserror::Error)]
pub enum FsEventStoreError {
    /// The store directory could not be created or read.
    #[error("failed to initialize file event store at {path}: {source}")]
    InitFailed {
        /// The path that could not be created or read.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Another process or handle already holds this store's root lock.
    #[error("event store at {path} is locked by another process or handle")]
    StoreLocked {
        /// The store root whose lock is held.
        path: PathBuf,
    },
    /// A persisted file could not be parsed.
    #[error("corrupted file at {path}: {detail}")]
    Corrupted {
        /// The file that could not be parsed.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
}

/// Errors from a [`crate::FileCheckpointStore`].
#[derive(Debug, thiserror::Error)]
pub enum FsCheckpointError {
    /// An underlying filesystem error.
    #[error("checkpoint io error: {0}")]
    Io(#[from] std::io::Error),
    /// A checkpoint file held an unparseable position.
    #[error("corrupted checkpoint at {path}: {detail}")]
    Corrupted {
        /// The checkpoint file that could not be parsed.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
}

/// Errors from a [`crate::FileProjectorCoordinator`].
#[derive(Debug, thiserror::Error)]
pub enum FsCoordinationError {
    /// Leadership is held by another instance.
    #[error(
        "leadership not acquired for subscription '{subscription_name}': another instance holds the lock"
    )]
    LeadershipNotAcquired {
        /// The subscription whose leadership is held elsewhere.
        subscription_name: String,
    },
    /// An underlying filesystem error.
    #[error("coordination io error: {0}")]
    Io(#[from] std::io::Error),
}
