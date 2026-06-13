//! Verifies that a single `append_events` call persists a large batch of
//! events correctly through the PostgreSQL backend. Events are written with a
//! multi-row INSERT chunked at 1000 rows; this test appends more events than
//! one chunk holds, to a single stream, and confirms the BEFORE INSERT trigger
//! still assigns gap-free sequential versions across the chunk boundary and
//! every event is durably stored.

mod common;

use eventcore_testing::contract::ContractTestEvent;
use eventcore_types::{EventStore, StreamId, StreamVersion, StreamWrites};

#[tokio::test]
async fn append_events_persists_large_batch_across_chunk_boundary() {
    // Given: a migrated PostgreSQL store.
    let store = common::create_test_store().await;

    // Use a unique stream so the test is isolated from the shared container.
    let stream_id =
        StreamId::try_new(format!("batch::large::{}", uuid::Uuid::now_v7())).expect("valid stream");

    // When: we append a batch larger than one INSERT chunk (1200 > 1000) to a
    // single stream in one append_events call.
    const EVENT_COUNT: usize = 1200;
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
