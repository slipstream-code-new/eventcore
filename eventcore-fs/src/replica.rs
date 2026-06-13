//! Replica identity: fingerprint-bound, machine-local write identity (ADR-0044).
//!
//! Each writing working copy has a `replica_id` recorded in every transaction
//! header. The id is machine-local and gitignored, never committed — committing
//! it would duplicate one identity across every clone (the copy trap). To also
//! defend the `cp -r` path (which copies the gitignored `.eventcore/` too), the
//! id is bound to a working-copy **fingerprint**: an OS machine identifier, the
//! repository's absolute path, and the `.git` directory inode. On open, if the
//! recorded fingerprint no longer matches this environment, the id is
//! regenerated — so a naive `cp -r` to a new path gets a *different* id (a
//! detectable fork rather than silent sharing).

use std::fs;

use uuid::Uuid;

use crate::config::{FsConfig, Roots};
use crate::error::FsEventStoreError;

/// Compute the fingerprint of this working copy: machine id, absolute repo
/// path, and `.git` inode. Any change (notably a `cp -r` to a new path) yields
/// a different fingerprint, which triggers id regeneration on the next open.
fn compute_fingerprint(roots: &Roots) -> String {
    let machine = machine_id();
    let path = fs::canonicalize(&roots.root)
        .unwrap_or_else(|_| roots.root.clone())
        .to_string_lossy()
        .into_owned();
    let git = git_inode(roots);
    format!("machine={machine};path={path};git={git}")
}

fn machine_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(unix)]
fn git_inode(roots: &Roots) -> String {
    use std::os::unix::fs::MetadataExt as _;
    match fs::metadata(roots.root.join(".git")) {
        Ok(metadata) => metadata.ino().to_string(),
        Err(_) => "none".to_string(),
    }
}

#[cfg(not(unix))]
fn git_inode(_roots: &Roots) -> String {
    "none".to_string()
}

/// Resolve this working copy's `replica_id` for stamping new transactions.
///
/// - An explicit [`FsConfig::with_replica_id`] override is used verbatim and
///   bypasses the fingerprint/generation path.
/// - Otherwise the persisted id is reused only if its recorded fingerprint
///   still matches this environment; on a mismatch (a `cp -r`, a move, a
///   restored backup) or when absent, a fresh id is generated and recorded
///   alongside the current fingerprint.
pub(crate) fn load_or_create_replica_id(
    roots: &Roots,
    config: &FsConfig,
) -> Result<Uuid, FsEventStoreError> {
    if let Some(id) = config.replica_id {
        return Ok(id);
    }

    let current = compute_fingerprint(roots);
    let id_path = roots.replica_id_path();
    match fs::read_to_string(&id_path) {
        Ok(contents) => {
            let recorded =
                Uuid::parse_str(contents.trim()).map_err(|error| FsEventStoreError::Corrupted {
                    path: id_path.clone(),
                    detail: format!("invalid replica id: {error}"),
                })?;
            if fingerprint_matches(roots, &current)? {
                Ok(recorded)
            } else {
                regenerate(roots, &current)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => regenerate(roots, &current),
        Err(source) => Err(FsEventStoreError::InitFailed {
            path: id_path,
            source,
        }),
    }
}

fn fingerprint_matches(roots: &Roots, current: &str) -> Result<bool, FsEventStoreError> {
    let path = roots.replica_fingerprint_path();
    match fs::read_to_string(&path) {
        Ok(recorded) => Ok(recorded == current),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(FsEventStoreError::InitFailed { path, source }),
    }
}

fn regenerate(roots: &Roots, fingerprint: &str) -> Result<Uuid, FsEventStoreError> {
    let id = Uuid::now_v7();
    let id_path = roots.replica_id_path();
    fs::write(&id_path, id.to_string()).map_err(|source| FsEventStoreError::InitFailed {
        path: id_path,
        source,
    })?;
    let fingerprint_path = roots.replica_fingerprint_path();
    fs::write(&fingerprint_path, fingerprint).map_err(|source| FsEventStoreError::InitFailed {
        path: fingerprint_path,
        source,
    })?;
    Ok(id)
}
