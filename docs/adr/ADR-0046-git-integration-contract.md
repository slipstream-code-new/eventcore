# ADR-0046: Git Integration Contract for the File Store

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend (ADR-0038) exists so that event-sourced developer tools can keep their entire history inside an ordinary git repository. The motivating workflow is offline collaboration: two developers — or one developer across two clones or two branches — each append events while disconnected, then later reconcile their histories with a plain `git merge`. The whole point of the file store is that git, not a database server, is the synchronization mechanism.

Git is a textual, line-oriented merge tool. It knows nothing about event sourcing, transaction DAGs, or `StreamVersion`. If the on-disk format invites git to merge _within_ a file — interleaving lines from two divergent branches into one file — the result is a syntactically and semantically corrupt event log, and the user discovers it only when the store fails to load. The file store must therefore define an explicit, defensive **contract with git** that guarantees a merge of the events directory is always a safe, mechanical operation, and that the store remains robust even when git behaves in surprising ways (partial merges, aborted merges, a misconfigured working copy, or a malicious edit).

**Key forces:**

1. **Merges must never produce textual conflicts.** A conflicting merge halts the user mid-flow, demands manual resolution of a file format they should never have to read, and risks them "resolving" the conflict by hand-editing immutable event data. The format must make conflicts structurally impossible for the additive case.
2. **The events directory is the single source of truth.** Everything else the store writes — staging files, the rebuildable index, projection checkpoints, coordinator lock files, this clone's replica identity — is derived or machine-local. Committing any of it pollutes history, causes spurious merge churn, and (in the case of replica identity, ADR-0044) actively breaks the merge-mode causality model by sharing a per-clone identity across clones.
3. **Git is not a trusted, well-behaved peer.** A user can hand-edit a committed file, a `git merge` can be aborted partway, a `merge=union` driver can silently splice two files together, and a fresh clone arrives with none of the local state the store normally maintains. The store cannot assume the file set on disk is the file set it last wrote. It must treat whatever files are present as authoritative at read time and degrade gracefully — surfacing problems through diagnostics, never through a crash or a silent data loss.
4. **Safety must be layered.** No single mechanism is sufficient. File immutability prevents conflicts in the common case; a defensive merge driver protects the additive case when git tooling is involved; a read-time integrity check catches edits that a merge driver would otherwise mask; dangling-transaction handling absorbs incomplete merges. Each layer covers a failure mode the others do not.

**Why this decision now:**

ADR-0038 fixes the on-disk format as one immutable, uniquely-named JSONL file per transaction, and that format is frozen before any code is written. The git contract is the consequence of that format and the precondition for merge mode (ADR-0041 through ADR-0045). The `.gitignore` and `.gitattributes` content, the read-time integrity check, and the dangling-transaction handling are all things the store creates or enforces from the very first write. They must be specified together with the format they protect, not retrofitted after collaboration breaks in the field.

## Decision

EventCore's file store defines a git integration contract with four parts. Together they make a `git merge` of the events directory a safe, additive operation and keep the store robust against the ways git can surprise it.

### 1. Immutability makes merges conflict-free

Every transaction is written to exactly one file, named by its `transaction_id` (a UUIDv7 minted once per `append_events`, per ADR-0038), and that file is **never edited or deleted** after the atomic rename that publishes it. Two writers — on two clones or two branches — therefore never write to the same path: distinct UUIDv7 transaction IDs guarantee distinct filenames. A `git merge` of `events/` sees only _added_ files on each side and combines them as a pure set union with **zero textual conflicts**. There is no path on which git is ever asked to reconcile two versions of the same file, because no file ever has two versions.

This is the load-bearing property of the entire feature. Reconciling the _content_ of divergent histories — assigning canonical `StreamVersion`, resolving forks — is the job of read-time linearization (ADR-0039) and merge mode (ADR-0041, ADR-0042), which run over the unioned file set inside the store. Git's only job is to union the files, and the immutable-file-per-transaction format guarantees it can always do exactly that.

### 2. `.gitignore` commits only the source of truth

The store writes a `.gitignore` into the repository root on initialization. It excludes **all** derived and machine-local state, leaving only `events/` committed:

```gitignore
tmp/
index/
checkpoints/
locks/
.eventcore/replica_id
.lock
```

- `tmp/` — atomic-write staging; never part of history.
- `index/` — the rebuildable read-model cache; fully reconstructable from `events/` (ADR-0039), so committing it would be redundant and would generate merge churn.
- `checkpoints/` — per-subscription projection progress; local to each clone's projections.
- `locks/` — projector coordinator advisory lock files (ADR-0040); meaningless outside the running process.
- `.eventcore/replica_id` — **this clone's identity** (ADR-0044). Committing it would share one identity across every clone, defeating merge-mode causality. It is intentionally per-clone and gitignored.
- `.lock` — the store-wide cross-process advisory lock (ADR-0040); a runtime artifact.

Only `events/` is committed, and `events/` is the single source of truth: the complete state of the store is reconstructable from the committed event files alone. Everything else is rebuilt or regenerated on open.

### 3. Defensive `.gitattributes merge=union` — belt and suspenders

The store also writes a `.gitattributes` declaring a union merge driver for event files:

```gitattributes
events/** merge=union
```

In the contract's normal operation this driver should never fire, because §1 guarantees git never merges within a transaction file — the two sides only ever add distinct files. The `merge=union` entry is a deliberate belt-and-suspenders defense: if some tooling or workflow _did_ present git with two versions of one path, union-merging them (taking all lines from both sides) is strictly safer than emitting conflict markers, which would corrupt the JSONL and halt the user.

This protection has an explicit, documented limitation. A union merge is safe only for the **additive** case. If a transaction file were _illegally edited_ on one side, `merge=union` would silently splice the edited and original lines together, producing a semantically corrupt file with no conflict marker to warn anyone. In other words, the merge driver protects the additive case but can _mask_ an illegal edit. It is therefore not sufficient on its own — it must be paired with the read-time integrity check in §4, which catches exactly the case the merge driver cannot.

### 4. Read-time integrity (`fsck`) and partial-merge robustness

The store treats the set of files present in `events/` as authoritative at read time. It does not assume the file set matches what it last wrote, and it never crashes or silently drops data because of an unexpected file set. Two read-time mechanisms enforce this:

**Content-hash fsck against the header anchor.** When loading each transaction file, the store recomputes a content hash over the file's event payload and compares it against the integrity anchor recorded in the transaction header. A mismatch means the file was edited after it was written — exactly the illegal-edit case that `merge=union` (§3) would otherwise mask. Such a file is **rejected**, surfaced through `status()`, and excluded from the linearized history, rather than being parsed as if it were trustworthy. This closes the gap left by the union merge driver: an edit a union would hide becomes a hard, visible integrity failure.

**Dangling-transaction handling for partial/aborted merges.** A `git merge` can be interrupted or aborted, leaving the working tree with some transaction files present and others — including transactions referenced as parents — absent. The store treats the present file set as the source of truth and is robust to missing files. A transaction whose `parent_transaction_ids` reference files that are not present on disk is classified as a **`DanglingTransaction`** and surfaced via `status()`. It is never a crash, never an unhandled error, and never a silent drop. When the rest of the merge completes (the missing files arrive), the dangling reference resolves on the next read and the transaction rejoins the DAG. This makes a half-finished git operation a recoverable, observable state rather than a fatal one.

**Fresh-clone replica identity.** Because `.eventcore/replica_id` is gitignored (§2), a fresh `git clone` arrives with the full `events/` history but **no committed replica identity**. This is correct: the new clone is a distinct replica and must have a distinct identity. It generates its `replica_id` **lazily on its first write**, per ADR-0044. A clone that only reads never needs one; a clone that writes mints its own, ensuring its transactions are correctly attributed in the merge-mode causality model.

### Summary of the layered contract

| Layer                         | Mechanism                                           | Failure mode it covers                                                |
| ----------------------------- | --------------------------------------------------- | --------------------------------------------------------------------- |
| Format (ADR-0038)             | One immutable, UUIDv7-named file per transaction    | Textual merge conflicts (made structurally impossible)                |
| `.gitignore`                  | Commit only `events/`                               | History pollution, merge churn, shared/leaked replica identity        |
| `.gitattributes merge=union`  | Defensive union driver                              | Conflict markers if tooling ever merges within a file (additive case) |
| Read-time fsck                | Content hash vs. header anchor                      | Illegal edits — including those a union merge would mask              |
| Dangling-transaction handling | Missing-parent transactions reported via `status()` | Partial/aborted git merges                                            |
| Lazy replica identity         | Generated on first write in a fresh clone           | A clone inheriting or lacking an identity                             |

## Consequences

### Positive

- **Conflict-free collaboration.** Because every transaction is an immutable, uniquely-named file, merging the events directory is always a pure additive union. Developers never see git conflict markers in event data and are never tempted to hand-resolve them.
- **History stays clean.** Only the source-of-truth events are committed. Index rebuilds, checkpoint movement, lock churn, and per-clone identity never touch git, so diffs and merges contain only real new events.
- **Correct multi-clone identity for free.** Gitignoring `replica_id` and generating it lazily on first write means each clone is automatically a distinct replica without any user action, which is exactly what merge-mode causality (ADR-0044) requires.
- **Defense in depth.** Each layer covers a failure the others cannot. The union driver protects against errant tooling; the fsck catches the edits the union driver would mask; dangling handling absorbs incomplete merges. No single misbehavior of git or the user can silently corrupt the store.
- **Partial merges are recoverable, not fatal.** An aborted `git merge` leaves the store in an observable, self-healing state reported through `status()`, not a crash. When the rest of the files arrive, dangling references resolve automatically.
- **The store is robust to the file set it finds.** Treating `events/` as authoritative at read time means a clone, a stash pop, a cherry-pick, or any other git operation that changes which files are present is handled by the same load path, with no special cases.

### Negative

- **The fsck has a per-file cost.** Recomputing and comparing a content hash for every transaction file on load adds work proportional to history size. For the file store's intended scale (developer-tool histories) this is acceptable, and it can be cached behind the rebuildable `index/` later; but it is real overhead the database backends do not pay.
- **`merge=union` can still mask an illegal edit until read time.** The defensive driver does not _prevent_ a masked edit; it only ensures git does not emit conflict markers. The illegal edit is caught by the fsck, but only when the store next loads — not at merge time. The contract makes corruption _detectable_, not _impossible_.
- **The contract depends on `.gitattributes` being honored.** `merge=union` only takes effect if the repository's git configuration respects the committed `.gitattributes`. A user who strips or overrides it loses the defensive layer (though §1 means the additive case still never conflicts, and §4 still catches edits). The store cannot enforce git configuration.
- **Dangling state is the user's to resolve.** The store reports a `DanglingTransaction` but cannot fetch the missing files itself — completing the merge is a git operation the user performs. The store's responsibility ends at surfacing the condition clearly.
- **A fresh read-only clone has no replica identity until it writes.** This is intended, but it means any fs-specific API that assumes a `replica_id` must tolerate its absence on a clone that has not yet written.

## Alternatives Considered

### Alternative 1: Mutable, append-in-place log files (e.g. one file per stream)

Store each stream as a single growing file and append new events to the end of it.

**Rejected because** two clones appending to the same stream file produce two divergent versions of one path, which is precisely the case git cannot merge without conflict. Even with `merge=union`, concurrent appends to the same file interleave by line and silently corrupt event ordering and framing. The whole conflict-free property of the contract depends on no two writers ever touching the same file — which only the immutable-file-per-transaction format guarantees.

### Alternative 2: Rely on `merge=union` alone (no read-time fsck)

Trust the union merge driver to keep merges safe and skip the content-hash integrity check.

**Rejected because** `merge=union` is safe only for additive merges. It cannot distinguish a legitimate union of two added files from a union of an _edited_ file with its original — and in the edit case it silently produces a corrupt file with no warning. Without the fsck, an illegal hand-edit (or a tool that rewrites a file) would be masked indefinitely and only manifest as mysterious downstream corruption. The fsck is the layer that makes the masked case detectable, so the two mechanisms are paired, not interchangeable.

### Alternative 3: Commit the index/checkpoints/replica identity for faster startup

Commit the rebuildable index and per-clone state so a fresh clone need not rebuild on open.

**Rejected because** every one of those artifacts is either derived (the index and checkpoints, fully reconstructable from `events/`) or machine-local (replica identity, locks). Committing derived state causes constant merge churn and the risk of an index that disagrees with the events it indexes. Committing replica identity is actively harmful: it would share one identity across every clone, breaking the per-clone attribution that merge-mode causality (ADR-0044) depends on. The startup cost of rebuilding from the source of truth is the correct price for keeping history clean and identity per-clone.

### Alternative 4: Treat a missing-parent transaction as a hard error on open

Fail to open the store, or raise an error, when a transaction references a parent file that is not present.

**Rejected because** a partial or aborted `git merge` legitimately produces this state, and it is transient — the missing files arrive when the merge completes. Treating it as fatal would make the store unusable in exactly the collaboration scenario it exists to support, and would punish the user for an ordinary, recoverable git workflow. Classifying it as a `DanglingTransaction` surfaced through `status()` keeps the store open and self-healing while still making the incomplete state visible.

### Alternative 5: A custom git merge driver that understands the event format

Ship a domain-aware git merge driver that parses transaction files and reconciles event histories at merge time.

**Rejected because** it puts event-sourcing semantics (DAG linearization, fork detection, domain-owned reconciliation) inside a git hook, where it would have to be installed and configured per repository, would run outside the store's control, and would couple merge correctness to a fragile external integration point. The contract deliberately keeps git's role trivial — union the files — and performs all semantic reconciliation inside the store at read time (ADR-0039) and through the merge-mode API (ADR-0042). A clone with no special git configuration must still merge correctly; only the immutable-file format and read-time linearization can guarantee that.

## Related Decisions

- ADR-0038: File-Based Event Store Format and Atomicity — defines the immutable, UUIDv7-named one-file-per-transaction format and the header integrity anchor this contract relies on.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection — performs the semantic reconciliation of unioned histories that the git union enables, and owns the rebuildable index that is gitignored here.
- ADR-0040: File-Store Locking and Projector Coordination — defines the lock files (`.lock`, `locks/`) and checkpoints excluded by `.gitignore`.
- ADR-0044: Replica Identity for Merge Mode — defines the gitignored, lazily-generated-on-first-write replica identity that a fresh clone produces.
- ADR-0045: Merge Mode Outside the EventStore Trait — defines the fs-specific `status()` surface through which dangling transactions and integrity failures are reported.
- ADR-005: Event Metadata Structure — establishes UUIDv7 for time-ordered identity, the basis for unique transaction filenames.
- ADR-001: Multi-Stream Atomicity Implementation Strategy — the all-or-nothing transaction guarantee preserved by one immutable file per transaction.
