---
name: fs-merge-mode
summary: Deterministic, domain-owned reconciliation of git-merged offline event histories for the file-based eventcore-fs store.
---

# Merge Mode (eventcore-fs)

Merge mode is the Layer-2 capability of the `eventcore-fs` file-based event store. It lets two replicas of an event-sourced history that were appended to offline — across two git clones, branches, or working copies — be `git merge`d back together and reconciled into a single valid, deterministically-ordered history, while never mutating an immutable event file. The library owns the _mechanism_ (fork detection, deterministic linearization, recording an N-parent merge transaction). The application owns the _policy_ (how a fork is resolved into compensation events). Merge mode lives outside the cross-backend `EventStore`/`EventReader` traits; the single-writer contract suite is untouched.

## Discovery

### Problem

Event-sourced developer tools increasingly want their event history to live _in git_, alongside the code that produces it. With `eventcore-fs`, each `append_events` writes one immutable, uniquely-named file under `events/`, so the history is just a directory of files that git tracks naturally.

The hard part is divergence. Two people — or one person across two clones, two branches, or an airplane-mode laptop and a desktop — each append events offline. Both build on the same starting point. Each independently advances a stream they think is at "version 6." Later, someone runs `git merge`. Because every transaction is a distinct file, the merge is a clean additive union with zero textual conflicts: git is happy. But the _history_ is now invalid — two divergent transactions both claim `account-1` at base version 6, the contiguous per-stream `StreamVersion` model has a duplicate, and the UUID7 global total order is unreliable across replicas (independent clocks, independent ID generators).

The combined history must reconcile into one valid history. Reconciliation has to be:

- **Deterministic** — every clone that ends up with the same set of files computes byte-identical linearized state and version assignments, with no coordination. Convergence cannot depend on who merged first or on wall-clock order.
- **Domain-owned** — what it _means_ to merge two divergent edits of the same account, document, or aggregate is a business decision (last-writer-wins, sum-the-deltas, raise-a-conflict-for-a-human). The library cannot know this. The application must supply the policy.
- **Non-destructive** — event files are immutable facts. Reconciliation may _add_ a merge transaction; it may never edit or delete an existing one.

### Target Users

Rust developers building **local-first / git-backed event-sourced tools**: CLI applications, desktop apps, and developer tooling that keep their authoritative state as a committed event log rather than in a hosted database. These users already reach for `eventcore-sqlite` for embedded single-process persistence; merge mode serves the subset whose persistence _is the git repository_ and who therefore inherit git's branching-and-merging model for their data.

### Value Proposition

Offline-first collaboration on an event-sourced domain with no server, no online coordination, and no hand-written merge-conflict resolution in raw files. Developers get git's familiar branch/merge workflow for their _data_, plus a typed, domain-owned reconciliation hook that turns a structural fork into ordinary domain events — produced by an ordinary command's `handle()` — so the rest of the application (projections, read models, command logic) keeps working unchanged. The guarantee that matters: **independent clones converge.** Given the same merged file set, every replica reaches identical state without talking to each other.

### Product Risks

- **Value** — _Will anyone want this?_ The bet is that local-first / git-backed tooling is a real and growing niche and that "your event log is just files in your repo" is a compelling story for it. The capability is purely additive: a user who never merges offline-divergent histories never pays for merge mode, so the downside of being wrong is low. Mitigation: ship Layer 1 (single-writer file backend) standalone first; merge mode is opt-in surface on top.
- **Usability** — _Can a developer actually use it?_ Reconciliation is expressed as something the developer already knows: a command whose `handle()` produces events. The library hands over a fully-folded `ForkContext` (ancestor state + per-branch events + affected streams); the developer does not touch the DAG, linearization, or version arithmetic. The principal usability hazard is the _idempotence constraint_ on reconciliation commands (see Projection Contract). Mitigation: make the constraint explicit and documented, and provide a topology-generation rebuild safety net for authors who cannot satisfy it.
- **Feasibility** — _Can it be built deterministically?_ Yes, and the mechanism is shared with Layer 1. A single read-time-linearization path builds a transaction DAG from recorded parent pointers, topologically sorts it, and breaks ties between concurrent transactions with the immutable triple `(created_at, replica_id, transaction_id)`. All inputs are recorded immutable values, so the sort is a pure function of the file set. The residual feasibility risk is _resolver determinism_: convergence of merge _content_ requires a `ForkResolver` to be a pure function of its `ForkContext`. A non-deterministic resolver merely produces a further reconcilable fork — it degrades, it does not corrupt.
- **Viability** — _Should we own this long-term?_ Merge mode is off-trait and crate-local to `eventcore-fs`, so it imposes zero maintenance burden on the postgres/sqlite/memory backends or the shared trait vocabulary. Its on-disk format reserves the merge header fields from Layer 1 day one, so adopting merge mode never forces an on-disk migration. The cost is bounded and isolated; the strategic upside (a differentiated local-first capability) is meaningful.

### Non-Goals

- **Not a general multi-master database.** Merge mode is offline-divergence reconciliation, not online multi-writer replication. There is no continuous sync protocol, no consensus, no liveness guarantee under concurrent online writers.
- **Not a replacement for postgres/sqlite in online multi-writer settings.** For concurrent online writers, use a backend with real transactional concurrency control. Merge mode targets the case where writers are _offline_ relative to each other and reconverge via git.
- **Reconciliation policy is the application's, not the library's.** The library detects forks and records merges; it never decides _how_ to merge. There is no built-in "last writer wins" or "sum the deltas" default.
- **Idempotent / compensating resolution is the application's read-side obligation.** The library guarantees a deterministic linear order and records compensation events as new heads. It does **not** guarantee that a projection which already ingested diverged events will end up correct unless the reconciliation command's events compensate to a correct read-model state _whether or not_ the diverged events were previously projected. Satisfying that is the app author's job.

## How It Works

### The Transaction DAG

Every `append_events` writes exactly one transaction file. Each transaction is a **node**; its header's `parent_transaction_ids` are **edges** to the writer's head transaction(s) at write time. In single-writer mode there is exactly one parent (the previous head), so the DAG is a simple chain.

A transaction's header also records, per stream it wrote, the `stream_bases[s]` — the `expected_version` the writer built on for stream `s`. This is the causality anchor for fork detection.

A **fork** on stream `s` exists when two transactions share the same `stream_bases[s]` (equivalently, the same optional `base_state_hash[s]`) and **neither is an ancestor of the other** in the DAG. Two writers both extended `account-1` from base version 6 without seeing each other's work. Ancestor reachability is computed once over the merged file set and memoized.

```
            ┌── T_b (replica B, base account-1 = 6)
   T_root ──┤
            └── T_a (replica A, base account-1 = 6)

   T_a and T_b: same stream_bases[account-1], no ancestor relation  ⇒  FORK
```

### Deterministic Linearization

The store never trusts the per-event `stream_version` written in a file — that value is _advisory_, the writer's locally-assigned guess. The canonical `StreamVersion` (and the global order) are **computed at read time**:

1. Build the DAG from `parent_transaction_ids`.
2. **Topologically sort** it, honoring DAG edges (a parent always precedes its children).
3. Break ties between _concurrent_ transactions (those with no ancestor relation) with the immutable triple **`(created_at, replica_id, transaction_id)`**. Every value is recorded and immutable, so every clone with the same file set computes the _identical_ linear order.
4. Walk the linear order per stream, assigning each event its canonical `StreamVersion` (0, 1, 2, …) by position.

Because the inputs are all recorded immutable values, linearization is a pure function of the file set: **same files ⇒ same order ⇒ same versions ⇒ same projected state.** That is the convergence guarantee.

**Single-writer mode degenerates to a chain.** With one writer the DAG has no forks, topological order equals append order equals UUID7 order, and computed versions equal the contiguous 0, 1, 2 … the contract suite expects. This is what makes "reserve the merge header fields in Layer 1" honest rather than dead code: Layer 1's linearizer genuinely _reads_ `parent_transaction_ids`, `replica_id`, and `stream_bases` from the first commit — it simply never finds a fork until a Layer-2 writer creates one.

### Why git Unions Are Conflict-Free

One file per transaction, UUID7-derived filenames that never collide, and an absolute never-edit rule together mean a `git merge` of `events/` is a **pure additive union**: branch A's files and branch B's files both land in the directory, and git sees no overlapping text to conflict on. The merged history is structurally invalid (it contains a fork) but _textually clean_ — and the read-time linearizer is what turns that clean union into a deterministic, reconcilable history. The discipline that _only_ `events/` is committed (all derived index, checkpoint, lock, and identity state is gitignored) is the load-bearing invariant the whole feature rests on.

## Reconciliation API

Merge mode is fs-specific, off-trait API on the file store:

```rust
fn detect_forks(&self) -> Vec<Fork>;
fn reconcile<R: ForkResolver>(&self, resolver: R) -> ReconcileReport;
fn status(&self) -> StoreStatus;
```

The reconciliation flow keeps the domain in charge:

1. **Detect.** The library finds a fork: a set of fork-head transactions that diverged from a common point on one or more streams.
2. **Compute ancestor state.** The library finds the lowest common ancestor (LCA) in the DAG and folds events up to the fork point using the **application's own `apply()`** to produce `ancestor_state`. The application's write-model logic is reused; the library supplies no fold of its own.
3. **Hand over the fork.** The library passes the application a `ForkContext`:

   ```rust
   struct ForkContext<State, Event> {
       ancestor_state: State,
       branches: Vec<Branch<Event>>,   // one per fork head: { replica_id, events }
       affected_streams: Vec<StreamId>,
   }
   ```

4. **Resolve as a command.** The `ForkResolver` returns a resolution **as a command to run** — never as raw events. The command's `handle()` produces the compensation/merge events, honoring the eventcore command pattern (typed state, pure `handle`, stream declarations). A multi-stream fork is presented as a single unit (the atomic transaction is the fork unit) and resolved by one multi-stream merge command.
5. **Record the merge.** The library appends an immutable **merge transaction** whose header lists _all_ fork-head transactions in `parent_transaction_ids` — a true N-parent merge node — and whose events are those produced by the resolution command's `handle()`. Nothing is edited or deleted; the divergent transactions remain in place as historical fact, now joined beneath a merge node.

`ResolutionOutcome::Unresolvable(reason)` is a first-class outcome. When the resolver declines (e.g., a genuine conflict needing a human, or schema-version skew between replicas), both branches stay in place, no merge node is recorded, and the affected stream is surfaced as in-conflict via `status()`. Merge mode never silently picks a winner.

**Recursion and termination.** Two independent reconciliations of one fork (two replicas each merged it offline) produce a fork _of merge nodes_ — which is itself the fork condition, so the same mechanism applies recursively. Each successful reconcile of a `k`-head fork replaces `k` heads with 1, strictly reducing the count of concurrent heads. The process monotonically converges to a single head once writers stop racing; it cannot loop forever.

## Projection Contract After Merge

A merge can insert causally-_earlier_ events behind a projection's checkpoint — the `EventReader` UUID7 cursor can be outrun by a reconciliation that linearizes new events ahead of where a projection has already read. The contract that keeps projections correct combines three layers:

- **(a) Compensating head events (the default).** Reconciliation expresses its outcome as new _head_ events (the compensation/merge events produced by the resolution command). A caught-up, cursor-based projection moves forward past those new heads and is corrected by them. The diverged history is not rewritten; the _correction_ is appended ahead.
- **(c) Per-replica local-ingestion cursor.** Cursor progress is tracked in each replica's _local ingestion order_, so a live projection never rewinds when a merge inserts causally-earlier events. The **canonical linearized order** governs state and version assignment; the **local ingestion order** governs cursor advancement. These are deliberately separate: convergence needs the canonical order, liveness needs the monotonic local order.
- **(b) Topology-generation rebuild safety net.** A topology-generation counter increments when a structural merge changes the DAG. Projections that opt out of trusting compensation events can detect a generation change and **rebuild from zero** against the freshly linearized order — guaranteed correct because the rebuild reads canonical order directly.

**The explicit app-author constraint.** A reconciliation command's events must compensate to a **correct read-model state whether or not the diverged events were already projected.** This is the read-side obligation called out in the non-goals. Authors who can satisfy it get live, no-rewind projections via (a)+(c). Authors who cannot fall back to topology-generation rebuilds via (b). The library guarantees deterministic order and records the compensations; it does not and cannot guarantee read-model correctness on the author's behalf.

## Git Integration

The integration contract makes git the transport and keeps everything derived out of the commit:

- **`.gitignore` set.** Only `events/` is committed. Ignored: `tmp/` (atomic-write staging), `checkpoints/` (projection progress), `locks/` (coordinator advisory locks), `index/` (rebuildable derived cache), `.eventcore/replica_id` (machine-local identity), and `.lock` (store-wide cross-process lock). Everything ignored is fully reconstructable from `events/`.
- **Defensive `.gitattributes`.** `events/** merge=union` is committed belt-and-suspenders. Because filenames never collide, a union merge is already conflict-free; the attribute guards against any future same-path edge case by preferring additive union over a conflict marker.
- **Read-time fsck.** On open and on read, the store validates each transaction file's content against its header anchor (content hash / `base_state_hash`). A file whose content does not match its anchor is rejected — this catches illegal hand-edits that a clean textual union could otherwise mask.
- **`DanglingTransaction` handling.** A partial or aborted git merge can leave a transaction whose `parent_transaction_ids` reference a file that did not come across. The store **never crashes and never silently drops** such a transaction; it reports it as a `DanglingTransaction` via `status()` so the application (or user) can resolve the incomplete merge.
- **Replica-id bootstrap on clone.** A fresh clone has no committed `replica_id` (it is gitignored). It generates one **lazily on first write**, bound to a working-copy fingerprint so a `cp -r` of a working tree gets a _different_ id on its next write (the safe outcome). A reconcile-time collision check fails loud with `ReplicaIdentityConflict` if two concurrent transactions carry the same `replica_id` but cannot have come from a single linear writer — rather than silently merging two histories that secretly share an identity.

## Scenarios

These Given/When/Then scenarios are the acceptance specification for merge mode. Each is realizable as an integration test that builds two stores on separate directories, simulates the git union of their `events/` directories, and exercises the off-trait API.

### Single-stream fork reconciles; both clones converge

```
Given replica A and replica B both start from the same committed history
  And A appends a transaction advancing account-1 from base version 6 (offline)
  And B appends a transaction advancing account-1 from base version 6 (offline)
When the events/ directories are union-merged into each clone
  And detect_forks() is called on each clone
Then each clone reports exactly one fork on account-1 with both transactions as heads
When a deterministic ForkResolver reconciles the fork on each clone independently
Then each clone records an N-parent merge transaction
  And both clones independently linearize to byte-identical state and version assignments
```

### Multi-stream fork resolved atomically

```
Given a single offline transaction on each replica wrote to account-1 and account-2
  And both transactions share stream_bases for account-1 and account-2 with no ancestor relation
When the histories are union-merged and detect_forks() is called
Then the fork is presented as one unit listing affected_streams = [account-1, account-2]
When a single multi-stream merge command resolves the fork
Then one merge transaction is recorded spanning both streams atomically
  And no partial reconciliation (one stream merged, the other not) is ever observable
```

### Recursive merge-of-merges converges and terminates

```
Given a fork was reconciled independently on replica A and on replica B
  And the two resulting merge transactions themselves form a fork (merge-of-merges)
When the histories are union-merged and detect_forks() is called
Then the fork of merge nodes is reported like any other fork
When it is reconciled
Then a single head remains
  And the count of concurrent heads strictly decreased at each reconcile step
  And the process terminates at one head (no infinite reconciliation loop)
```

### Copy-trap yields distinct replica ids or ReplicaIdentityConflict

```
Given a working copy is duplicated with cp -r before its replica_id is committed (it is gitignored)
When the copy performs its next write
Then it generates a different replica_id, bound to a different working-copy fingerprint
When two concurrent transactions nonetheless carry the same replica_id
  And they cannot have come from a single linear writer
Then reconcile fails loud with ReplicaIdentityConflict rather than silently merging
```

### Partial git merge surfaces DanglingTransaction

```
Given a git merge was aborted or applied partially
  And a transaction references a parent_transaction_id whose file did not come across
When the store is opened and status() is called
Then the orphaned transaction is reported as a DanglingTransaction
  And the store neither crashes nor silently drops the transaction
```

### A live projection sees the compensation; a rebuild-on-topology projection matches it

```
Given a live cursor-based projection has already ingested replica A's diverged events
When a fork is reconciled and compensation head events are appended
Then the live projection advances past the compensations and reaches the correct read-model state
  And it never rewinds its local-ingestion cursor
Given a second projection opts out of compensation-trust and rebuilds on topology-generation change
When the structural merge increments the topology generation
Then the second projection rebuilds from zero against the canonical linearized order
  And it reaches read-model state byte-identical to the live projection's
```

## Related Systems

- [store-backends](store-backends.md) — `eventcore-fs` is the file-based backend; merge mode is its off-trait Layer-2 surface (the cross-backend contract suite passes unchanged in single-writer mode).
- [event-sourcing](event-sourcing.md) — Multi-stream atomicity, optimistic concurrency, and immutability that merge mode preserves; the merge transaction is an additive append under these guarantees.
- [projection-system](projection-system.md) — The `EventReader` cursor, checkpointing, and coordination that the Projection Contract After Merge extends with local-ingestion cursors and topology-generation rebuilds.
- ADR-0038: File-Based Event Store Format and Atomicity — the immutable one-file-per-transaction JSONL format with reserved merge header fields.
- ADR-0039: Read-Time Linearization and `StreamVersion` as Projection — the shared topological-sort-with-tiebreak linearization path.
- ADR-0040: File-Store Locking and Projector Coordination — in-process append mutex, cross-process store lock, advisory-lock coordination.
- ADR-0041: Merge Causality via Transaction DAG — parent pointers + per-stream `base_version`/`base_state_hash` as the causality model.
- ADR-0042: Domain-Owned Reconciliation API — `ForkResolver` returning a command; library records an N-parent merge transaction.
- ADR-0043: Projection Behavior After Structural Merge — compensating head events, per-replica local-ingestion cursor, topology-generation rebuild, and the app-author idempotence constraint.
- ADR-0044: Replica Identity for Merge Mode — machine-local, gitignored, lazily-on-write identity with copy-trap fingerprinting and collision detection.
- ADR-0045: Merge Mode Outside the `EventStore` Trait — the off-trait, fs-specific API surface.
- ADR-0046: Git Integration Contract for the File Store — `.gitignore`/`.gitattributes` set, read-time fsck, `DanglingTransaction` handling, replica-id bootstrap on clone.
