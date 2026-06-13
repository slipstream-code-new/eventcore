---
name: store-backends
summary: Pluggable EventStore implementations for PostgreSQL, SQLite, in-memory testing, and git-mergeable files.
---

# Store Backends

Four interchangeable implementations of the event sourcing traits. Domain code is backend-agnostic; the choice of backend is a deployment decision.

## Overview

Each backend implements `EventStore`, `EventReader`, `CheckpointStore`, and `ProjectorCoordinator`. The in-memory backend is for testing, SQLite for embedded/single-instance, PostgreSQL for production distributed deployments, and the file backend (`eventcore-fs`) for local-first / git-backed tools that need offline collaboration via `git merge`.

## Trait Implementation Matrix

| Trait                  | PostgreSQL                      | SQLite                           | In-Memory             | File (`eventcore-fs`)                        |
| ---------------------- | ------------------------------- | -------------------------------- | --------------------- | -------------------------------------------- |
| `EventStore`           | ACID transactions               | WAL mode                         | Mutex-guarded HashMap | One immutable JSONL file per transaction     |
| `EventReader`          | SQL queries with UUID7 ordering | SQL queries                      | Global log scan       | Read-time linearization of a transaction DAG |
| `CheckpointStore`      | Dedicated table                 | Dedicated table                  | HashMap               | Per-subscription JSON files (gitignored)     |
| `ProjectorCoordinator` | Advisory locks (distributed)    | In-memory locks (single-process) | In-memory locks       | OS advisory file locks (`fs4`)               |

The file backend additionally exposes a **merge mode** — fork detection and domain-owned reconciliation of histories combined via `git merge` — as file-store-specific API _outside_ the shared traits (ADR-0045). See the [fs-merge-mode](fs-merge-mode.md) blueprint.

## Streaming Reads (ADR-0049)

`EventStore::read_stream` returns `Result<EventStream<E>, EventStoreError>`,
where `EventStream<E>` is an async stream yielding `Result<E, EventStoreError>`
per event in stream-version order. Opening may fail up front; per-event decode
failures surface as `Err` items. The executor folds events incrementally as
they arrive rather than collecting the whole stream. `collect_events(stream)`
is the helper for callers that want a `Vec`.

Per-backend streaming behavior:

| Backend    | Read strategy                                                                                                                            |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| PostgreSQL | Lazy `sqlx` `query(...).fetch(&pool)` over a cloned pool — rows pulled incrementally (real win)                                          |
| SQLite     | `spawn_blocking` reads rows and sends them over a bounded `tokio::sync::mpsc` channel (backpressure); the async stream deserializes each |
| In-Memory  | Per-event downcast + clone materialized under the lock, then yielded one at a time (data already in memory)                              |
| File       | Per-event deserialize under the index read guard preserving linearization order, then yielded one at a time                              |

A per-row/per-event decode failure is yielded as `EventStoreError::DeserializationFailed`, preserving the cross-backend type-mismatch contract.

## PostgreSQL Backend

**Connection:** `sqlx` pool with configurable max connections, timeouts.

**Schema:**

- `eventcore_events` — stream_id, stream_version, event_type, event_data (JSON), event_id (UUID7)
- `eventcore_subscription_versions` — subscription_name, last_position (UUID7)

**Key features:**

- Database triggers enforce immutability (reject UPDATE/DELETE)
- Version checking via session-level config + trigger (atomic within transaction)
- Advisory locks (`pg_try_advisory_lock`) for distributed projector coordination
- FNV-1a hash of subscription name for lock key
- Guard releases lock via `pg_advisory_unlock()` on drop

**Files:** `eventcore-postgres/src/lib.rs`

## SQLite Backend

**Connection:** `rusqlite` with `Arc<Mutex<Connection>>`, async via `spawn_blocking`.

**Schema:** Same logical structure as PostgreSQL, TEXT columns for UUIDs/timestamps.

**Key features:**

- Optional SQLCipher encryption via `apply_encryption_key()` (opt-in `encryption` feature)
- Vendored vanilla SQLite by default via `bundled` feature; disable defaults to bring your own
- `rusqlite` re-exported at crate root so consumers don't redeclare it
- `from_connection` constructor on `SqliteEventStore` and `SqliteCheckpointStore`
  for bring-your-own-connection setups
- WAL journaling mode for better read concurrency (applied to crate-built connections)
- Manual version checking (no trigger-based validation)
- In-memory coordination locks (single-process only)

**Files:** `eventcore-sqlite/src/lib.rs`

## In-Memory Backend

**Storage:** `Mutex<StoreData>` with `HashMap<StreamId, Vec<Box<dyn Any>>>` per stream and a `Vec<GlobalLogEntry>` for global ordering.

**Key features:**

- Type erasure via `Box<dyn Any + Send>` with downcast on read
- Zero dependencies beyond std
- All-or-nothing atomicity via single mutex lock
- Suitable for testing and single-process development

**Files:** `eventcore-memory/src/lib.rs`

## File Backend (`eventcore-fs`)

**Storage:** One immutable JSONL file per `append_events` transaction under
`events/`, named by a transaction UUID7. Line 1 is a header (transaction id,
replica id, parent transaction ids, `created_at`, per-stream base versions);
lines 2..N are one event envelope each. Only `events/` is committed to git; the
in-memory index, checkpoints, locks, replica id, and tmp staging are gitignored.

**Key features:**

- All-or-nothing atomicity via tmp-write → fsync → atomic `rename` → dir-fsync;
  one transaction file is the atomic multi-stream unit (ADR-0038)
- `StreamVersion` and global order are computed at read time by linearizing a
  transaction DAG, not read from a stored column (ADR-0039); single-writer mode
  is the degenerate linear-chain case that passes the contract suite unchanged
- In-process append mutex + cross-process `.lock` + per-subscription OS advisory
  locks via `fs4` (ADR-0040)
- **Merge mode** (off-trait, ADR-0045): `detect_forks` / `reconcile` / `status`
  reconcile histories combined via `git merge`, with deterministic linearization
  and domain-owned resolution (ADR-0041 through ADR-0046)

**Files:** `eventcore-fs/src/lib.rs`

## Common Patterns

All backends share:

- JSON serialization for event storage
- UUID7 for global event ordering
- Per-stream version tracking (event count from 0)
- `EventFilter`-based filtering for EventReader, applied **before** the pagination
  `LIMIT` so non-matching events never consume batch slots
- Checkpoint-based resumable projection processing

### EventFilter Pushdown

`EventFilter` selects events by stream identity (literal `StreamPrefix` **or** glob
`StreamPattern`, mutually exclusive) and optional event type. Each backend pushes the
predicate down to its query layer (ADR-0047):

| Backend    | Prefix              | Glob pattern                                       |
| ---------- | ------------------- | -------------------------------------------------- |
| PostgreSQL | `stream_id LIKE $n` | `stream_id ~ $n` (anchored, injection-safe regex)  |
| SQLite     | `stream_id LIKE ?`  | `stream_id GLOB ?` (native POSIX glob)             |
| In-memory  | `starts_with`       | `StreamPattern::matches` (in-process, before take) |
| File       | `starts_with`       | `StreamPattern::matches` (in-process, before take) |

The Postgres glob→regex translation maps `*`→`.*`, `?`→`.`, keeps `[...]` character
classes (`[!`→`[^`), escapes all regex metacharacters in literal segments, and
anchors with `^...$`. Postgres and SQLite build their `read_events` query
dynamically so prefix XOR pattern + cursor + event_type compose without a
combinatorial set of hand-written query strings.

## Deployment Patterns

**Development/Testing:** InMemoryEventStore — zero dependencies, instant setup.

**Embedded/Single-Process:** SqliteEventStore — persistence for CLI tools, desktop
apps, single-instance servers. Optional SQLCipher encryption. In-memory coordination
only (no distributed leader election).

**Production/Distributed:** PostgresEventStore — ACID transactions, advisory locks
for distributed projector coordination, connection pooling.

```toml
# Development (default)
eventcore = "0.6"

# Production with PostgreSQL
eventcore = { version = "0.6", features = ["postgres"] }

# Embedded with SQLite
eventcore = { version = "0.6", features = ["sqlite"] }
```

## Files

| File                            | Description                                   |
| ------------------------------- | --------------------------------------------- |
| `eventcore-postgres/src/lib.rs` | PostgresEventStore + all trait impls          |
| `eventcore-sqlite/src/lib.rs`   | SqliteEventStore + all trait impls            |
| `eventcore-memory/src/lib.rs`   | InMemoryEventStore + all trait impls          |
| `eventcore-fs/src/lib.rs`       | FileEventStore + all trait impls + merge mode |

## Related Systems

- [event-sourcing](event-sourcing.md) — Traits these backends implement
- [testing-infrastructure](testing-infrastructure.md) — Contract tests that verify backend correctness
- [fs-merge-mode](fs-merge-mode.md) — Git-mergeable reconciliation layered on the file backend
- ADR-011: In-memory store crate location
- ADR-022: Crate reorganization for feature flags
- ADR-0038 through ADR-0046: File backend format, linearization, locking, and merge mode
- ADR-017: Reserved characters for StreamId and StreamPrefix
- ADR-0047: Glob pattern matching for subscriptions (EventFilter pushdown)
- ADR-0049: Streaming reads for `read_stream` (per-backend streaming strategies)
