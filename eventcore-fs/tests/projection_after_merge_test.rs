//! Projection behaviour after a structural merge (ADR-0043).
//!
//! A `git merge` can union in a transaction whose canonical linearized position
//! falls *earlier* than a cursor a projection has already advanced past. A naive
//! cursor keyed on canonical position (the per-event UUID7) would silently skip
//! that event — read-model corruption. ADR-0043(c) requires the `EventReader`
//! cursor on the file store to track *local-ingestion order* instead: a
//! merge-introduced event is new to this replica, so it receives a fresh, larger
//! local-ingestion position and the cursor reaches it without ever rewinding.
//!
//! These tests drive a real cursor-based projection loop through the public
//! `EventReader` API and assert the no-miss / no-rewind guarantee, plus the
//! topology-generation rebuild safety net (ADR-0043(b)).

use std::path::Path;

use eventcore_fs::{FileEventStore, ResolutionOutcome};
use eventcore_types::{
    BatchSize, Event, EventFilter, EventPage, EventReader, EventStore, StreamId, StreamPosition,
    StreamVersion, StreamWrites,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

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
/// `git merge` performs), skipping files already present.
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

/// Drain a projection's view forward from `cursor`, returning the texts observed
/// (in delivery order) and the new cursor. Models one poll of a cursor-based
/// projection runner reading through the public `EventReader` API.
async fn drain(
    store: &FileEventStore,
    mut cursor: Option<StreamPosition>,
) -> (Vec<String>, Option<StreamPosition>) {
    let mut texts: Vec<String> = Vec::new();
    loop {
        let page = match cursor {
            None => EventPage::first(BatchSize::new(1000)),
            Some(position) => EventPage::after(position, BatchSize::new(1000)),
        };
        let batch = store
            .read_events::<Note>(EventFilter::all(), page)
            .await
            .expect("read_events");
        if batch.is_empty() {
            break;
        }
        for (note, position) in &batch {
            texts.push(note.text.clone());
            cursor = Some(*position);
        }
    }
    (texts, cursor)
}

#[tokio::test]
async fn live_projection_does_not_miss_a_merge_introduced_earlier_event() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-1").expect("stream id");

    // Shared ancestor written by A, cloned to B.
    {
        let a = FileEventStore::open(dir_a.path()).expect("open a");
        append(&a, &account, 0, "opened").await;
    }
    union_events(dir_a.path(), dir_b.path());

    // B writes its divergent event FIRST (earlier wall-clock => smaller UUID7),
    // so the event the merge later introduces on A is canonically *behind* the
    // cursor A will have advanced past. The sleep forces distinct millisecond
    // timestamps so the adverse ordering is deterministic.
    {
        let b = FileEventStore::open(dir_b.path()).expect("open b");
        append(&b, &account, 1, "from-B").await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a");
        append(&a, &account, 1, "from-A").await;
    }

    // A's live projection consumes everything it currently has.
    let cursor = {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a for projection");
        let (seen, cursor) = drain(&a, None).await;
        assert_eq!(
            seen,
            vec!["opened".to_string(), "from-A".to_string()],
            "A's projection sees its own history in local-ingestion order"
        );
        cursor
    };

    // The merge: B's files arrive at A, and A reconciles the fork, appending a
    // compensation event as a new head.
    union_events(dir_b.path(), dir_a.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen merged a");
        let _ = a
            .reconcile::<Note, _>(|context| {
                ResolutionOutcome::Resolve(vec![Note {
                    stream_id: context.stream_id().clone(),
                    text: "merged".to_string(),
                }])
            })
            .await
            .expect("reconcile");
    }

    // A's projection polls again from its saved cursor. The merge-introduced
    // "from-B" is canonically earlier and has a smaller UUID7 than the cursor,
    // but its local-ingestion position is larger, so the projection must still
    // observe it — and must not reprocess already-seen events.
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a for second poll");
        let (more, _) = drain(&a, cursor).await;
        assert!(
            more.contains(&"from-B".to_string()),
            "the merge-introduced earlier event must not be missed, saw {more:?}"
        );
        assert!(
            more.iter().any(|text| text == "merged"),
            "the compensation head event is observed, saw {more:?}"
        );
        assert!(
            !more.contains(&"opened".to_string()) && !more.contains(&"from-A".to_string()),
            "already-projected events are not reprocessed, saw {more:?}"
        );
    }
}

#[tokio::test]
async fn topology_rebuild_projection_matches_the_live_projection_state() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-1").expect("stream id");

    {
        let a = FileEventStore::open(dir_a.path()).expect("open a");
        append(&a, &account, 0, "opened").await;
    }
    union_events(dir_a.path(), dir_b.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a");
        append(&a, &account, 1, "from-A").await;
    }
    {
        let b = FileEventStore::open(dir_b.path()).expect("open b");
        append(&b, &account, 1, "from-B").await;
    }

    // A live projection consumes A's history before the merge, recording the
    // topology generation it has caught up to.
    let (mut live_state, mut cursor) = {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a for projection");
        let (seen, cursor) = drain(&a, None).await;
        (seen, cursor)
    };
    let generation_before = {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a for generation");
        a.topology_generation().expect("generation")
    };

    // The merge arrives and is reconciled — a structural merge.
    union_events(dir_b.path(), dir_a.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen merged a");
        let _ = a
            .reconcile::<Note, _>(|context| {
                ResolutionOutcome::Resolve(vec![Note {
                    stream_id: context.stream_id().clone(),
                    text: "merged".to_string(),
                }])
            })
            .await
            .expect("reconcile");
    }

    // Live projection (a): advances forward over the new events via its cursor.
    let generation_after = {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a after merge");
        let (more, _) = drain(&a, cursor.take()).await;
        live_state.extend(more);
        a.topology_generation().expect("generation")
    };

    // Rebuild projection (b): detects the topology generation changed and
    // rebuilds from zero against the full history.
    assert!(
        generation_after > generation_before,
        "a structural merge must bump the topology generation"
    );
    let rebuilt_state = {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a for rebuild");
        let (all, _) = drain(&a, None).await;
        all
    };

    // Both read models reach the same final state (order-independent): the
    // compensation makes the result correct whether or not the diverged events
    // were already projected (the app-author constraint, ADR-0043).
    let mut live_sorted = live_state.clone();
    live_sorted.sort();
    let mut rebuilt_sorted = rebuilt_state.clone();
    rebuilt_sorted.sort();
    assert_eq!(
        live_sorted, rebuilt_sorted,
        "live and rebuild-on-topology projections reach identical state"
    );
    assert_eq!(
        live_sorted,
        vec![
            "from-A".to_string(),
            "from-B".to_string(),
            "merged".to_string(),
            "opened".to_string(),
        ],
        "every event is observed exactly once"
    );
}
