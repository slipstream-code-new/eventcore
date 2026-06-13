//! Git-integration robustness: read-time fsck and dangling-transaction
//! handling (ADR-0046).
//!
//! `merge=union` keeps a `git merge` of `events/` conflict-free for the additive
//! case, but it can *mask* an illegal in-place edit of a JSONL file. A read-time
//! content-hash fsck catches that: a transaction whose payload no longer matches
//! the integrity anchor in its header is rejected and surfaced via `status()`.
//! Separately, a partial or aborted `git merge` can leave a transaction whose
//! `parent_transaction_ids` reference files that did not arrive; such a
//! transaction is reported as a `DanglingTransaction` rather than crashing or
//! being silently dropped.

use std::path::{Path, PathBuf};

use eventcore_fs::{FileEventStore, FsConfig, FsyncPolicy, TransactionId};
use eventcore_types::{Event, EventStore, StreamId, StreamVersion, StreamWrites, collect_events};
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

fn transaction_files(events_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(events_dir)
        .expect("read events dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

fn transaction_id_of(path: &Path) -> TransactionId {
    let stem = path.file_stem().and_then(|s| s.to_str()).expect("stem");
    TransactionId::new(Uuid::parse_str(stem).expect("uuid stem"))
}

#[tokio::test]
async fn a_hand_edited_transaction_file_is_rejected_by_fsck() {
    let dir = TempDir::new().expect("temp dir");
    let account = StreamId::try_new("account-1").expect("stream id");

    {
        // No fsync keeps the test fast; integrity is independent of durability.
        let store = FileEventStore::open_with_config(
            FsConfig::new(dir.path()).with_fsync(FsyncPolicy::None),
        )
        .expect("open");
        append(&store, &account, 0, "genuine").await;
    }

    // Hand-edit the event payload, leaving the header's integrity anchor stale —
    // exactly the illegal edit a union merge would silently splice in.
    let events_dir = dir.path().join("events");
    let file = transaction_files(&events_dir).remove(0);
    let original = std::fs::read_to_string(&file).expect("read txn");
    let tampered = original.replace("genuine", "tampered");
    assert_ne!(original, tampered, "the edit changed the payload");
    std::fs::write(&file, &tampered).expect("write tampered");

    // Reopening must not crash. The tampered transaction is rejected and
    // surfaced via status(), not parsed as trustworthy.
    let store = FileEventStore::open(dir.path()).expect("reopen does not crash");
    let status = store.status().expect("status");
    assert_eq!(
        status.integrity_failures().len(),
        1,
        "the hand-edited transaction is reported as an integrity failure"
    );
    assert_eq!(
        status.integrity_failures()[0].transaction_id(),
        transaction_id_of(&file),
        "the failure names the tampered transaction"
    );
    assert!(
        !status.is_clean(),
        "a store with a tampered file is not clean"
    );

    let event_stream = store
        .read_stream::<Note>(account)
        .await
        .expect("read does not panic");
    let notes: Vec<Note> = collect_events(event_stream).await.expect("collect");
    assert_eq!(
        notes.len(),
        0,
        "the tampered transaction is excluded from the linearized history"
    );
}

#[tokio::test]
async fn a_transaction_referencing_an_absent_parent_is_reported_as_dangling() {
    let dir = TempDir::new().expect("temp dir");
    let account = StreamId::try_new("account-1").expect("stream id");

    {
        let store = FileEventStore::open_with_config(
            FsConfig::new(dir.path()).with_fsync(FsyncPolicy::None),
        )
        .expect("open");
        append(&store, &account, 0, "first").await;
        append(&store, &account, 1, "second").await;
    }

    // Simulate a partial/aborted git merge: the parent transaction's file did
    // not come across, but its child did.
    let events_dir = dir.path().join("events");
    let files = transaction_files(&events_dir);
    let parent_id = transaction_id_of(&files[0]);
    let child_id = transaction_id_of(&files[1]);
    std::fs::remove_file(&files[0]).expect("remove parent file");

    let store = FileEventStore::open(dir.path()).expect("reopen does not crash");
    let status = store.status().expect("status");
    let dangling = status.dangling();
    assert_eq!(dangling.len(), 1, "exactly one dangling transaction");
    assert_eq!(
        dangling[0].transaction_id(),
        child_id,
        "the child whose parent is absent is reported as dangling"
    );
    assert!(
        dangling[0].missing_parents().contains(&parent_id),
        "the dangling report names the absent parent"
    );

    // Reads remain robust — the child's events are still readable.
    let event_stream = store
        .read_stream::<Note>(account)
        .await
        .expect("read does not panic");
    let notes: Vec<Note> = collect_events(event_stream).await.expect("collect");
    assert!(
        notes.iter().any(|note| note.text == "second"),
        "the dangling transaction's events are not silently dropped"
    );
}
