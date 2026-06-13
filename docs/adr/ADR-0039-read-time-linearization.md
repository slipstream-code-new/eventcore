# ADR-0039: Read-Time Linearization and StreamVersion as Projection

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend persists each `append_events` call as one immutable
JSONL transaction file under `events/` (see ADR-0038). Because every transaction
is a uniquely-named file that is never edited, a `git merge` of two clones'
`events/` directories is a pure additive union with zero textual conflicts. This
file-immutability property is the foundation the entire merge-mode feature rests
on — but it also creates a problem that no other EventCore backend has to solve.

**Two replicas can both legitimately claim the same stream version.**

Consider two developers who each clone the repository, go offline, and append to
`account-1`. The shared backend types are deliberately thin: a `StreamWriteEntry`
carries `{ stream_id, event, event_type, event_data }`, the backend mints
`event_id` (`Uuid::now_v7()`) and `created_at` at write time, `StreamPosition` is
a `nutype(Uuid)` over UUID7, and `StreamVersion` is a `nutype(usize)` starting at 0. When each developer writes locally, their backend assigns the next contiguous
version. After both append to a stream that was at version 5, **both** transaction
files record `stream_version: 6` for their respective events. After a `git merge`
unions the two files, the directory now contains two events that each claim to be
"version 6 of account-1," and the per-event `stream_version` field stored in the
files can no longer be trusted as authoritative.

**The same break afflicts global order.** EventCore relies on UUID7 event IDs for
a deterministic global total order (ADR-005). Within one writer, `Uuid::now_v7()`
is monotonic and the order is sound. Across two replicas, each mints its own
UUID7s against its own clock; clock skew between machines means UUID7 ordering is
no longer a reliable proxy for causal or canonical order once files from two
replicas are combined.

**The forces in tension:**

1. **The contract suite must pass unchanged.** Layer 1 (single-writer mode) must
   satisfy the existing 19-test contract suite (ADR-013): contiguous per-stream
   `StreamVersion` 0,1,2…, `VersionConflict` when `expected != actual`, and a
   stable global total order. The contract suite is the definition of a
   conforming backend; `eventcore-fs` cannot earn a special exemption.

2. **Immutability is non-negotiable.** Events are immutable facts (ADR-005). A
   merge must never renumber, rewrite, or otherwise edit a stored transaction
   file to "fix" a version collision. Doing so would destroy the additive-union
   property that makes git merges conflict-free, and would violate the
   append-only guarantee every projection and audit trail depends on.

3. **Reconciliation must be deterministic.** Every clone that holds the same set
   of transaction files must independently compute the _identical_ canonical
   history — same global order, same per-stream versions — without coordination.
   This is the convergence property: two collaborators who merge each other's
   branches must arrive at byte-identical linearized state.

4. **One code path, not two.** A single-writer code path that diverges from a
   merge-aware code path would mean Layer 1 and Layer 2 are tested against
   different logic, and the "reserve merge fields now" decision in ADR-0038 would
   produce dead, untested fields until Layer 2 lands — violating
   `no-dead-code-workarounds`.

These forces appear to conflict: the contract suite demands authoritative
contiguous versions, immutability forbids renumbering files, and merge mode makes
the stored versions ambiguous. The resolution is to stop treating the stored
version as authoritative at all.

## Decision

**`StreamVersion` is a read-time projection over a deterministically linearized
transaction DAG — never an authoritative stored value.** The store computes each
event's canonical `StreamVersion` and its position in the global order at read
time, from immutable recorded inputs, via a single linearization engine used
identically in both single-writer and merge modes.

### The linearization engine

On `open()`, and as transactions are appended, the store builds an in-memory
index (a CQRS read model; see below) by scanning `events/` and reading the
recorded header of each transaction. The header carries the fields reserved by
ADR-0038: `transaction_id`, `replica_id`, `parent_transaction_ids`, `created_at`,
and `stream_bases`. From the `parent_transaction_ids` pointers the engine builds
a **directed acyclic graph of transactions**. It then:

1. **Topologically sorts the DAG**, honoring every parent → child edge so that a
   transaction never appears before any of its ancestors. Causal order is always
   respected.

2. **Breaks ties between concurrent transactions deterministically.** Two
   transactions are _concurrent_ when neither is an ancestor of the other in the
   DAG. Where the topological sort leaves such transactions unordered relative to
   each other, the tiebreak is the lexicographic tuple
   **`(created_at, replica_id, transaction_id)`**. Every component of this tuple
   is an immutable value recorded in the transaction header at write time. Because
   the inputs are fixed and the comparison is total, **every replica holding the
   same file set computes the identical linear order** — this is the convergence
   guarantee.

3. **Assigns canonical versions by walking the linear order per stream.** Once the
   transactions are in canonical linear order, the engine walks them in that order
   and, for each stream, assigns contiguous `StreamVersion` values 0, 1, 2, …
   to that stream's events as they are encountered. The per-event `stream_version`
   field stored in the file is **advisory only** — the writer's locally-assigned
   guess — and the read path ignores it in favor of the computed value. The global
   order is likewise the canonical linear order, not raw UUID7 sort order.

`read_stream` returns a stream's events in computed-version order; `read_events`
returns the global linear order. Both are projections over the linearized DAG.

### Why one code path serves both modes

**In single-writer mode the DAG is a linear chain.** Each `append_events` records
the writer's current head as its single parent, so the DAG has no forks: every
transaction has exactly one parent and exactly one child. A topological sort of a
chain is the chain itself; the tiebreak is never consulted because no two
transactions are concurrent. Walking that chain assigns exactly the contiguous
0, 1, 2… versions per stream that a naive append-order counter would produce, and
the canonical global order equals append order, which (within one monotonic-clock
writer) equals UUID7 order.

The consequence is decisive: **the same linearization code that serves merge mode
produces, in the degenerate single-writer case, precisely the output the contract
suite (ADR-013) expects** — contiguous versions, append-order global ordering,
`VersionConflict` semantics preserved. There is no separate "simple" code path.
Layer 1's linearizer genuinely _reads_ `parent_transaction_ids`, `replica_id`, and
`stream_bases` from the first commit onward; it simply never encounters a fork
until Layer 2's writers create one. This is what makes ADR-0038's "reserve merge
fields now" honest rather than a dead-code workaround.

### Why this honors immutability

Because the canonical version is _computed_, a merge never has to renumber stored
files. The two colliding "version 6" events keep their files exactly as written;
the linearization engine simply assigns them distinct canonical versions (say 6
and 7) based on the deterministic order, and re-derives every downstream stream's
versions accordingly. Stored files are read, never rewritten. Immutability
(ADR-005) is preserved precisely _because_ `StreamVersion` is a projection rather
than a stored authority.

### Interaction with optimistic concurrency (ADR-007)

Two distinct phenomena must not be conflated:

- **Live write-time conflicts within one replica** continue to be detected and
  rejected exactly as the contract requires. When `append_events` runs, the store
  validates the writer's `expected_versions()` against the current head versions
  computed for the relevant streams; on mismatch it returns
  `VersionConflict { stream_id, expected, actual }` _before writing anything_.
  This is unchanged optimistic-concurrency behavior (ADR-007) and is what the
  contract suite's concurrency tests assert. Linearization does not relax it — the
  head versions it compares against are themselves the computed projection.

- **Merge-time divergence is a different category entirely.** When two transaction
  files arrive via `git merge`, no live write is occurring against either; the
  divergence is discovered at read time by inspecting the DAG and `stream_bases`.
  This is _not_ a `VersionConflict` to be returned from `append_events` — there is
  no append in flight. It is a _fork_, a structural property of the combined file
  set, detected and resolved by the merge-mode machinery (ADR-0041 for causality
  and fork detection, ADR-0042 for domain-owned reconciliation). Linearization
  always produces a single consistent canonical history even while a fork is
  unresolved; reconciliation is about producing _correct domain content_, not
  about producing a consistent order.

Keeping these two mechanisms separate is what lets the single code path satisfy
the optimistic-concurrency contract while also tolerating merge divergence.

### The derived index is gitignored and rebuildable

The linearization result lives in an in-memory index behind `Arc<RwLock<…>>` —
the DAG of transactions, per-stream event lists, and the global linear order with
each event's computed `(StreamPosition, StreamVersion)`. In Layer 1 this index is
**rebuilt from scratch on every `open()`** (simple and always correct). It is a
read model in the CQRS sense and shares no code with the write path.

Crucially, the index is **fully reconstructable from `events/` alone** — this is
the load-bearing invariant for git interoperability. The `index/` directory is
reserved (and gitignored) for a future validated on-disk cache, but is not relied
upon for correctness today; if it is missing, stale, or corrupt, the store simply
rebuilds from `events/`. Only `events/` is committed to git; the linearization,
the cache, and all identity/lock/checkpoint state are gitignored, so a fresh clone
recomputes identical canonical state from the committed events alone.

## Consequences

### Positive

- **One linearization code path serves both modes.** Layer 1 passes the existing
  19-test contract suite (ADR-013) unchanged, and the same engine handles forks in
  Layer 2. No divergent logic to test twice; the merge fields reserved in ADR-0038
  are exercised from day one, satisfying `no-dead-code-workarounds`.
- **Deterministic convergence.** Because the tiebreak tuple
  `(created_at, replica_id, transaction_id)` is composed entirely of immutable
  recorded values, every replica with the same file set computes byte-identical
  canonical order and versions, with zero coordination. This is the property that
  makes offline-then-merge collaboration sound.
- **Immutability preserved (ADR-005).** Version collisions after a merge are
  resolved by _re-deriving_ canonical versions, never by rewriting stored files.
  The append-only guarantee and the conflict-free additive-union git property both
  survive.
- **Optimistic concurrency intact (ADR-007).** Live write-time conflicts within a
  replica still produce `VersionConflict`; the projection model does not weaken
  the concurrency contract.
- **Robust to derived-state loss.** Losing the index (or a future cache) is never
  a correctness problem — the canonical history is always recomputable from
  `events/`, the only committed artifact.

### Negative

- **Stored `stream_version` is advisory and could mislead.** A casual reader
  inspecting a raw `.jsonl` file might assume the stored per-event version is
  authoritative. This must be clearly documented; the field exists for
  forward-compatibility and writer-side bookkeeping, not as a source of truth.
- **Read-time computation cost.** Building the DAG and topologically sorting it on
  `open()` is O(transactions); for very large histories this is more expensive
  than reading a precomputed sequence number from a SQL column. The reserved
  `index/` cache is the planned mitigation, but Layer 1 pays the full rebuild cost
  on every open.
- **Canonical version can differ from the writer's locally-observed version after
  a merge.** An event a writer thought was "version 6" may be canonically version
  7 once a concurrent transaction sorts ahead of it. Applications and projections
  must treat the _computed_ canonical version as authoritative, not any version
  they may have observed pre-merge — a constraint that surfaces in
  projection-after-merge behavior (ADR-0043).
- **Tiebreak determinism depends on header integrity.** If a transaction header's
  `created_at` or `replica_id` were tampered with, the computed order could diverge
  between replicas. This is mitigated by read-time hash validation of transaction
  files (the git-integrity fsck), but it places the convergence guarantee on the
  integrity of the immutable headers.

## Alternatives Considered

### Alternative 1: Trust the stored per-event `stream_version`

Treat the `stream_version` written into each event line as authoritative and serve
reads directly from it, as a SQL backend would serve from a version column.

**Rejected because:** it breaks under merge. After a `git merge` unions two
offline branches, two distinct events can carry the same stored `stream_version`
for the same stream, and a third stream may have gaps where a concurrent writer's
versions interleave. There is no way to reconcile colliding stored versions
without renumbering files (which violates immutability) or picking one writer's
numbering arbitrarily (which is non-deterministic across replicas and silently
discards the other branch's ordering). The stored version is fine as a
single-writer optimization but cannot survive the central use case the feature
exists to serve.

### Alternative 2: Renumber event files in place during merge

On detecting a version collision after a merge, rewrite the affected transaction
files to assign fresh, non-colliding contiguous versions.

**Rejected because:** it violates event immutability (ADR-005). Editing stored
transaction files destroys the append-only guarantee, breaks any content-hash
integrity check, and — most damagingly — destroys the additive-union property that
makes `git merge` of `events/` conflict-free. Two replicas renumbering
independently would also produce different on-disk bytes for the "same" event,
defeating convergence. Immutable files plus computed versions achieve the same
end (non-colliding canonical versions) without any of these costs.

### Alternative 3: Order by wall-clock timestamp alone

Use each event's `created_at` (or its UUID7 timestamp component) as the sole
canonical ordering key across all replicas.

**Rejected because:** it is non-deterministic under clock skew. Two machines'
clocks are not synchronized; a transaction written "later" in causal terms on a
lagging clock can carry an _earlier_ timestamp than a transaction it causally
follows, producing an order that violates the DAG and that two replicas would not
even agree on if their clocks disagree about which event came first. Wall-clock
time is admissible only as _one component_ of the tiebreak between genuinely
concurrent transactions (where no causal edge constrains them), and even then it
must be combined with `replica_id` and `transaction_id` to remain total and
deterministic. Causal (DAG) order must always take precedence over timestamp.

## Related Decisions

- ADR-005: Event Metadata Structure — UUID7 event identity and the immutability
  guarantee that read-time linearization preserves by never renumbering files.
- ADR-007: Optimistic Concurrency Control Strategy — live write-time
  `VersionConflict` detection, kept distinct from merge-time fork divergence.
- ADR-013: EventStore Contract Testing Approach — the 19-test suite that the
  single-writer degeneration of this linearization path passes unchanged.
- ADR-0038: File-Based Event Store Format and Atomicity — the immutable
  one-file-per-transaction format and the reserved header fields
  (`parent_transaction_ids`, `replica_id`, `stream_bases`) that this engine reads.
- ADR-0041: Merge Causality via Transaction DAG — the causality model and fork
  detection that the linearization engine's DAG underpins.
- ADR-0043: Projection Behavior After Structural Merge — how projections cope when
  canonical versions and global order are re-derived after a merge.
