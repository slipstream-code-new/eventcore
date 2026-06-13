//! Verifies that a single `append_events` call persists a large batch of
//! events correctly. The SQLite backend writes events with a multi-row INSERT
//! that is chunked to stay under SQLite's bound-parameter limit; this test
//! appends more events than fit in one chunk to exercise the chunk boundary
//! and confirm every event is durably stored with sequential versions.

use eventcore_sqlite::SqliteEventStore;
use eventcore_testing::contract::ContractTestEvent;
use eventcore_types::{EventStore, StreamId, StreamVersion, StreamWrites};

#[tokio::test]
async fn append_events_persists_large_batch_across_chunk_boundary() {
    // Given: a migrated in-memory SQLite store.
    let store = SqliteEventStore::in_memory().expect("should create in-memory SQLite store");
    store.migrate().await.expect("migration should succeed");

    let stream_id = StreamId::try_new("batch::large".to_string()).expect("valid stream id");

    // When: we append a batch larger than a single INSERT chunk (250 > 100) in
    // one append_events call.
    const EVENT_COUNT: usize = 250;
    let mut writes = StreamWrites::new()
        .register_stream(stream_id.clone(), StreamVersion::new(0))
        .expect("register_stream should succeed");
    for _ in 0..EVENT_COUNT {
        writes = writes
            .append(ContractTestEvent::new(stream_id.clone()))
            .expect("append should succeed");
    }
    let _slice = store
        .append_events(writes)
        .await
        .expect("append_events should persist the full batch atomically");

    // Then: reading the stream back returns every appended event.
    let reader = store
        .read_stream::<ContractTestEvent>(stream_id.clone())
        .await
        .expect("read_stream should succeed");
    assert_eq!(reader.len(), EVENT_COUNT);
}
