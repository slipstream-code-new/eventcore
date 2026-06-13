# ADR-0041: Merge Causality via Transaction DAG

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend stores each `append_events` call as a single
immutable, uniquely-named transaction file under `events/`. Because the
files are never edited and their names are minted from `Uuid::now_v7()`,
a `git merge` of two clones' `events/` directories is a pure additive
union with zero textual conflicts. After such a union, however, the
combined history can contain transactions that were written concurrently
on different clones (or branches) while offline. Two divergent
transactions can both claim to have written `account-1` "version 6"
against the same starting point. Layer 2's merge mode must reconcile
these into a single valid history.

Reconciliation must satisfy two hard constraints established for the
feature:

1. **Deterministic.** Every clone, given the same set of transaction
   files, must compute the _same_ answer for "which transactions are
   concurrent" and "what is the canonical order." Convergence of the
   reconciled history depends on this.
2. **Non-mutating.** The mechanism must never rewrite or relabel an
   immutable file. All recorded fields — including the writer's
   locally-assigned `stream_version` — are taken as historical fact;
   canonical ordering and versions are _computed_ at read time
   (ADR-0039), not stamped into the files.

Before reconciliation policy (ADR-0042) can run, the store needs a
precise, deterministic answer to one question: **which transactions are
concurrent, and on which streams?** This is a causality question. The
two values the contiguous-integer model and the UUID7 total order rely
on both break across replicas:

- **Contiguous per-stream `StreamVersion`** (0, 1, 2…) is only
  meaningful within a single linear writer. Two offline writers each
  produce a "version 6" for the same stream — the integer no longer
  identifies a unique position.
- **UUID7 global order** assumes a single source of monotonic time.
  Across clones with independent ID generation and clock skew, UUID7
  ordering is not a reliable causal order; an earlier-numbered event on
  one replica may be causally later than a higher-numbered event on
  another.

EventCore already records, in every transaction header (ADR-0038), the
fields needed to reconstruct causality independently of integer versions
and wall-clock IDs: `parent_transaction_ids`, `replica_id`,
`created_at`, and the per-stream `stream_bases` map. This ADR decides
how those fields are combined into a causality model and how forks are
defined and detected. None of this machinery is exercised in
single-writer mode — there, every transaction's parent is the previous
head, so the DAG is a chain and no fork ever exists.

## Decision

`eventcore-fs` detects concurrency by building a **git-like transaction
DAG** from the recorded `parent_transaction_ids`, combined with the
per-stream `stream_bases` recorded in each header. Causality is a
structural property of the DAG, not a function of integer versions or
UUID7 timestamps.

### The transaction DAG

Each transaction is **one node** in the DAG. Its outgoing edges point to
the transactions named in its `parent_transaction_ids` — the writer's
head transaction(s) at the moment of the write. In single-writer mode
there is exactly one parent (the previous head), so the DAG is a chain.
An N-parent node is a merge transaction (recorded by ADR-0042's
reconciliation). Because a transaction is atomic and multi-stream, a
single DAG node may touch several streams at once — which is what allows
a single fork to span several streams simultaneously.

**Ancestor reachability** is the partial order induced by the DAG: `a`
is an ancestor of `b` iff `b` can reach `a` by following parent edges.
Reachability is computed **once over the merged file set and memoized**,
so repeated fork queries do not re-traverse the graph.

### Fork definition

State the condition crisply. For a stream `s`, a **fork** exists iff two
transactions `t1` and `t2`:

1. record the **same `stream_bases[s]`** (the same base version the
   writer built stream `s` on) — and, when present, the **same
   per-stream `base_state_hash[s]`** (see below); and
2. are **causally concurrent**: neither `t1` is an ancestor of `t2` nor
   `t2` an ancestor of `t1` in the DAG.

Both conditions are required. Two transactions that share a
`stream_bases[s]` but where one is an ancestor of the other are _not_ a
fork — that is ordinary linear progress through a recursive merge.
Two concurrent transactions that touch `s` but built on _different_
bases are also not a fork on `s`; they are simply concurrent writes that
do not contend for the same position.

Because a fork is defined per stream but evaluated on whole-transaction
nodes, a single pair of concurrent multi-stream transactions can
constitute a fork on several streams at once. The fork unit presented to
reconciliation (ADR-0042) is the transaction, so a multi-stream fork is
resolved as one unit, never stream-by-stream in a way that could split
an atomic write.

### `base_state_hash` as a complementary anchor

The header MAY carry, alongside `stream_bases`, an optional per-stream
`base_state_hash[s]`: a hash of the state the writer folded up to the
fork point for stream `s`. It is **not the primary fork signal** —
`stream_bases` plus DAG reachability already define forks. It serves two
complementary roles:

1. **Cross-replica "did we fold the same ancestor" check.** When two
   replicas record the same `stream_bases[s]`, matching
   `base_state_hash[s]` values confirm they genuinely diverged from
   byte-identical ancestor state. A mismatch is evidence of corruption,
   schema skew, or a non-deterministic fold and is surfaced rather than
   silently reconciled.
2. **Tamper-evidence anchor.** Combined with the read-time fsck
   (ADR-0046), the hash lets a replica detect an illegal edit to history
   that a `merge=union` could otherwise mask.

`base_state_hash` is optional and additive; absence degrades cleanly to
fork detection by `stream_bases` and DAG reachability alone.

### Single-writer degeneration

In single-writer mode the DAG is a chain: every transaction's
`parent_transaction_ids` is the previous head, and every
`stream_bases[s]` equals the current head version for `s`. No two
transactions are concurrent, so the fork condition is never satisfied —
zero forks, by construction. The same code path that feeds Layer 2's
linearization (ADR-0039) runs in Layer 1; it simply never finds a fork.
This is what makes "reserve the merge header fields in Layer 1"
honest — the linearizer genuinely reads `parent_transaction_ids`,
`replica_id`, and `stream_bases` from the first commit.

## Consequences

### Positive

- **Deterministic across replicas.** Causality is derived entirely from
  immutable recorded fields (`parent_transaction_ids`, `stream_bases`,
  and optionally `base_state_hash`). Every clone with the same file set
  computes the same ancestor relation and therefore the same set of
  forks — the precondition for convergent reconciliation.
- **No reliance on integer versions or wall-clock IDs for causality.**
  Contiguous `StreamVersion` and UUID7 order, both of which break across
  replicas, are not used to determine concurrency. They remain useful
  only where valid: advisory within a writer, and as a tiebreak input to
  linearization (ADR-0039).
- **Multi-stream atomicity preserved.** A transaction is a single DAG
  node, so a fork that spans several streams is presented and resolved
  as one atomic unit, never split.
- **One model, both modes.** The same DAG and reachability machinery
  that powers merge mode runs unchanged in single-writer mode, where it
  degenerates to a chain with zero forks. No separate "merge path" can
  drift from the normal path.
- **Memoized reachability.** Ancestor relations are computed once per
  merged file set, so fork detection over many streams does not pay
  repeated graph-traversal cost.
- **Optional tamper-evidence and ancestor agreement.** When present,
  `base_state_hash` catches divergence from a non-identical ancestor and
  illegal edits to history, strengthening the guarantees of ADR-0046's
  fsck without being required for correctness.

### Negative

- **Header carries causal pointers.** Every transaction must record its
  parent(s) and per-stream bases, adding bytes and the discipline that
  writers always stamp the correct head. This cost is paid even in
  single-writer mode, where the fields are inert.
- **Reachability state grows with history.** Memoized ancestor
  information scales with the size of the DAG. For long histories this
  is a memory and rebuild cost, mitigated by the gitignored, rebuildable
  index (ADR-0039) rather than recomputation on every query.
- **Fork detection is necessary but not sufficient.** Identifying a fork
  does not resolve it. Reconciliation policy is a separate, domain-owned
  concern (ADR-0042); this ADR only guarantees the deterministic
  detection that policy consumes.
- **`base_state_hash` depends on a stable fold.** The hash is only a
  meaningful agreement check if both replicas fold the same events to
  identical bytes. Non-deterministic application state or schema skew
  produces a mismatch that must be surfaced rather than reconciled.

## Alternatives Considered

### Alternative 1: Version vectors as the primary causality mechanism

Track a per-replica version vector and use vector comparison to detect
concurrency, as in classic optimistic-replication systems.

**Rejected as primary because** version vectors presume a _bounded,
registered_ set of replicas. The `eventcore-fs` use case is ad-hoc
clones: anyone can `git clone` the repository, append offline, and merge
back, and clones can themselves be copied (the copy-trap that ADR-0044
addresses). There is no registration step at which a replica joins a
known membership, and the replica set is unbounded and unenumerable.
Version vectors would grow without bound, require coordination to assign
slots, and have no clean answer for a clone that appears, writes once,
and never returns. The transaction DAG needs no membership: causality is
read directly off the parent pointers each transaction already records,
and a never-returning clone simply contributes a few leaf nodes.

### Alternative 2: Pure Merkle / hash chains as the primary mechanism

Chain each transaction to its predecessor by content hash, git-commit
style, and use hash-chain structure alone to order and detect divergence.

**Rejected as primary because** a pure hash chain carries no _who_ and
no _when_. It establishes that transaction B followed transaction A in
content, but it cannot disambiguate concurrent transactions for a
deterministic tiebreak (ADR-0039 needs `created_at` and `replica_id`
for that), nor attribute a branch to a replica for collision detection
(ADR-0044). A hash chain is also naturally per-stream or per-line; it
does not cleanly span a multi-stream atomic transaction, which is the
fork unit here. **However**, the tamper-evidence value of content
hashing is real, so this ADR _adopts_ an optional per-stream
`base_state_hash` as a complementary anchor — a cross-replica "did we
fold the same ancestor" check and a tamper-evidence signal — while
keeping the DAG of parent pointers plus `stream_bases` as the primary
fork-detection mechanism.

### Alternative 3: Detect forks from duplicate integer `StreamVersion` alone

Treat two transactions that both assign the same `stream_version` to a
stream as a fork, without consulting the DAG.

**Rejected because** the writer-assigned `stream_version` is advisory
(ADR-0038/0039) and the canonical version is _computed_ at read time.
Two transactions can legitimately carry the same advisory version
without being concurrent (e.g., across a recursive merge), and the
integer cannot distinguish "concurrent on the same base" from "linear
progress that happens to reuse a number." Fork detection must be
grounded in the causal structure (`stream_bases` plus DAG reachability),
not in a non-canonical integer that loses meaning across replicas.

### Alternative 4: Wall-clock / UUID7 ordering to define concurrency

Use the UUID7 `created_at`/`event_id` total order to decide which
transaction "came first" and treat overlaps within a time window as
concurrent.

**Rejected because** UUID7 ordering assumes one monotonic clock source.
Across clones with independent generation and clock skew, it is not a
reliable causal order, and there is no principled window that
distinguishes "concurrent" from "sequential" across machines. UUID7
order is retained only as a _deterministic tiebreak input_ in ADR-0039
(via `created_at`), where it is applied to transactions the DAG has
already proven to be concurrent — never to _establish_ concurrency.

## Related Decisions

- ADR-0038: File-Based Event Store Format and Atomicity — defines the
  header fields (`parent_transaction_ids`, `replica_id`, `created_at`,
  `stream_bases`, and optional `base_state_hash`) this ADR consumes.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection —
  the DAG built here feeds deterministic linearization; concurrent
  transactions identified here are tiebroken there by
  `(created_at, replica_id, transaction_id)`.
- ADR-0042: Domain-Owned Reconciliation API — consumes the forks
  detected here; resolves each fork (including multi-stream forks) as a
  single domain-owned unit.
- ADR-0044: Replica Identity for Merge Mode — `replica_id` disambiguates
  concurrent transactions and backs the reconcile-time collision check
  referenced by this ADR's tiebreak and agreement guarantees.
- ADR-001: Multi-Stream Atomicity — the atomic transaction is the DAG
  node and therefore the fork unit, preserving multi-stream atomicity
  across reconciliation.
- ADR-007: Optimistic Concurrency Control — within a single writer,
  `stream_bases` is the recorded `expected_version`; across replicas it
  becomes the fork-detection anchor.
