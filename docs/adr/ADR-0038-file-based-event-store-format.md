# ADR-0038: File-Based Event Store Format and Atomicity

## Status

Accepted

## Date

2026-06-12

## Context

EventCore is gaining a new backend, the `eventcore-fs` crate, that persists
events to plain files on disk so that event-sourced developer tools can keep
their entire history inside a git repository. The motivating requirement is
unusual for an event store: two people (or one person across two clones or
branches) must be able to append events offline and later reconcile their
divergent histories with an ordinary `git merge`. That reconciliation is a
business-domain concern, so the application owns the policy while the library
owns the mechanism.

The work is split into two layers with very different risk profiles:

- **Layer 1 — single-writer file backend.** Implements the existing
  `EventStore` / `EventReader` / `CheckpointStore` / `ProjectorCoordinator`
  traits and passes the existing contract suite unchanged: contiguous
  per-stream `StreamVersion` (0, 1, 2, …), optimistic concurrency
  (`VersionConflict` when expected ≠ actual), and a global total order.
- **Layer 2 — merge mode.** Genuinely novel. After a git union of immutable
  transaction files, two divergent transactions can both claim the same stream
  at the same version. The contiguous-integer version model and UUID7 total
  order both break across replicas (clock skew, independent ID generation).
  Reconciliation must be deterministic (every clone computes the same result)
  and domain-owned, while never mutating immutable files.

This ADR governs **Layer 1's on-disk format and its write-time atomicity** —
the foundation the entire feature rests on. Two forces dominate the decision:

1. **File immutability is permanent.** Event files, once committed, are facts.
   They are never edited or deleted (ADR-005). A later format change therefore
   cannot rewrite already-committed files — any field Layer 2 needs must be
   present from the very first commit, or those events become unmergeable.

2. **Git is the merge transport.** The reconciliation story only works if a
   `git merge` of the on-disk history is a pure additive union with zero
   textual conflicts. That property dictates the file granularity, the naming
   scheme, and the discipline that only the source-of-truth directory is
   committed while all derived state is gitignored.

A subtle consequence of force (1) is that Layer 1's format must reserve fields
that Layer 1 itself never branches on. The repository's
`no-dead-code-workarounds` and `incremental-event-fields` rules normally forbid
writing fields no current code reads. This ADR records the user-ratified
exception and — critically — the design choice that makes the exception honest
rather than a workaround: a single read-time-linearization code path
(ADR-0039) genuinely reads those reserved fields from the first commit onward.

**Why this decision now:** Because the format is immutable, it must be ratified
in full — including Layer 2's needs — before any code is written. The format
cannot be evolved incrementally the way a SQL schema can.

## Decision

`eventcore-fs` persists each transaction as **one immutable JSONL file**, where
one transaction corresponds to one `append_events` call. The file's contents
are append-only facts that are never edited or deleted after creation.

### File granularity and naming

- **One file per transaction.** Every `append_events` call produces exactly one
  file. That file is the atomic unit: all of a multi-stream write either appears
  in `events/` as a single complete file or does not appear at all (ADR-001).
- **Filename = transaction UUID7.** The file is named `<transaction_id>.jsonl`,
  where `transaction_id` is a `Uuid::now_v7()` minted once per `append_events`.
  UUID7 is sortable (time-ordered prefix), collision-free without coordination,
  and git-merge-friendly: two replicas independently minting transaction IDs
  never collide, so a merge of two `events/` directories is a conflict-free
  union of distinct filenames.

### On-disk schema (format version 1)

A transaction file is JSONL: **line 1 is a header record**, and **lines 2..N are
one event envelope per line**, in entry order.

**Header (line 1):**

```json
{
  "record": "header",
  "format_version": 1,
  "transaction_id": "<uuid7>",
  "replica_id": "<uuid>",
  "parent_transaction_ids": ["<uuid7>", "..."],
  "created_at": "2026-06-12T17:39:00.123456Z",
  "stream_bases": { "account-123": 5 }
}
```

- `record: "header"` — discriminator distinguishing the header line from event
  lines.
- `format_version: 1` — schema version anchor; lets future readers detect and
  refuse formats they cannot interpret.
- `transaction_id` — the file's UUID7, equal to the filename stem; minted once
  per `append_events`.
- `replica_id` — this clone's machine-local identity (sourced from
  `.eventcore/replica_id`). Inert in single-writer mode; read by the
  linearization tiebreak (ADR-0039) and consumed by merge causality
  (ADR-0041) and replica-identity collision detection (ADR-0044).
- `parent_transaction_ids` — the writer's head transaction(s) at write time. In
  single-writer mode this is the single previous head; this forms the
  transaction DAG that the read-time linearizer consumes from the first commit
  (ADR-0041).
- `created_at` — RFC 3339 commit timestamp, minted by the backend at write
  time. Primary deterministic tiebreak key for linearization (ADR-0039).
- `stream_bases` — a map from each written `stream_id` to the
  `expected_version` the writer built on for that stream. In single-writer mode
  this equals the current head, so no fork is ever detected; in merge mode it is
  the per-stream `base_version` used for fork detection (ADR-0041).

**Event line (lines 2..N):**

```json
{
  "record": "event",
  "event_id": "<uuid7>",
  "stream_id": "account-123",
  "stream_version": 6,
  "event_type": "MoneyDeposited",
  "event_data": { "...": "..." },
  "metadata": {}
}
```

- `record: "event"` — discriminator marking this as an event envelope.
- `event_id` — `Uuid::now_v7()` minted per entry, in `into_entries()` order;
  doubles as the event's `StreamPosition`.
- `stream_id` — the target stream for this event.
- `stream_version` — the writer's locally assigned per-stream version
  (contiguous from `stream_bases[stream_id] + 1`). This value is **advisory
  only**. The authoritative version is computed at read time by linearization
  (ADR-0039); the two coincide in single-writer mode but may diverge after a
  merge, and the read path never trusts the recorded value over the computed
  one.
- `event_type` — the static event-type name, mirroring the value backends
  already persist for filtering.
- `event_data` — the serialized event payload (`serde_json::Value`).
- `metadata` — `{}` in Layer 1, mirroring the sqlite backend's current
  behavior; present in the schema for forward compatibility.

### Atomic write protocol

Each `append_events` materializes its file with a tmp-write-fsync-rename
sequence so that `events/` never observes a partial file:

1. Serialize the complete header line plus all event lines into an in-memory
   buffer.
2. Write the buffer to `tmp/<transaction_id>.jsonl.tmp`.
3. `fsync` the tmp file (`File::sync_all`) so its bytes are durable before it is
   linked into the committed directory.
4. `fs::rename` the tmp file to `events/<transaction_id>.jsonl`. Rename is
   atomic on the same filesystem, so the committed file appears whole or not at
   all.
5. `fsync` the `events/` directory so the new directory entry is durable.

This makes one transaction file an **all-or-nothing multi-stream write**,
satisfying ADR-001's atomicity guarantee with no transaction manager beyond the
filesystem's own rename atomicity.

### Crash safety

- A crash before step 4 leaves at most a leftover `tmp/*.tmp` file. The store
  scanner reads only `events/*.jsonl`, so leftover tmp files are ignored on the
  next open and may be cleaned lazily.
- Because rename is atomic, `events/` never holds a half-written file. There is
  no recovery code path for partially committed transactions, because partial
  commits cannot occur.

### Immutability

Files in `events/` are never edited or deleted after creation. All corrections
are expressed as new events in new transactions, consistent with ADR-005's
immutability guarantee. This is what permits a `git merge` to be a pure
additive union and what makes read-time validation (a content hash that must
match the file) meaningful.

### Directory layout: committed vs. derived

Only `events/` is committed to git; everything else is derived or
machine-local and is gitignored:

```
<root>/
  events/                    # ONLY committed source of truth
    <transaction_id>.jsonl   # one file per append_events
  tmp/                       # atomic-write staging (gitignored)
  index/                     # rebuildable derived cache (gitignored)
  checkpoints/               # projection progress (gitignored)
  locks/                     # coordinator advisory locks (gitignored)
  .eventcore/replica_id      # this clone's identity, lazily on write (gitignored)
  .lock                      # store-wide cross-process lock (gitignored)
```

The load-bearing invariant is that **all derived state is fully reconstructable
from `events/` alone**. The index, checkpoints, locks, and replica identity are
local concerns that must never travel through git, because each clone's derived
state is its own.

### Reserving Layer-2 fields now: a conscious, ratified exception

The header's `replica_id`, `parent_transaction_ids`, and `stream_bases` fields
exist solely to serve merge mode. Layer 1 never forks, so a naive reading of the
`no-dead-code-workarounds` and `incremental-event-fields` rules would forbid
writing them before a test demands them. This ADR records the user's explicit
ratification of an exception, justified on two grounds:

1. **Immutability makes deferral impossible.** Event files committed under
   Layer 1 are permanent. If the merge fields were added later, every event
   written before the format change would lack them and could never be merged.
   Unlike a mutable SQL schema, the on-disk format cannot be migrated after the
   fact — the only safe time to reserve these fields is the first commit.

2. **The fields are not dead — they are read from day one.** ADR-0039
   establishes a single read-time-linearization code path used in _both_ modes.
   That code genuinely reads `parent_transaction_ids` (to build the DAG),
   `replica_id` (as a linearization tiebreak), and `stream_bases` (to detect
   forks). In single-writer mode the DAG is a degenerate chain with no forks, so
   the computed order equals append order and the computed versions equal the
   contiguous 0, 1, 2, … the contract suite expects — but the fields are read on
   every load regardless. There is no fake read, no `let _ =`, and no
   write-only field. The exception is therefore narrow and honest: it reserves
   format space whose readers exist from the first commit.

## Consequences

### Positive

- **Format never changes.** Reserving the merge fields up front means the
  Layer 1 → Layer 2 transition adds no new on-disk schema and no migration of
  already-committed history.
- **Conflict-free git merges.** One immutable, UUID7-named file per transaction
  makes any merge of `events/` a pure additive union of distinct filenames with
  zero textual conflicts.
- **Strong crash safety with no recovery code.** tmp-write → fsync → atomic
  rename → dir-fsync guarantees `events/` never holds a partial file; there is
  no half-committed state to recover from.
- **Multi-stream atomicity for free.** One file = one transaction = all-or-
  nothing, satisfying ADR-001 using only filesystem rename atomicity.
- **Auditability.** JSONL is human-readable and line-diffable; a reviewer can
  inspect a transaction file directly in a pull request.
- **Clean derived/committed split.** Gitignoring everything but `events/` keeps
  each clone's index, checkpoints, locks, and identity local, and guarantees the
  committed history is fully self-describing.
- **Honest single read path.** The same linearization engine serves both modes,
  so the reserved fields are exercised continuously rather than lying dormant.

### Negative

- **Many small files.** A repository with a long history accumulates one file
  per transaction. Directory-entry overhead and scan-on-open cost grow with
  history length; a future validated `index/` cache is reserved to amortize this
  but is not built in Layer 1.
- **No write-time compaction.** Immutability forbids rewriting or merging old
  files, so storage grows monotonically. Compaction, if ever needed, must be a
  separate, explicit, history-rewriting operation outside normal writes.
- **Two fsyncs per write.** Syncing both the tmp file and the `events/`
  directory adds durability cost to every append relative to a single buffered
  write. A configurable `FsyncPolicy` mitigates this for tests, not production.
- **Deliberate reservation of currently-unbranched fields.** The header carries
  three fields no Layer 1 writer forks on. This is an accepted, documented
  exception to the dead-code rules, sound only because immutability forces it and
  ADR-0039 reads the fields from the first commit.
- **Advisory recorded versions.** The per-event `stream_version` on disk is not
  authoritative; readers must always recompute it via linearization, which is a
  subtlety implementors must respect rather than trusting the stored value.

## Alternatives Considered

### Alternative 1: One append-only log file for the whole store

Persist all transactions as appended lines in a single `events.log` file.

**Rejected because** every write appends to the same file, so any two replicas
that both append produce a textual conflict at the same trailing lines on
`git merge`. The conflict-free-merge property — the entire reason the file
backend exists — would be lost on the very first concurrent write. A single log
also conflates the atomic unit (a transaction) with the file, making partial-
write recovery harder.

### Alternative 2: One file per stream

Persist each stream's events to `streams/<stream_id>.jsonl`.

**Rejected because** it breaks multi-stream atomicity: a single `append_events`
that writes to several streams would have to touch several files, and there is
no filesystem primitive to rename multiple files atomically — a crash could
leave some streams written and others not, violating ADR-001. It also
reintroduces git conflicts: two replicas appending to the same stream both
modify the same `<stream_id>.jsonl` and conflict on merge.

### Alternative 3: Sequential numeric filenames

Name transaction files `000001.jsonl`, `000002.jsonl`, … using a monotonic
counter.

**Rejected because** the counter is per-clone, so two replicas working offline
both mint `000007.jsonl` for different transactions. On `git merge` those become
either a rename/content conflict or, worse, a silent collision where one
transaction overwrites another. Sequential names require coordination the file
backend explicitly does not have. UUID7 filenames provide the same sortable,
time-ordered property without any coordination and with collision-free
uniqueness across replicas.

### Alternative 4: Defer the merge header fields until Layer 2

Write only the fields Layer 1 reads (no `replica_id`,
`parent_transaction_ids`, or `stream_bases`) and add them when merge mode is
built.

**Rejected because** event files are immutable forever. Any event committed
under the Layer 1 format would permanently lack the merge fields and could never
participate in a reconciliation. There is no migration path for already-written
immutable files. The only safe time to reserve the fields is the first commit,
and ADR-0039's shared read path makes them genuinely read from that commit
onward — so deferral buys nothing and forecloses the entire Layer 2 story.

## Related Decisions

- ADR-001: Multi-Stream Atomicity Implementation Strategy — one transaction file
  is the all-or-nothing multi-stream write unit this ADR realizes on the
  filesystem.
- ADR-005: Event Metadata Structure — establishes UUIDv7 event identity and the
  immutability guarantee that this format enforces by never editing files.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection — consumes
  the reserved header fields (`parent_transaction_ids`, `replica_id`,
  `stream_bases`) and computes the authoritative `StreamVersion`, making the
  advisory on-disk version and the reserved fields non-dead from the first
  commit.
- ADR-0041: Merge Causality via Transaction DAG — uses `parent_transaction_ids`
  and `stream_bases` to model causality and detect forks.
- ADR-0046: Git Integration Contract for the File Store — defines the
  `.gitignore` / `.gitattributes` discipline and read-time validation that this
  format's immutability and committed/derived split make possible.
