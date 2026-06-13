# ADR-0043: Projection Behavior After Structural Merge

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend (see ADR-0038, ADR-0039) supports a merge mode in
which two replicas append events offline and a later `git merge` produces a
single combined history. After such a merge, the store performs a _structural
merge_: it rebuilds the transaction DAG from recorded parent pointers and
re-derives the canonical linear order of every event (ADR-0039). This ADR
governs what happens to **projections** (read models) when that canonical
order changes.

**The core problem: a cursor a projection has already passed can be undercut.**

`EventReader` returns events ordered by `StreamPosition` — a nutype-wrapped
UUIDv7 minted per event at write time (ADR-005) — and paginates with an
exclusive `after_position` cursor. Projections persist that cursor as a
checkpoint and resume from it (ADR-021). In single-writer mode this is sound:
UUIDv7 is monotonic with append order, so "everything after my cursor" is
always "everything I have not yet seen."

A structural merge breaks that assumption. Two replicas mint UUIDv7 event ids
from independent clocks. When their histories are unioned, the **canonical
linearized position** of a freshly-ingested event can fall _earlier_ than a
cursor a projection has already advanced past. Concretely:

1. Replica A appends events, minting positions that the local projection
   consumes; its checkpoint advances to position `P_a`.
2. Replica B, offline, appended a divergent event with a UUIDv7 position `P_b`
   where `P_b < P_a` (B's clock was behind, or the events simply interleave
   that way once linearized).
3. The branches merge. The store linearizes the union and assigns canonical
   `StreamVersion`s. B's event now sits _behind_ the projection's checkpoint.
4. A naive cursor-based projection asks for "events after `P_a`" and **never
   sees B's event** — because its canonical position is less than the cursor.

This is not an implementation bug to be patched; it is a genuine consequence of
deriving order from independently-generated, uncoordinated identifiers. Left
unaddressed it is **read-model corruption**: the projection's state silently
diverges from the events of record. This is the "business-domain concern" the
design anticipated — the point where merge mode stops being purely the
library's mechanism and starts imposing an obligation on the application
author.

**Why we cannot simply make positions globally monotonic.**

The obvious fix — assign every event a single, dense, globally-monotonic
position so a cursor can never be undercut — requires a coordinator: a single
authority that hands out positions in agreed order. The file-store model
_forbids_ a coordinator by construction. Replicas append offline, on different
machines, with no network between them; the only synchronization point is a
later `git merge`. A coordinator-free, dense, globally-monotonic order across
all replicas is impossible. Any scheme that pretends otherwise either
reintroduces a coordinator or silently loses events at merge time.

**Forces at play:**

1. **No coordinator.** Positions are minted independently per replica
   (ADR-0039). Cross-replica ordering only becomes knowable after a merge.
2. **Canonical order is derived, not assigned.** `StreamVersion` is a
   projection of the DAG (ADR-0039), recomputed at read time. A merge can move
   an event earlier in canonical order without violating any immutable record.
3. **Live projections must not rewind.** A running projection holds in-memory
   and persisted checkpoint state. Silently rewinding its cursor — or forcing
   it to reprocess already-seen events out of nowhere — violates the
   poll-based resumption contract (ADR-021) and breaks projections that are not
   built to be replayed.
4. **Reconciliation is domain-owned.** The application decides how a fork
   resolves (ADR-0042); its resolution command produces the events that express
   the merged outcome. The library owns the mechanism; the app owns the policy.
5. **Some projections cannot be made idempotent.** A projection that does
   non-idempotent work (incrementing a counter, emitting a side effect) cannot
   safely absorb a compensation that arrives after the fact. Such projections
   need an escape hatch, not a silent correction.

We must define a default that is correct without app cooperation where
possible, and that makes the residual app-author obligation **explicit and
documented** where cooperation is unavoidable.

## Decision

`eventcore-fs` adopts a three-part default for projection behavior after a
structural merge. The three parts are complementary: (a) makes the common
caught-up projection self-correct, (c) guarantees a live projection never
rewinds, and (b) gives projections that cannot trust compensation a way out.

### (a) Reconciliation outcomes are expressed as new HEAD events (compensations)

A reconciliation never edits, reorders, or back-dates existing events. Instead,
the domain's resolution command (ADR-0042) produces **new events**, and the
library records them in a **merge transaction whose header descends from all
fork heads** (`parent_transaction_ids` lists every fork-head transaction —
ADR-0041). Because the merge transaction is a descendant of every branch, its
events sort to the _latest_ canonical positions in the linearized order.

Consequence for projections: a caught-up, cursor-based projection moves
**forward monotonically** to read the compensation events and is thereby
corrected. It does not need to know a merge happened. The corrective events
arrive ahead of its cursor, in the normal forward direction, exactly like any
other newly-appended events. This is the path that makes the default work
"for free" for the majority of projections — those that can express the merged
truth as a forward correction.

### (c) A per-replica LOCAL-INGESTION-ORDER cursor governs projection progress

The store maintains **two distinct orderings**, and assigns each to the job it
is actually correct for:

- **Canonical linearized order** (the DAG topological sort with the
  deterministic tiebreak from ADR-0039) governs **state-folding and
  `StreamVersion`**. This is the order in which events are applied to
  reconstruct command state and read-model state, and it converges across all
  replicas with the same file set.
- **Local-ingestion order** governs **cursor progress**. Each replica records
  the order in which _it_ first became aware of each transaction (the order in
  which files appeared in its working copy, including those that arrived via a
  merge). A projection's checkpoint is a position in this local-ingestion
  order, **not** in canonical order.

The key property: when a merge ingests transactions whose canonical positions
are _earlier_ than the projection's checkpoint, those transactions receive a
**fresh, larger local-ingestion position** — because they are new to _this_
replica, regardless of where they linearize. The projection's
local-ingestion cursor therefore advances forward to cover them, and the
projection **never rewinds**. State-folding still applies them in canonical
order (so `StreamVersion` and folded state remain convergent), but the cursor
that decides "have I seen this yet" is monotonic in ingestion, not in
canonical position. This resolves the corruption from the Context section:
the merge-introduced event is _behind_ in canonical order but _ahead_ in
local-ingestion order, so the cursor reaches it.

Because local-ingestion order is per-replica, it is machine-local and
gitignored (it is part of the rebuildable index, ADR-0039); it is never
committed and never expected to agree across replicas. Only canonical order —
derived from immutable recorded fields — is shared.

### (b) A topology-generation counter enables rebuild-from-zero

The store exposes a **topology-generation counter**: a monotonically increasing
value that the library bumps whenever a structural merge changes the DAG (i.e.,
whenever a merge introduces a fork resolution or otherwise alters the linearized
order). A projection can read this counter alongside its checkpoint.

Projections that **cannot guarantee compensation-idempotence** — those for which
part (a)'s forward-correction model is unsafe because absorbing a compensation
after the diverged events were already projected would produce a wrong result —
may compare the stored generation against the current one and, on a change,
**rebuild from zero**: discard the read model and re-fold the entire canonical
history. This is the safety net. It is more expensive than incremental
correction, but it is unconditionally correct for any projection, idempotent or
not, because it never relies on a compensation absorbing prior state.

### The imposed, documented app-author constraint

Parts (a) and (c) make the default correct for projections whose corrections can
be expressed as forward compensations. That correctness rests on an obligation
the library **cannot enforce** and therefore **imposes and documents
explicitly**:

> A reconciliation command's events MUST compensate to a correct read-model
> state **whether or not** the diverged events were already projected.

In other words, the resolution events authored by the domain (ADR-0042) must be
written so that applying them on top of _either_ "the read model has already
seen both branches' diverged events" _or_ "the read model has not yet seen
them" yields the same, correct final state. A projection author who cannot
satisfy this constraint for a given read model must instead opt that read model
out of forward-correction and use the topology-generation counter (part b) to
rebuild from zero. This is the explicit boundary between what the library
guarantees (mechanism: monotonic cursors, convergent canonical state, a
generation signal) and what the application owns (policy: compensations that are
correct under reordering).

## Consequences

### Positive

- **No silent read-model corruption.** The local-ingestion cursor (c)
  guarantees a projection reaches every merge-introduced event even when its
  canonical position is behind the checkpoint. The failure mode described in the
  Context — silently skipping a merged event — cannot occur under the default.
- **Live projections never rewind.** Cursor progress is monotonic in
  local-ingestion order, preserving the poll-based resumption contract
  (ADR-021). A running projection is never forced backward or asked to
  reprocess events it has already consumed.
- **The common case self-corrects for free.** Because reconciliation outcomes
  are new head events (a), a caught-up cursor-based projection moves forward and
  is corrected with no merge-awareness and no app-specific wiring.
- **Convergent state is preserved.** Canonical linearized order still governs
  state-folding and `StreamVersion` (ADR-0039), so every replica with the same
  file set reconstructs byte-identical read-model state once caught up — even
  though their local-ingestion cursors differ.
- **An unconditional escape hatch exists.** The topology-generation counter (b)
  gives non-idempotent or compensation-averse projections a correct,
  always-available recovery path: rebuild from zero.
- **The residual obligation is explicit.** The app-author constraint is stated
  as a documented contract, not buried as an implicit assumption — matching
  ADR-013's philosophy that semantic contracts the type system cannot enforce
  are made explicit and verified by documentation and tests.

### Negative

- **Two orderings to reason about.** Maintaining a separate local-ingestion
  order alongside canonical order adds conceptual and implementation surface.
  Authors and maintainers must keep clear which order governs which job
  (state-folding vs. cursor progress).
- **The app-author constraint is not compile-time enforced.** Like the version
  checking contract in ADR-013, "compensations must be correct under reordering"
  is a runtime/behavioral obligation the library cannot prove. A projection
  author can violate it and produce a wrong read model; only tests and review
  catch this.
- **Local-ingestion order is per-replica and non-portable.** A projection
  checkpoint is meaningful only on the replica that produced it; it cannot be
  copied to another clone. This is correct (the index is gitignored and
  rebuildable) but is a sharp edge for anyone tempted to share checkpoints.
- **Rebuild-from-zero can be expensive.** Projections that opt into part (b)
  re-fold the entire canonical history on every structural-merge topology
  change. For large histories or frequent merges this is a real cost, traded for
  unconditional correctness.
- **Forward compensation can transiently expose an intermediate state.** Between
  ingesting a merged branch's diverged events and folding the later
  reconciliation compensation, a projection may briefly reflect a pre-correction
  state. The constraint guarantees the final state is correct, not that every
  intermediate read is.

## Alternatives Considered

### Alternative 1: A single dense, globally-monotonic shared position

Assign every event one position from a global counter so a cursor can never be
undercut, making projections trivially correct.

**Rejected because** this is impossible without a coordinator — a single
authority that hands out positions in agreed order across all replicas. The
file-store model forbids a coordinator: replicas append offline on different
machines with no synchronization until `git merge`. Any coordinator-free scheme
that claims a dense global order either smuggles a coordinator back in or loses
events when independently-minted positions collide or reorder at merge time.
This is the constraint that forces the whole design: positions are minted
independently (ADR-0039), and cross-replica order is only knowable post-merge.

### Alternative 2: Rewind the cursor to the merged event's canonical position

On detecting a merge that introduced a causally-earlier event, reset every
affected projection's checkpoint back to just before that event's canonical
position and let it re-read forward.

**Rejected because** it forces live projections to rewind, violating the
poll-based resumption contract (ADR-021). A running projection would reprocess
events it has already consumed — safe only for idempotent projections, and even
then it discards in-flight progress. It also makes cursor progress
non-monotonic, the exact property part (c) is designed to preserve. Rewinding
trades a clean "never skip" guarantee for a messy "sometimes replay" behavior
that most projections are not built to tolerate.

### Alternative 3: Always rebuild every projection from zero after any merge

Make rebuild-from-zero (part b) the _only_ behavior: any structural merge
invalidates all read models, which re-fold the full canonical history.

**Rejected because** it is correct but needlessly expensive for the common case.
Most projections can absorb a forward compensation (part a) and only need to
read a handful of new head events, not re-fold their entire history. Forcing a
full rebuild on every merge — even a trivial one — penalizes well-behaved
idempotent projections to accommodate the minority that cannot trust
compensation. Rebuild-from-zero is retained as the opt-in safety net (b), not
imposed as the universal policy.

### Alternative 4: Mutate or re-back-date events so canonical order matches append order

Edit the merged events' recorded positions (or insert renumbering records) so
that canonical order never disagrees with any replica's local cursor.

**Rejected because** it violates event immutability (ADR-005) and the file
store's foundational guarantee that event files are append-only and never edited
(ADR-0038). Mutating positions would also defeat the convergence property of
read-time linearization (ADR-0039): canonical order is convergent precisely
because it is derived from immutable recorded fields. Rewriting those fields
would make different replicas compute different orders and break the
deterministic merge.

## Related Decisions

- ADR-021: Poll-based projection runner with checkpoint resumption — the
  cursor-and-checkpoint contract this ADR must not violate.
- ADR-0039: Read-time linearization and `StreamVersion` as projection — defines
  the canonical order that governs state-folding and the per-replica rebuildable
  index that holds local-ingestion order.
- ADR-0042: Domain-owned reconciliation API — produces the compensation/merge
  events (via a command's `handle()`) whose correctness-under-reordering is the
  app-author constraint imposed here.
