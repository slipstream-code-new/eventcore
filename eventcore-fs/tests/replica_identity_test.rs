//! Replica identity hardening for merge mode (ADR-0044).
//!
//! A working copy's `replica_id` is machine-local and gitignored, generated on
//! first write. Two layered defenses against the "copy trap" — where a `cp -r`
//! of a working tree duplicates a live identity and makes divergent forks
//! invisible — are exercised here:
//!
//! 1. **Fingerprint binding.** The id is bound to a working-copy fingerprint
//!    (machine id + absolute path + `.git` inode). A `cp -r` to a new path no
//!    longer matches the recorded fingerprint, so the copy regenerates a fresh
//!    id — a detectable fork rather than silent sharing.
//! 2. **Reconcile-time collision check.** If two concurrent transactions carry
//!    the same `replica_id` but cannot have come from one linear writer, the
//!    store fails loud with `ReplicaIdentityConflict` rather than silently
//!    merging a corrupt history.

use std::path::Path;
use std::process::Command;

use eventcore_fs::{FileEventStore, FsConfig, FsEventStoreError};
use eventcore_types::{Event, EventStore, StreamId, StreamVersion, StreamWrites};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    stream_id: StreamId,
    text: String,
}

impl Event for Note {
    fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    fn event_type_name() -> &'static str {
        "Note"
    }
}

async fn append(store: &FileEventStore, stream_id: &StreamId, expected: usize, text: &str) {
    let writes = StreamWrites::new()
        .register_stream(stream_id.clone(), StreamVersion::new(expected))
        .and_then(|writes| {
            writes.append(Note {
                stream_id: stream_id.clone(),
                text: text.to_string(),
            })
        })
        .expect("build writes");
    let _ = store.append_events(writes).await.expect("append succeeds");
}

/// Copy every transaction file from `src/events` into `dst/events` (the union a
/// `git merge` performs).
fn union_events(src: &Path, dst: &Path) {
    let dst_events = dst.join("events");
    std::fs::create_dir_all(&dst_events).expect("dst events dir");
    for entry in std::fs::read_dir(src.join("events")).expect("read src events") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let name = path.file_name().expect("file name");
            let target = dst_events.join(name);
            if !target.exists() {
                let _ = std::fs::copy(&path, &target).expect("copy transaction file");
            }
        }
    }
}

#[tokio::test]
async fn cp_r_of_a_working_tree_yields_a_distinct_replica_id() {
    let dir = TempDir::new().expect("temp dir");
    let copy_parent = TempDir::new().expect("copy parent");
    let copy_path = copy_parent.path().join("clone");
    let account = StreamId::try_new("account-1").expect("stream id");

    // Original working copy writes once, minting its identity.
    let original_id = {
        let store = FileEventStore::open(dir.path()).expect("open original");
        append(&store, &account, 0, "opened").await;
        store.replica_id()
    };

    // A naive `cp -r` duplicates the whole tree, INCLUDING the gitignored
    // .eventcore/ directory — the copy trap.
    let status = Command::new("cp")
        .arg("-r")
        .arg(dir.path())
        .arg(&copy_path)
        .status()
        .expect("cp -r");
    assert!(status.success(), "cp -r succeeded");

    // Opening the copy at its new path detects the fingerprint mismatch and
    // regenerates a fresh identity, so its next write is correctly attributed
    // to a distinct replica.
    let copy_store = FileEventStore::open(&copy_path).expect("open copy");
    append(&copy_store, &account, 1, "from-copy").await;
    let copy_id = copy_store.replica_id();

    assert_ne!(
        original_id, copy_id,
        "a cp -r copy must regenerate a distinct replica id (copy trap defense)"
    );

    // The original, reopened at its own unchanged path, keeps its identity.
    let reopened = FileEventStore::open(dir.path()).expect("reopen original");
    assert_eq!(
        reopened.replica_id(),
        original_id,
        "an unchanged working copy keeps its replica id across reopen"
    );
}

#[tokio::test]
async fn concurrent_transactions_sharing_a_replica_id_surface_a_conflict() {
    // Force the catastrophic case the fingerprint defense normally prevents: two
    // independent writers configured (via explicit override — a botched manual
    // provisioning) with the SAME replica id, producing concurrent transactions.
    let shared = Uuid::now_v7();
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-1").expect("stream id");

    {
        let a =
            FileEventStore::open_with_config(FsConfig::new(dir_a.path()).with_replica_id(shared))
                .expect("open a");
        append(&a, &account, 0, "opened").await;
    }
    union_events(dir_a.path(), dir_b.path());
    {
        let a =
            FileEventStore::open_with_config(FsConfig::new(dir_a.path()).with_replica_id(shared))
                .expect("reopen a");
        append(&a, &account, 1, "from-A").await;
    }
    {
        let b =
            FileEventStore::open_with_config(FsConfig::new(dir_b.path()).with_replica_id(shared))
                .expect("open b");
        append(&b, &account, 1, "from-B").await;
    }
    union_events(dir_b.path(), dir_a.path());

    let store =
        FileEventStore::open_with_config(FsConfig::new(dir_a.path()).with_replica_id(shared))
            .expect("open merged");

    let detected = store.detect_forks();
    assert!(
        matches!(
            detected,
            Err(FsEventStoreError::ReplicaIdentityConflict { .. })
        ),
        "two concurrent transactions sharing a replica id must surface \
         ReplicaIdentityConflict, got {detected:?}"
    );
}
