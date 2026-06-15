# ADR-0042: Domain-Owned Reconciliation API

## Status

Accepted

## Date

2026-06-12

## Context

ADR-0041 establishes that the `eventcore-fs` backend records causality as a
transaction DAG: every transaction file's header carries
`parent_transaction_ids` (the writer's head transaction(s) at write time) and
`stream_bases` (the `expected_version` the writer built on for each stream).
After a `git merge` performs an additive union of immutable transaction files,
the DAG can contain a **fork**: two transactions that share the same
`stream_bases[s]` for some stream `s` and where neither is an ancestor of the
other. ADR-0039's read-time linearization deterministically orders such a fork
so that every clone computes byte-identical `StreamVersion` values, but a
deterministic _ordering_ of two divergent histories is not the same as a
_reconciliation_ of them. Two people who each appended events offline against
the same base have made independent business decisions; deciding what the
combined history should mean is a domain question, not a storage question.

EventCore's design principle of **infrastructure neutrality** (ADR-010, the
free-function API; the `design-principles` rule) draws a hard line here: the
library owns infrastructure concerns and never assumes a particular business
domain. It cannot know whether two concurrent deposits to `account-1` should
sum, whether the later one should win, whether they represent a double-submit
to deduplicate, or whether they are a genuine conflict that a human must
resolve. The reconciliation **policy** belongs to the application. What the
library can and must own is the **mechanism**: surfacing the fork, computing
the state at the fork point, and recording the application's decision as a new
immutable transaction that preserves every invariant the rest of EventCore
depends on.

This creates a direct tension with the `eventcore-command-pattern` rule, which
states that **every event must originate from a command's `handle()` method**.
A reconciliation cannot be allowed to hand-construct compensation or merge
events directly and append them — that would bypass `handle()`, skip the
command's own precondition validation, and create events that no command could
have produced. Yet reconciliation manifestly _does_ need to produce events: the
combined history is incomplete until some new fact records how the divergence
was resolved. The reconciliation API must therefore let the application express
its decision **as a command to run**, so that the command's `handle()` produces
the resolution events the same way every other event in the system is produced.

Several questions must be settled to make this mechanism correct and
convergent:

1. **What does the library hand the application, and what does it get back?**
   The application needs enough context to make a domain decision (the state at
   the fork point and the divergent branches), and the library needs a result
   it can record without bypassing the command pattern.
2. **How is the result recorded in the DAG?** ADR-0041 makes the DAG the source
   of truth for causality; a reconciliation must extend it honestly.
3. **How is multi-stream atomicity (ADR-001) preserved?** A single atomic
   transaction can touch several streams, so a fork can span several streams at
   once. Presenting and resolving those streams independently would let a
   reconciliation commit a partial result, violating ADR-001.
4. **Which execution path produces the merge events?** EventCore's public
   `execute()` (ADR-010) is the canonical entry point, but the shared
   write-path types it depends on (`ExecutionResponse { attempts }`,
   `EventStreamSlice` as a unit struct, `StreamWriteEntry`) are deliberately
   thin and currently stubbed. Routing reconciliation through public
   `execute()` would couple merge mode to those types and likely force them to
   change.
5. **What happens when the application cannot decide?** A silent automatic pick
   would corrupt history with a decision no one made.
6. **What guarantees must the application's resolver provide for convergence?**
   Determinism of the _ordering_ (ADR-0039) is not enough; the _content_ of the
   merge must also converge across clones.
7. **What about schema-version skew?** Per ADR-0035, two replicas may run
   different event-enum versions. A replica that cannot deserialize a peer's
   event variant cannot fold the history to the fork point at all.

This ADR settles the reconciliation API. It depends on the causality model of
ADR-0041, defines the inputs to the projection-after-merge policy of ADR-0043,
and is realized through the off-trait merge surface of ADR-0045.

## Decision

**Reconciliation policy is owned by the application; the library owns the
reconciliation mechanism.** On a detected fork, the `eventcore-fs` store
performs the mechanical work — locating the fork point, reconstructing the
state there, and presenting the divergence — and the application supplies a
domain decision, expressed as a command, which the library then records as a
new immutable merge transaction.

### The fork unit is the atomic transaction

Because a single `append_events` writes one immutable transaction file that may
touch several streams (ADR-001), the unit of divergence is the **transaction**,
not the individual stream. When a fork is detected, the library presents _all_
streams affected by the diverging transactions as **one unit**. A multi-stream
fork is resolved by **one multi-stream merge command**, so the resolution is
itself a single atomic transaction. This preserves ADR-001: there is no way to
commit a reconciliation that resolves some affected streams and leaves others
inconsistent.

### `ForkContext`: what the library hands the application

For a detected fork, the library:

1. Computes the **lowest common ancestor (LCA)** of the diverging fork heads in
   the DAG — the most recent transaction from which all branches descend.
2. Folds the events from the root up to the fork point using **the
   application's own `apply()`** (the same `CommandLogic::apply` the resolution
   command will use), producing `ancestor_state` — the command-local state as
   it stood at the moment the branches diverged.
3. Assembles the divergent branches: for each fork head, the `replica_id` that
   produced it and the ordered list of events that branch contributed after the
   fork point.

It hands the application a `ForkContext`:

```
ForkContext {
    ancestor_state,                 // State at the fork point, via the app's apply()
    branches: [ { replica_id, events } ],  // One per fork head
    affected_streams,               // Every stream touched by the diverging transactions
}
```

The folding uses the application's `apply()` precisely because the write-model
state is the application's own type (`cqrs-model-separation`): the library has
no other way to materialize "the state the writers were building on" in terms
the resolution command will understand.

### The resolution is a command, never raw events

The application implements a `ForkResolver` that inspects the `ForkContext` and
returns a `ResolutionOutcome`. A successful outcome is expressed **as a command
to run** — never as hand-constructed events. The library runs that command so
that its `handle()` produces the compensation/merge events. This honors the
`eventcore-command-pattern` rule end to end: the resolution events are produced
by a `handle()` like every other event in the system, with the command's own
precondition validation intact. The application decides _what_ to do; the
command pattern decides _how the resulting facts are produced_.

### Recording the result: an N-parent merge transaction

The library records the command's output as a new, append-only **merge
transaction**. Its header lists **all** of the fork-head transaction IDs in
`parent_transaction_ids` — a true N-parent merge node in the DAG. This is the
one mechanism that collapses the fork: a `k`-head fork, once reconciled,
descends from all `k` heads, so the read-time linearizer (ADR-0039) sees a
single head where it previously saw `k` concurrent ones. Nothing is mutated;
the diverging transactions remain on disk exactly as committed, and the merge
transaction is layered on top as a new fact.

### Reconcile runs through an fs-internal handle()-driven path, not public `execute()`

The reconcile path uses an **fs-internal, `handle()`-driven merge-append**
rather than the public `execute()` entry point. The decision criterion is
isolation of the shared write-path types: routing reconciliation through public
`execute()` would couple merge mode to `ExecutionResponse { attempts }` and the
unit-struct `EventStreamSlice`, almost certainly forcing those deliberately
thin, currently-stubbed types to grow merge-specific shape. Keeping reconcile
fs-internal lets it call the resolution command's `handle()` directly with the
`ancestor_state` already folded, append the resulting events as the N-parent
merge transaction through the same atomic-write path used by `append_events`,
and leave the shared types untouched. The command pattern is still honored —
events still come from `handle()` — but the _plumbing_ that invokes `handle()`
is fs-specific, consistent with merge mode living off-trait (ADR-0045).

### Unresolvable forks are surfaced, never silently picked

A `ForkResolver` may return `ResolutionOutcome::Unresolvable(reason)` when no
automatic domain decision is appropriate (for example, a genuine conflict a
human must adjudicate). In that case the library **leaves both branches in
place** and surfaces the affected stream as in-conflict via `status()`
(ADR-0045). It never silently picks a winner. An unreconciled fork is a visible,
queryable state — not a corruption and not a hidden default. The application (or
a human operating it) can return later with a resolver that _can_ decide.

### Resolver output must be a pure function of its `ForkContext`

It is a **documented requirement** that a `ForkResolver`'s output be a pure
function of its `ForkContext`. ADR-0039 guarantees that every clone computes the
same linear _order_ and therefore the same `ForkContext` for a given fork; the
_content_ of the merge converges across clones **only if** the resolver maps
that identical `ForkContext` to an identical resolution command. A resolver that
consults wall-clock time, a random source, machine-local configuration, or any
input outside its `ForkContext` will produce different merge events on different
clones. The failure mode is benign but real: divergent merge transactions are
themselves concurrent, so they form a _further_ fork that the same mechanism
must reconcile again — convergence is delayed, not lost, but a non-pure resolver
can prevent the system from ever reaching a single head while writers race.
Purity is the application's obligation; the library documents it as the
precondition for content convergence.

### Schema-version skew blocks reconcile loudly

Per ADR-0035, event variants evolve as enum variants, and replicas may run
different versions of the event enum at reconcile time. Folding `ancestor_state`
and running the resolution command both require deserializing the peer
branches' events. If a replica encounters an event variant it cannot
deserialize — a peer recorded events using a newer schema this replica does not
yet understand — it **cannot** fold the history to the fork point and therefore
cannot reconcile correctly. The default posture is to **block reconcile loudly**
with an upgrade-required error rather than attempt a partial or guessed fold.
Silently skipping undeserializable events would fold an incomplete
`ancestor_state` and produce a wrong resolution. General upcasting (ADR-0035
territory) is out of scope here; until it exists, the safe behavior is to refuse
and tell the operator to upgrade.

## Consequences

### Positive

- **Policy/mechanism split respects infrastructure neutrality.** The library
  never embeds a merge strategy; applications express domain-specific
  reconciliation while the library guarantees the structural invariants
  (immutability, DAG honesty, atomicity).
- **The command pattern is preserved end to end.** Every resolution event is
  produced by a `handle()`, so reconciliation events are indistinguishable in
  provenance from any other event and carry the command's own validation.
- **The DAG stays honest.** Recording an N-parent merge node, rather than
  rewriting or deleting diverging transactions, keeps the causality model of
  ADR-0041 intact and makes reconciliation auditable: the fork and its
  resolution are both permanently visible.
- **ADR-001 atomicity survives merge mode.** Multi-stream forks are presented
  and resolved as a single unit, so no reconciliation can commit a partial
  multi-stream result.
- **Shared write-path types are untouched.** Routing reconcile through an
  fs-internal `handle()`-driven path keeps `ExecutionResponse`,
  `EventStreamSlice`, and `StreamWriteEntry` thin and stub-shaped, isolating
  the experimental merge feature from the stable cross-backend types.
- **No silent corruption.** `Unresolvable` surfaces conflicts through
  `status()`; convergence requires an explicit, recorded decision, never a
  hidden default.
- **Convergence is well-defined and self-healing.** With pure resolvers,
  content converges; even with imperfect resolvers the worst case is a further
  reconcilable fork, not data loss.

### Negative

- **Convergence depends on an application contract the library cannot enforce.**
  Resolver purity is a documented requirement, not a compiler-checked one (the
  same trust-boundary posture as ADR-013's contract-testing approach). A
  non-pure resolver silently delays convergence; runtime detection of impurity
  is a possible future safety net but is not part of this decision.
- **Two execution paths now produce events.** Public `execute()` for normal
  commands and the fs-internal merge-append for reconciliation. The fs-internal
  path must be kept behaviorally faithful to the command pattern, and the
  duplication is a maintenance cost justified only by keeping the shared types
  stub-shaped.
- **Schema skew hard-blocks reconciliation.** Until upcasting exists, a fleet of
  replicas on mismatched event-enum versions cannot reconcile and must be
  upgraded first. This is the safe choice but it is operationally rigid.
- **Resolver authors carry real cognitive load.** Writing a correct resolver
  requires understanding the `ForkContext`, the purity requirement, and (per
  ADR-0043) the read-side compensation/idempotence obligation. This is
  irreducible: domain-owned reconciliation means domain authors own the hard
  parts.
- **Reconciliation is explicit, not automatic.** The application must call into
  the merge API and run resolvers; forks do not self-heal on store open. This is
  consistent with the off-trait, explicit surface of ADR-0045 but means an
  unattended store can accumulate unresolved forks visible only via `status()`.

## Alternatives Considered

### Alternative 1: Library-chosen automatic merge strategy

Have `eventcore-fs` apply a built-in reconciliation rule (last-writer-wins by
linearized order, or a CRDT-style merge) with no application involvement.

**Rejected because** it violates infrastructure neutrality. The library cannot
know the domain meaning of two concurrent histories; any built-in rule would be
wrong for most domains and silently corrupt history for the rest. It also
contradicts the `eventcore-command-pattern` rule, since a library-chosen merge
would synthesize events that no command produced. The right default for an
ambiguous merge is to ask the domain, not to guess.

### Alternative 2: Resolver returns hand-constructed events

Let the `ForkResolver` return the compensation/merge events directly, which the
library appends as the merge transaction.

**Rejected because** it bypasses `handle()`, directly violating the rule that
every event originates from a command. Hand-constructed events skip the
command's precondition validation and produce facts no command could have
produced, breaking the uniform provenance the rest of EventCore relies on.
Returning a _command_ keeps event production where it belongs.

### Alternative 3: Route reconciliation through public `execute()`

Reuse the canonical `execute()` entry point (ADR-010) to run the resolution
command, treating reconciliation as an ordinary command execution.

**Rejected for now** because `execute()` is built on the shared write-path types
(`ExecutionResponse { attempts }`, the unit-struct `EventStreamSlice`,
`StreamWriteEntry`), which are deliberately thin and currently stubbed. Threading
N-parent merge semantics and the pre-folded `ancestor_state` through `execute()`
would force those cross-backend types to grow merge-specific shape, coupling a
stable, multi-backend API to an experimental fs-only feature. The fs-internal
`handle()`-driven path achieves the same command-pattern guarantee while keeping
the shared types untouched. This may be revisited if merge mode is ever
generalized beyond `eventcore-fs`.

### Alternative 4: Per-stream fork resolution

Detect and resolve forks one stream at a time, presenting each affected stream
to the resolver independently.

**Rejected because** it breaks ADR-001 multi-stream atomicity. A single atomic
transaction may touch several streams; resolving them independently would let a
reconciliation commit a result for some streams while leaving others
unreconciled, producing exactly the partial-write state that the atomic
transaction boundary exists to prevent. The fork unit must be the transaction,
and a multi-stream fork must be resolved by one multi-stream command.

### Alternative 5: Silently pick a winner for unresolvable forks

When a resolver cannot decide, default to one branch (for example, the
linearized winner) so the store always converges to a single head.

**Rejected because** it records a decision no one made and discards the other
branch's events from the active history without an explicit, auditable choice. A
genuine conflict that needs human judgment would be resolved by accident.
`Unresolvable` surfacing through `status()` keeps the conflict visible and
queryable until someone resolves it deliberately; an unresolved fork is a safe,
recoverable state, whereas a silent pick is irreversible information loss.

### Alternative 6: Best-effort fold across schema-version skew

When a peer branch contains an event variant this replica cannot deserialize,
skip the unknown events and fold whatever remains.

**Rejected because** a partial fold yields a wrong `ancestor_state`, and the
resolution command would then make a domain decision on incorrect state. The
resulting merge transaction would be confidently wrong and would itself diverge
from the merge other (upgraded) replicas compute. Blocking loudly with an
upgrade-required error is the only safe behavior until general upcasting
(ADR-0035) exists.

## Related Decisions

- ADR-001: Multi-Stream Atomicity — the fork unit is the atomic transaction;
  multi-stream forks are resolved by one multi-stream merge command.
- ADR-010: Free-Function API Design — establishes `execute()` as the canonical
  entry point and the infrastructure-neutrality principle that motivates the
  policy/mechanism split; reconcile uses an fs-internal variant to keep the
  shared write-path types stub-shaped.
- ADR-0035: Event Schema Evolution via Enum Variants — the source of
  schema-version skew that makes reconcile block loudly when a peer's event
  variant cannot be deserialized.
- ADR-0041: Merge Causality via Transaction DAG — supplies the DAG,
  `parent_transaction_ids`, and `stream_bases` that this API reads to compute
  the LCA and record the N-parent merge transaction.
- ADR-0043: Projection Behavior After Structural Merge — consumes the
  reconciliation outcomes (compensation/merge events) this API produces and
  defines the read-side idempotence obligation on the application.
- ADR-0045: Merge Mode Outside the EventStore Trait — places `detect_forks`,
  `reconcile`, and `status` on the fs-specific surface, off the cross-backend
  `EventStore`/`EventReader` traits, which is what permits the fs-internal
  reconcile path.
