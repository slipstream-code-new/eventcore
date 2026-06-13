# ADR-0049: Streaming reads for `EventStore::read_stream`

## Status

Accepted

## Context

`EventStore::read_stream<E>(stream_id)` previously returned
`Result<EventStreamReader<E>, EventStoreError>`, where `EventStreamReader<E>`
was a thin wrapper around an eagerly-collected `Vec<E>`. Every backend read
the entire stream into memory before returning, and the executor folded the
materialized `Vec` into command state in a single pass.

For large streams (10K+ events) this means peak memory equal to the whole
stream materialized at once, and the executor cannot begin folding until the
last event has been loaded. The PostgreSQL backend made this worse by using
`fetch_all`, buffering the complete result set inside `sqlx` before EventCore
even saw it.

Issue #364 asks for streaming reads so that:

1. The backend pulls events incrementally rather than buffering the whole
   result set.
2. The executor folds events into command state as they arrive, so peak
   memory is bounded by the in-progress state, not the stream length.

This is a pre-1.0 library, so a clean breaking change to the `EventStore`
trait is preferable to bolting a lazy wrapper onto the existing collected
return type.

## Decision

### 1. `read_stream` returns an async stream

`EventStore::read_stream` now returns
`Result<EventStream<E>, EventStoreError>`. Opening the read may fail up front
(connection/setup), after which the returned `EventStream<E>` yields
`Result<E, EventStoreError>` per event in stream-version order
(oldest → newest). Per-event decode failures surface as `Err` items rather
than aborting the whole read up front.

`EventStream<E>` is a **named newtype** in `eventcore-types` wrapping
`Pin<Box<dyn futures::Stream<Item = Result<E, EventStoreError>> + Send>>`:

```rust
pub struct EventStream<E: Event> { /* boxed Send stream */ }
impl<E: Event> EventStream<E> {
    pub fn new(stream: impl Stream<Item = Result<E, EventStoreError>> + Send + 'static) -> Self;
}
impl<E: Event> Stream for EventStream<E> { /* delegates poll_next */ }
```

We chose a named newtype over raw RPITIT (`-> impl Future<Output = Result<impl
Stream<...> + Send, _>> + Send`) because the executor's effect plumbing
(`StoreEffectResult`) and the backend-wrapping mock stores (chaos,
deterministic, test doubles) all need to _name_ the stream type. A boxed
newtype keeps the trait usable as a plain generic bound `S: EventStore`,
keeps the effect enum nameable, and avoids the per-backend opaque-type
divergence that RPITIT would impose. The single `Box::pin` allocation per read
is negligible next to per-event database I/O.

### 2. `EventStreamReader` is removed; `collect_events` replaces the "I want them all" case

`EventStreamReader` is deleted. Callers that genuinely want the whole history
materialized use the free async helper:

```rust
pub async fn collect_events<E, S>(stream: S) -> Result<Vec<E>, EventStoreError>
where E: Event, S: Stream<Item = Result<E, EventStoreError>>;
```

It returns the first `Err` item immediately, matching the previous behavior
where one bad event failed the whole read. It is exported from both
`eventcore-types` and `eventcore`. Tests, small streams, and ad-hoc inspection
use it; the executor does NOT.

### 3. The executor folds incrementally (the memory win)

The execution pipeline keeps its `ReadStream` effect, but the shell now pumps
events into the pipeline one at a time instead of handing it a `Vec`:

- The shell opens the stream, then for each event calls
  `pipeline.resume(StoreEffectResult::StreamEvent(e))`. The pipeline folds that
  single event into the in-progress `state`, increments an `event_count`, and
  returns `PipelineStep::WaitForResult` — staying in its `AwaitingStreamRead`
  phase.
- On stream end the shell calls `resume(StoreEffectResult::StreamEnded)`, which
  sets `expected_version = StreamVersion::new(event_count)` (equivalent to the
  old `reader.len()`), runs dynamic stream discovery against the folded state,
  and advances.
- On an open failure or a per-event decode error the shell calls
  `resume(StoreEffectResult::StreamReadError(e))`, which completes the pipeline
  with `CommandError::EventStoreError`.

The fold stays inside the pipeline (it owns `self.command` and thus `apply`).
The shell only pulls events and pushes them in — it never collects the whole
stream into a `Vec`. Dynamic stream discovery, multi-stream handling, retries,
and error propagation are all preserved.

### 4. Per-backend streaming

- **PostgreSQL** (the real streaming win): uses `sqlx`'s lazy
  `query(...).fetch(&pool)` instead of `fetch_all`, mapping each row to a
  `Result<E, EventStoreError>` inside an `async_stream::stream!`. The pool (an
  `Arc` internally) is cloned into the stream so it is `'static`. A per-row
  deserialization failure is yielded as an `Err` item (preserving the
  `read_stream_errors_on_type_mismatch` contract).
- **SQLite**: rusqlite is synchronous. A `spawn_blocking` task reads rows and
  sends each row's JSON over a bounded `tokio::sync::mpsc` channel (capacity 64);
  the returned async stream consumes the channel and deserializes each row into
  `E`. This is genuinely incremental with backpressure. Deserialization happens
  on the async side so a type mismatch surfaces as an `Err` item.
- **In-memory**: events are stored type-erased behind a lock. The downcast +
  clone to produce owned `E` per item still happens (expected — see #363), so
  the per-event results are materialized while the lock is held, then the lock
  is released and the items are yielded one at a time via
  `futures::stream::iter`. The win here is API uniformity, not memory: the data
  already lives in memory.
- **File store**: same shape as in-memory — deserialize each indexed event
  while holding the `RwLock` read guard (preserving read-time linearization
  order), release the lock, and yield the per-event results incrementally.

The cross-backend `eventcore-testing` contract suite is the behavioral safety
net: order, isolation, missing-stream, atomicity, and type-mismatch contracts
all run unchanged against the streaming API (consuming via `collect_events`).

### Out of scope

`EventReader::read_events` (the paginated subscription read) is already
incremental via `EventPage`/limit and is unchanged.

## Consequences

### Positive

- Peak memory for command execution is bounded by the in-progress command
  state, not the stream length.
- PostgreSQL reads pull rows lazily; SQLite reads stream with backpressure.
- The `EventStore` trait stays a simple generic bound; the effect plumbing
  names a concrete `EventStream<E>`.
- `collect_events` keeps the ergonomic "give me everything" path for tests and
  small streams.

### Negative / trade-offs

- Breaking change to the public `EventStore` trait (acceptable pre-1.0). All
  backends, mocks, and call sites were migrated in lockstep.
- The in-memory and file backends do not gain a memory benefit — they still
  read their local data up front before yielding. This is acceptable: they are
  in-process stores whose data already resides in memory, and the issue targets
  not materializing a _second_ copy of the whole stream at the read boundary.
- One `Box::pin` allocation per `read_stream` call (negligible vs. I/O).

## Related

- Issue #364: streaming reads for large event streams
- ADR-0035: event schema evolution via enum variants (decode failures)
- #363: per-event clone in the in-memory downcast path (separate concern)
