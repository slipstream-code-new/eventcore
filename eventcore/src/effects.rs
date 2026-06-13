use std::time::Duration;

use eventcore_types::{Event, EventStoreError, EventStreamSlice, StreamId, StreamWrites};

/// Internal effect yielded by the execute pipeline state machine.
///
/// These effects describe store operations that the shell loop dispatches
/// to the `EventStore` trait. The state machine never performs I/O directly.
#[derive(Debug)]
pub(crate) enum StoreEffect {
    /// Open a stream and fold its events into command state incrementally.
    ///
    /// The shell opens the stream via `EventStore::read_stream`, then pumps
    /// events into the pipeline one at a time (via `StoreEffectResult::StreamEvent`)
    /// so the executor folds them without ever materializing the whole stream
    /// in memory. This is the memory win behind issue #364.
    ReadStream { stream_id: StreamId },
    /// Atomically append events to one or more streams.
    AppendEvents { writes: StreamWrites },
    /// Sleep for a retry backoff duration.
    Sleep { duration: Duration },
}

/// Result of dispatching a `StoreEffect` to the backend.
///
/// A single `ReadStream` effect is resolved by a sequence of results: zero or
/// more `StreamEvent` (each folded into state immediately), terminated by
/// either `StreamEnded` (success) or `StreamReadError` (open or per-event
/// failure). The pipeline stays in its awaiting-read phase across the
/// `StreamEvent` pushes and only advances on the terminator.
pub(crate) enum StoreEffectResult<E: Event> {
    /// A single event streamed from the current stream, to be folded into state.
    StreamEvent(E),
    /// The current stream has been fully consumed (all events folded).
    StreamEnded,
    /// Opening or reading the current stream failed.
    StreamReadError(EventStoreError),
    /// Result of an `AppendEvents` effect.
    EventsAppended(Result<EventStreamSlice, EventStoreError>),
    /// Sleep completed.
    Slept,
}
