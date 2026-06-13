---
name: event-sourcing
summary: Core event sourcing model with multi-stream atomic writes, optimistic concurrency, and immutable event storage.
---

# Event Sourcing Core

The foundational persistence model for eventcore. All state changes are recorded as immutable events in ordered streams, with atomic multi-stream writes and optimistic concurrency control.

## Overview

EventCore implements aggregateless event sourcing where commands can atomically read from and write to multiple event streams in a single transaction. Events are append-only, version-tracked per stream, and globally ordered via UUIDv7 positions.

## Architecture

### Write Path

1. **Stream Reading** — `EventStore::read_stream()` opens an async `EventStream<E>`
   that yields each event in stream-version order; the executor folds events
   incrementally as they arrive (peak memory is bounded by the in-progress
   command state, not the stream length — see ADR-0049)
2. **State Reconstruction** — Each streamed event is folded via `CommandLogic::apply()` into command state one at a time
3. **Business Logic** — `CommandLogic::handle()` validates preconditions and produces new events
4. **Atomic Append** — `EventStore::append_events()` persists all events atomically with version checks

### Read Path (Projections)

1. **Global Reading** — `EventReader::read_events()` reads events across all streams
2. **Filtering** — `EventFilter` supports all-events or stream-prefix matching
3. **Pagination** — `EventPage` provides cursor-based pagination to prevent memory exhaustion
4. **Checkpointing** — `CheckpointStore` persists projection progress for resumable processing

### Optimistic Concurrency

- Each stream tracks a `StreamVersion` (event count, starting at 0)
- On append, expected versions must match current stream versions
- Version conflicts return `EventStoreError::VersionConflict`, triggering automatic retry
- No locks are held during command execution — conflicts resolved via retry

### Immutability Guarantees

- Events cannot be modified or deleted after writing
- PostgreSQL backend enforces via database triggers (rejects UPDATE/DELETE)
- Multi-stream writes are all-or-nothing (ACID transactions)

## Key Types

| Type             | Purpose                                                            |
| ---------------- | ------------------------------------------------------------------ |
| `StreamId`       | Validated stream identifier (no glob chars, max 255 chars)         |
| `StreamVersion`  | Monotonic per-stream event count                                   |
| `StreamPosition` | UUIDv7-based global event position                                 |
| `StreamWrites`   | Builder for atomic multi-stream write batches                      |
| `EventStream<E>` | Async stream of `Result<E, _>` yielded by `read_stream` (ADR-0049) |
| `collect_events` | Helper that drains an `EventStream<E>` into a `Vec<E>`             |
| `EventFilter`    | Stream prefix filtering for projections                            |
| `EventPage`      | Cursor-based pagination                                            |

## Key Traits

| Trait                  | Purpose                                                                    |
| ---------------------- | -------------------------------------------------------------------------- |
| `Event`                | Domain events: `Clone + Send + Serialize + DeserializeOwned + stream_id()` |
| `EventStore`           | Read streams and append events atomically                                  |
| `EventReader`          | Read events globally for projections                                       |
| `CheckpointStore`      | Persist and resume projection progress                                     |
| `ProjectorCoordinator` | Single-leader election for projectors                                      |

## Files

| File                                | Description                                                    |
| ----------------------------------- | -------------------------------------------------------------- |
| `eventcore-types/src/store.rs`      | EventStore trait, stream types, StreamWrites builder, errors   |
| `eventcore-types/src/projection.rs` | EventReader, CheckpointStore, ProjectorCoordinator, pagination |
| `eventcore-types/src/command.rs`    | Event trait definition                                         |

## Event Schema Evolution

Event schemas evolve via **new enum variants**, not upcasting or migration.
See ADR-0035 for full rationale.

### Additive Changes

New optional fields use `#[serde(default)]`. Old events deserialize with
the default value.

### Incompatible Changes

Add a new variant (e.g., `MoneyDepositedV2`). Old variants remain — they
represent historical facts. `apply()` and projectors handle all variants
via exhaustive pattern matching. `handle()` emits only the latest variant.

### Storage Format

Events are stored as JSON using serde's externally-tagged enum format.
The `event_type` column is informational metadata (auditing/debugging),
not used for deserialization routing.

## Related Systems

- [command-execution](command-execution.md) — Orchestrates the read-validate-write cycle
- [projection-system](projection-system.md) — Builds read models from the event stream
- [store-backends](store-backends.md) — Backend implementations of these traits
- ADR-001: Multi-stream atomicity
- ADR-007: Optimistic concurrency control
- ADR-012: Event trait for domain-first design
- ADR-017: StreamId reserved characters
- ADR-035: Event schema evolution via enum variants
- ADR-0049: Streaming reads for `read_stream` (incremental fold, `EventStream<E>`, `collect_events`)
