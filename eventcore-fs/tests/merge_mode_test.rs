//! Layer 2 merge mode: deterministic convergence after a simulated `git merge`.
//!
//! A `git merge` of two clones' `events/` directories is a pure additive union
//! of immutable, uniquely named files. These tests reproduce that union by
//! copying transaction files between store roots and assert the ADR-0039
//! convergence guarantee: every clone holding the same file set computes the
//! identical canonical order.

use std::fs;
use std::path::Path;

use eventcore_fs::{FileEventStore, ResolutionOutcome};
use eventcore_types::{
    BatchSize, Event, EventFilter, EventPage, EventReader, EventStore, StreamId, StreamVersion,
    StreamWrites,
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
    fs::create_dir_all(&dst_events).expect("dst events dir");
    for entry in fs::read_dir(src.join("events")).expect("read src events") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let name = path.file_name().expect("file name");
            let target = dst_events.join(name);
            if !target.exists() {
                let _ = fs::copy(&path, &target).expect("copy transaction file");
            }
        }
    }
}

async fn canonical_order(root: &Path) -> Vec<(String, String)> {
    let store = FileEventStore::open(root).expect("open");
    let events = store
        .read_events::<Note>(EventFilter::all(), EventPage::first(BatchSize::new(1000)))
        .await
        .expect("read_events");
    events
        .into_iter()
        .map(|(note, _position)| (note.stream_id.as_ref().to_string(), note.text))
        .collect()
}

#[tokio::test]
async fn two_clones_converge_after_union_merge() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-1").expect("stream id");

    // A writes the shared ancestor.
    {
        let a = FileEventStore::open(dir_a.path()).expect("open a");
        append(&a, &account, 0, "opened").await;
    }
    // B "clones" A by unioning A's events, then both diverge offline from v1.
    union_events(dir_a.path(), dir_b.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a");
        append(&a, &account, 1, "from-A").await;
    }
    {
        let b = FileEventStore::open(dir_b.path()).expect("open b");
        append(&b, &account, 1, "from-B").await;
    }

    // Merge both directions — git merge unions the files.
    union_events(dir_a.path(), dir_b.path());
    union_events(dir_b.path(), dir_a.path());

    let order_a = canonical_order(dir_a.path()).await;
    let order_b = canonical_order(dir_b.path()).await;

    assert_eq!(
        order_a, order_b,
        "clones holding the same file set must converge to identical canonical order"
    );
    assert_eq!(order_a.len(), 3, "ancestor plus both divergent events");
    assert_eq!(order_a[0].1, "opened", "the causal ancestor sorts first");
}

#[tokio::test]
async fn convergence_is_independent_of_merge_direction() {
    // Build a fork, then assert that a clone which received the files in the
    // opposite order still computes the same canonical order as the forward
    // case — order depends only on file content, never on arrival order.
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let stream = StreamId::try_new("doc-1").expect("stream id");

    {
        let a = FileEventStore::open(dir_a.path()).expect("open a");
        append(&a, &stream, 0, "root").await;
    }
    union_events(dir_a.path(), dir_b.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a");
        append(&a, &stream, 1, "branch-A").await;
    }
    {
        let b = FileEventStore::open(dir_b.path()).expect("open b");
        append(&b, &stream, 1, "branch-B").await;
    }
    union_events(dir_a.path(), dir_b.path());
    union_events(dir_b.path(), dir_a.path());

    // A third clone receives the files via B, not A.
    let dir_c = TempDir::new().expect("temp c");
    union_events(dir_b.path(), dir_c.path());

    let order_a = canonical_order(dir_a.path()).await;
    let order_c = canonical_order(dir_c.path()).await;
    assert_eq!(order_a, order_c, "canonical order is content-determined");
}

#[tokio::test]
async fn detect_forks_reports_divergent_stream() {
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
    union_events(dir_b.path(), dir_a.path());

    let store = FileEventStore::open(dir_a.path()).expect("open merged");
    let forks = store.detect_forks().expect("detect forks");
    assert_eq!(forks.len(), 1, "exactly one diverged stream");
    assert_eq!(forks[0].stream_id().as_ref(), "account-1");
    assert_eq!(forks[0].base_version(), StreamVersion::new(1));
    assert_eq!(
        forks[0].transactions().len(),
        2,
        "two concurrent transactions extended the stream from v1"
    );

    let status = store.status().expect("status");
    assert!(!status.is_clean(), "a forked store is not clean");
    assert_eq!(status.forks().len(), 1);
}

#[tokio::test]
async fn linear_history_has_no_forks() {
    let dir = TempDir::new().expect("temp dir");
    let stream = StreamId::try_new("ledger-1").expect("stream id");
    let store = FileEventStore::open(dir.path()).expect("open");
    append(&store, &stream, 0, "first").await;
    append(&store, &stream, 1, "second").await;

    assert!(
        store.detect_forks().expect("detect").is_empty(),
        "a linear single-writer history never forks"
    );
    assert!(store.status().expect("status").is_clean());
}

async fn read_texts(store: &FileEventStore) -> Vec<String> {
    store
        .read_events::<Note>(EventFilter::all(), EventPage::first(BatchSize::new(1000)))
        .await
        .expect("read_events")
        .into_iter()
        .map(|(note, _position)| note.text)
        .collect()
}

#[tokio::test]
async fn reconcile_collapses_fork_and_converges() {
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
    union_events(dir_b.path(), dir_a.path());

    let store = FileEventStore::open(dir_a.path()).expect("open merged");
    assert_eq!(
        store.detect_forks().expect("detect").len(),
        1,
        "fork present"
    );

    // Domain-owned resolution: keep both notes and append a merge note.
    let report = store
        .reconcile::<Note, _>(|context| {
            let texts: Vec<String> = context
                .branches()
                .iter()
                .flat_map(|branch| branch.events().iter().map(|note| note.text.clone()))
                .collect();
            ResolutionOutcome::Resolve(vec![Note {
                stream_id: context.stream_id().clone(),
                text: format!("merged:{}", texts.join("+")),
            }])
        })
        .await
        .expect("reconcile");

    assert_eq!(report.resolved_count(), 1);
    assert!(report.unresolved_streams().is_empty());
    assert!(
        store.detect_forks().expect("detect after").is_empty(),
        "the fork is resolved by the merge transaction"
    );

    let reader = store
        .read_stream::<Note>(account.clone())
        .await
        .expect("read stream");
    assert_eq!(reader.len(), 4, "ancestor + both branches + resolution");
    let stream_texts: Vec<String> = reader.iter().map(|note| note.text.clone()).collect();
    assert_eq!(stream_texts[0], "opened");
    assert!(
        stream_texts[3].starts_with("merged:"),
        "resolution event sorts last"
    );
    assert!(
        stream_texts[3].contains("from-A") && stream_texts[3].contains("from-B"),
        "the resolver saw both branches' events: {}",
        stream_texts[3]
    );

    // The merge transaction replicates to B (git union); B sees it resolved
    // and converges to the identical canonical order.
    union_events(dir_a.path(), dir_b.path());
    let b = FileEventStore::open(dir_b.path()).expect("reopen b");
    assert!(
        b.detect_forks().expect("detect b").is_empty(),
        "B also sees the fork resolved"
    );
    assert_eq!(
        read_texts(&store).await,
        read_texts(&b).await,
        "both clones converge after reconcile"
    );
}

#[tokio::test]
async fn topology_generation_increases_when_a_merge_is_recorded() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-5").expect("stream id");

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
    union_events(dir_b.path(), dir_a.path());

    let store = FileEventStore::open(dir_a.path()).expect("open merged");
    assert_eq!(
        store.topology_generation().expect("generation"),
        0,
        "no merges recorded yet, even though a fork exists"
    );

    let _ = store
        .reconcile::<Note, _>(|context| {
            ResolutionOutcome::Resolve(vec![Note {
                stream_id: context.stream_id().clone(),
                text: "merged".to_string(),
            }])
        })
        .await
        .expect("reconcile");

    assert_eq!(
        store.topology_generation().expect("generation"),
        1,
        "recording the merge transaction bumps the topology generation"
    );
}

#[tokio::test]
async fn reconcile_can_decline_a_fork() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let stream = StreamId::try_new("doc-9").expect("stream id");

    {
        let a = FileEventStore::open(dir_a.path()).expect("open a");
        append(&a, &stream, 0, "base").await;
    }
    union_events(dir_a.path(), dir_b.path());
    {
        let a = FileEventStore::open(dir_a.path()).expect("reopen a");
        append(&a, &stream, 1, "a").await;
    }
    {
        let b = FileEventStore::open(dir_b.path()).expect("open b");
        append(&b, &stream, 1, "b").await;
    }
    union_events(dir_b.path(), dir_a.path());

    let store = FileEventStore::open(dir_a.path()).expect("open merged");
    let report = store
        .reconcile::<Note, _>(|_context| {
            ResolutionOutcome::Unresolvable("needs a human".to_string())
        })
        .await
        .expect("reconcile");

    assert_eq!(report.resolved_count(), 0);
    assert_eq!(report.unresolved_streams().len(), 1);
    assert!(
        !store.detect_forks().expect("detect").is_empty(),
        "a declined fork remains unresolved"
    );
}

#[tokio::test]
async fn merge_of_merges_converges_and_terminates() {
    let dir_a = TempDir::new().expect("temp a");
    let dir_b = TempDir::new().expect("temp b");
    let account = StreamId::try_new("account-2").expect("stream id");

    // Shared ancestor, then a fork visible to both clones.
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
    union_events(dir_a.path(), dir_b.path());
    union_events(dir_b.path(), dir_a.path());

    // Both clones reconcile the SAME fork independently → two merge nodes.
    {
        let a = FileEventStore::open(dir_a.path()).expect("a");
        let _ = a
            .reconcile::<Note, _>(|context| {
                ResolutionOutcome::Resolve(vec![Note {
                    stream_id: context.stream_id().clone(),
                    text: "merged".to_string(),
                }])
            })
            .await
            .expect("a reconcile");
    }
    {
        let b = FileEventStore::open(dir_b.path()).expect("b");
        let _ = b
            .reconcile::<Note, _>(|context| {
                ResolutionOutcome::Resolve(vec![Note {
                    stream_id: context.stream_id().clone(),
                    text: "merged".to_string(),
                }])
            })
            .await
            .expect("b reconcile");
    }

    // Union the two independent merges — a fork of merge nodes.
    union_events(dir_a.path(), dir_b.path());
    union_events(dir_b.path(), dir_a.path());

    let store = FileEventStore::open(dir_a.path()).expect("merged");
    assert!(
        !store.detect_forks().expect("detect").is_empty(),
        "two independent reconciliations are themselves a fork"
    );

    // Reconciling recursively terminates at a single resolved head.
    let _ = store
        .reconcile::<Note, _>(|context| {
            ResolutionOutcome::Resolve(vec![Note {
                stream_id: context.stream_id().clone(),
                text: "final".to_string(),
            }])
        })
        .await
        .expect("reconcile merges");
    assert!(
        store.detect_forks().expect("detect again").is_empty(),
        "recursion terminates: a single resolved head remains"
    );

    union_events(dir_a.path(), dir_b.path());
    let b = FileEventStore::open(dir_b.path()).expect("reopen b");
    assert_eq!(
        read_texts(&store).await,
        read_texts(&b).await,
        "merge-of-merges converges across clones"
    );
}
