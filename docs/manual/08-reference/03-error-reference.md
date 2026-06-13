# Chapter 8.3: Error Reference

This chapter is a comprehensive reference for the error types EventCore
actually returns. The two error types a downstream consumer encounters are
`CommandError` (returned by `execute()`) and `EventStoreError` (returned by
backend operations and wrapped inside `CommandError`). Both derive
`thiserror::Error` and use machine-readable, kebab-case-style messages.

## Command Errors

### `CommandError`

`execute()` returns `Result<ExecutionResponse, CommandError>`. `CommandError`
has exactly four variants:

```rust
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// A business rule was violated. Wraps the command's own typed error
    /// (any `std::error::Error + Send + Sync`). This is what the `require!`
    /// macro produces and what your command's typed error enum converts into.
    #[error(transparent)]
    BusinessRuleViolation(Box<dyn std::error::Error + Send + Sync>),

    /// Optimistic concurrency conflicts persisted after the retry policy was
    /// exhausted. The `u32` is the number of retry attempts that were made.
    #[error("concurrency conflict after {0} retry attempts")]
    ConcurrencyError(u32),

    /// The underlying event store failed (read or append). Wraps the
    /// `EventStoreError` that caused it.
    #[error("event store error: {0}")]
    EventStoreError(EventStoreError),

    /// A validation error surfaced during execution.
    #[error("validation error: {0}")]
    ValidationError(String),
}
```

#### `BusinessRuleViolation`

```text
Error: insufficient funds for account account-123: balance=50, attempted_withdrawal=100
```

**Cause:** Your command's `handle()` rejected the operation. This is the normal
way to signal that a domain precondition was not met — produced either by the
`require!` macro or by returning your own typed error (which converts into
`CommandError::BusinessRuleViolation` via its `From` impl).

**Resolution:**

- This is expected control flow, not a bug. Match on the variant and surface a
  meaningful message to the caller.
- To recover the original typed error, downcast the boxed error or match on its
  `Display` output.
- Define a typed error enum per command (see
  `.claude/rules/thiserror-for-errors.md` convention) rather than ad-hoc
  strings.

```rust
match execute(&store, command, RetryPolicy::new()).await {
    Ok(response) => { /* ... */ }
    Err(CommandError::BusinessRuleViolation(err)) => {
        eprintln!("rejected: {err}");
    }
    Err(other) => return Err(other),
}
```

#### `ConcurrencyError`

```text
Error: concurrency conflict after 3 retry attempts
```

**Cause:** Another writer modified one of the command's streams between the read
and the append, and the conflict was still present after every retry the
`RetryPolicy` allowed. EventCore retries optimistic-concurrency conflicts
automatically; this variant means retries were exhausted.

**Resolution:**

- Increase `RetryPolicy::max_retries` or widen the backoff for hot streams.
- Reduce contention by narrowing the streams a command declares, so unrelated
  writes do not conflict.
- If the conflict is legitimate (a genuine race the user should resolve), surface
  it to the caller for a retry-or-cancel decision.

#### `EventStoreError`

```text
Error: event store error: version conflict on stream account-123: expected version 4, found 5
```

**Cause:** A backend operation failed. The wrapped `EventStoreError` (see
below) carries the specific failure.

**Resolution:** Inspect the inner `EventStoreError` variant and follow its
guidance.

#### `ValidationError`

```text
Error: validation error: <detail>
```

**Cause:** A validation failure surfaced during execution that is not a
domain business-rule violation.

**Resolution:** Validate inputs at the boundary (parse-don't-validate) so domain
types are always valid by construction; this variant should be rare in
well-typed code.

## Event Store Errors

### `EventStoreError`

Returned by `EventStore::read_stream`, `EventStore::append_events`, and the
`StreamWrites` builder. It is also wrapped by `CommandError::EventStoreError`.

```rust
#[derive(Debug, Clone, thiserror::Error)]
pub enum EventStoreError {
    /// The same stream was registered twice with different expected versions
    /// when building a `StreamWrites` value.
    ConflictingExpectedVersions { /* stream_id, ... */ },

    /// A write targeted a stream that was never registered with an expected
    /// version. Register every target stream before appending.
    #[error("stream {stream_id} must be registered before appending events")]
    UndeclaredStream { stream_id: StreamId },

    /// Serializing an event for storage failed.
    #[error("failed to serialize event for stream {stream_id}: {detail}")]
    SerializationFailed { stream_id: StreamId, detail: String },

    /// Decoding a stored event back into your domain type failed.
    #[error("failed to deserialize event for stream {stream_id}: {detail}")]
    DeserializationFailed { stream_id: StreamId, detail: String },

    /// A backend operation (connection, query, transaction) failed.
    #[error("{operation} operation failed")]
    StoreFailure { operation: Operation },

    /// Optimistic concurrency: the stream's actual version did not match the
    /// expected version supplied with the write.
    #[error("version conflict on stream {stream_id}: expected version {expected}, found {actual}")]
    VersionConflict { /* stream_id, expected, actual */ },
}
```

#### `VersionConflict`

**Cause:** The expected version supplied with a write no longer matches the
stream's current version — a concurrent writer got there first. Inside
`execute()`, this drives the automatic retry loop; you only observe it directly
when calling `append_events` yourself.

**Resolution:** Reload state, re-validate, and retry. When using `execute()`,
this is handled for you — a persistent conflict surfaces as
`CommandError::ConcurrencyError`.

#### `UndeclaredStream`

**Cause:** A command (or manual `StreamWrites` builder) tried to append to a
stream it never declared. Every stream a command writes must be declared via
`#[stream]` fields (`#[derive(Command)]` generates the declarations) or
registered with `StreamWrites::register_stream`.

**Resolution:**

- Add the stream to the command's `#[stream]`-tagged fields.
- For dynamic streams discovered during `handle()`, declare them through the
  `StreamResolver` so EventCore loads and locks them before the append.

#### `DeserializationFailed`

**Cause:** A stored event could not be decoded into the requested domain type.
Usually a schema-evolution problem: an event was written with one shape and is
being read with an incompatible one.

**Resolution:**

- Add `#[serde(default)]` for newly added optional fields so old events still
  decode (see [Chapter 5.1: Schema Evolution](../05-advanced-topics/01-schema-evolution.md)).
- For non-backwards-compatible changes, use event upcasting (see
  [Chapter 5.2: Event Versioning](../05-advanced-topics/02-event-versioning.md)).

#### `SerializationFailed`

**Cause:** An event could not be serialized for storage. Almost always a bug in
a custom `Serialize` impl or a non-serializable field.

**Resolution:** Ensure every event type derives `Serialize`/`Deserialize` and
contains only serializable fields.

#### `StoreFailure`

**Cause:** The backend (PostgreSQL, SQLite, file store) failed to complete an
operation — connectivity, transaction, or I/O. The `Operation` enum records
which operation failed.

**Resolution:**

- Verify backend connectivity and configuration (connection string, file
  permissions, disk space).
- Retry transient failures at the application layer.
- See [Chapter 7.4: Troubleshooting](../07-operations/04-troubleshooting.md) for
  backend-specific diagnostics.

#### `ConflictingExpectedVersions`

**Cause:** While building a `StreamWrites` value, the same stream was registered
twice with different expected versions.

**Resolution:** Register each stream once with a single expected version.

## Projection Errors

`run_projection()` returns `Result<(), ProjectionError>`. A projector's own
`on_error()` callback decides whether a per-event failure is retried or fatal
(via `FailureStrategy`); see
[Chapter 2.4: Projections](../02-getting-started/04-projections.md) and the
`Projector` trait documentation.

## See Also

- [Chapter 3.5: Error Handling](../03-core-concepts/05-error-handling.md) —
  patterns for handling errors in command logic.
- [Chapter 7.4: Troubleshooting](../07-operations/04-troubleshooting.md) —
  operational diagnosis of production issues.
- API docs: run `cargo doc -p eventcore --open` for the rendered rustdoc of
  `CommandError`, `EventStoreError`, and the rest of the public surface.
