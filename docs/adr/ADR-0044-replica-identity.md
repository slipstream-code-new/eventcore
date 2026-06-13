# ADR-0044: Replica Identity for Merge Mode

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend (ADR-0038) persists each `append_events` call as one
immutable JSONL transaction file, named by a UUIDv7 `transaction_id`. Layer 2
("merge mode") allows two working copies of the same git repository to append
events offline and later combine their histories via `git merge`, which is a
pure additive union of the immutable `events/` directory (ADR-0046). Once
combined, the read-time linearizer (ADR-0039) builds a transaction DAG from the
recorded `parent_transaction_ids` and topologically sorts it with a
deterministic tiebreak.

That tiebreak — and the fork-detection logic that drives reconciliation
(ADR-0041) — depends on being able to answer a question the single-writer model
never had to ask: **which independent writer produced this transaction?** Two
transactions are _concurrent_ (a candidate fork) when they build on the same
per-stream base yet neither is an ancestor of the other in the DAG. To resolve
such forks deterministically and to detect impossible histories, the store must
attribute every transaction to the working copy that wrote it.

EventCore already mints a UUIDv7 `event_id` per event and a UUIDv7
`transaction_id` per write (ADR-005's UUIDv7 ordering rationale carries over).
But neither identifies the _writer_. A transaction id is unique per write; an
event id is unique per event; neither is stable across the many writes a single
working copy performs over its lifetime. Merge mode needs a third, distinct
notion: a **replica identity** that is stable for one working copy and
different across working copies.

**Key forces:**

1. **Deterministic linearization.** The linearization tiebreak is
   `(created_at, replica_id, transaction_id)` (ADR-0039). For every clone to
   compute byte-identical state from the same file set, `replica_id` must be a
   recorded, immutable value on each transaction — not recomputed at read time.

2. **Fork attribution and collision detection.** Fork detection (ADR-0041)
   reasons about which writer produced which branch. If two genuinely
   independent writers share an identity, the store cannot tell "one linear
   writer made both of these" apart from "two writers diverged," and a real
   fork becomes invisible.

3. **The copy trap.** Working copies are created by copying: `git clone`,
   `cp -r`, restoring a backup, or branching a filesystem snapshot. Any identity
   that travels with the copied bytes is duplicated by the copy. Two writers
   then silently share one identity. This is the worst failure class in the
   whole feature — not a crash, not a conflict, but _silent corruption_: forks
   that should be detected and reconciled instead disappear into a history that
   looks linear but is not.

4. **Single-writer mode must stay inert.** The Layer 1 contract suite (the 19
   tests of ADR-0038/ADR-0040) runs in single-writer mode and must pass
   unchanged. Replica identity is recorded in every transaction header from day
   one (so the immutable format never changes — ADR-0038), but none of its
   merge-mode machinery may activate, allocate, or fail in single-writer use.

5. **Two distinct things share a name.** "Replica identity" describes both (a)
   the immutable, committed record of _which replica wrote a given transaction_,
   and (b) the machine-local, mutable record of _this working copy's current
   identity for new writes_. These are different in lifetime, location, and
   mutability, and conflating them is exactly the mistake that produces the copy
   trap.

6. **Infrastructure neutrality (ADR-010 / design principles).** The library owns
   the identity mechanism; it must not assume a particular machine, user, or
   deployment topology, and it must let an operator override the identity
   explicitly when the environment demands it (containers with ephemeral
   filesystems, CI runners, deliberate replica provisioning).

## Decision

EventCore's file store assigns each **writing working copy** a `replica_id` and
records it in every transaction header. The identity is **machine-local,
gitignored, and never committed.** It is generated lazily on the first write
that needs it and bound to a working-copy fingerprint so that copying a working
tree does not duplicate a live identity.

### Two records, deliberately separate

The design keeps the two notions of "replica identity" physically and
semantically apart:

- **In the transaction header (committed, immutable):** every transaction's
  header carries the `replica_id` of the working copy that wrote it (ADR-0038's
  reserved header field). This is the historical attribution the linearizer and
  fork detector consume. It is part of `events/`, committed to git, and never
  edited.

- **In `.eventcore/replica_id` (machine-local, mutable, gitignored):** this file
  holds **this clone's current identity for new writes**. It is in the
  gitignore set (ADR-0046), never committed, and may be regenerated. It governs
  only future writes; it has no authority over the immutable attribution already
  recorded in past transactions.

These are two different things. The header answers "who wrote this transaction?"
(a permanent fact). The `.eventcore/replica_id` file answers "who am I when I
write next?" (a local, current property of this working copy).

### Lazy, on-write generation

`replica_id` is **not** generated when a store is opened or when it is read.
It is generated only on the first `append_events` that needs to stamp a header,
and only if `.eventcore/replica_id` is absent or no longer valid for this
environment (see fingerprinting). When generated, it is a fresh `Uuid` written
to `.eventcore/replica_id`. A read-only consumer of a fresh clone never creates
one; a clone that only reads history never acquires an identity at all.

### The copy trap and its mitigations

If `replica_id` were committed, `git clone` or `cp -r` would duplicate it, two
working copies would silently share identity, divergent forks would become
invisible, and the store would record a corrupt-but-linear-looking history —
the worst failure class. Three layered mitigations prevent this:

1. **Never commit it.** `.eventcore/replica_id` is in the gitignore set
   (ADR-0046). A `git clone` produces a working copy with no identity file; it
   generates its own on first write. This is the primary defense.

2. **Bind it to a working-copy fingerprint.** The identity file records, alongside
   the id, a fingerprint of _this_ working copy: an OS-level machine identifier,
   the repository's absolute path, and the `.git` directory inode. On each write,
   the store recomputes the fingerprint and compares it to the recorded one. If
   they no longer match — the classic `cp -r` to a new path, a move, a restored
   backup — the store treats the recorded id as belonging to a _different_
   working copy and regenerates a fresh `replica_id` for this one. A naive
   `cp -r` that bypasses gitignore therefore yields a _different_ id on its next
   write, which is the safe outcome (a detectable fork rather than silent
   sharing).

3. **Reconcile-time collision check.** As a final backstop, when two concurrent
   transactions in the merged DAG carry the _same_ `replica_id` but their parent
   sets are inconsistent with a single linear writer (a single writer's
   transactions form a chain; these do not), the store does not guess and does
   not silently merge. It fails loud with `ReplicaIdentityConflict`, naming the
   colliding transactions, so the operator can investigate rather than absorb a
   corrupted history.

### Explicit override

An operator may set the `replica_id` explicitly via store configuration
(`FsConfig`). When provided, the configured value is used verbatim and the
lazy-generation / fingerprint path is bypassed for write stamping. This serves
environments where the filesystem-based identity is unreliable or where replicas
are provisioned deliberately (containers, CI, orchestrated multi-replica
deployments). It is the operator's responsibility to ensure configured ids are
genuinely distinct across independent writers; the reconcile-time collision
check still applies as a backstop.

### Inert in single-writer mode

None of the above — fingerprint binding, regeneration, collision detection — is
required or exercised in single-writer mode. A single writer generates one
`replica_id` on its first write, every transaction it writes forms a linear
chain under that id, the linearizer's tiebreak never has to break a tie between
concurrent transactions, and no collision can arise because there is only one
writer. The contract suite (ADR-0040) passes unchanged: `replica_id` is present
in every header but behaviorally inert.

## Consequences

### Positive

- **Silent corruption is structurally prevented.** Because identity is never
  committed and is fingerprint-bound, the only ways two independent writers can
  share an id are deliberate misconfiguration — and even then the reconcile-time
  collision check converts the failure into a loud `ReplicaIdentityConflict`
  rather than an invisible fork. The worst failure class has three independent
  defenses.
- **Deterministic convergence holds.** Recording an immutable `replica_id` per
  transaction gives the linearization tiebreak `(created_at, replica_id,
transaction_id)` a stable, recorded input, so every clone with the same file
  set computes byte-identical state (ADR-0039).
- **Clear separation of "who wrote this" from "who am I now."** The committed
  header attribution and the gitignored current-identity file cannot be confused
  or accidentally cross-wired, which is precisely the confusion that would
  otherwise create the copy trap.
- **Zero cost in single-writer mode.** Lazy on-write generation means a
  read-only or never-merged store pays nothing; the contract suite is unaffected;
  the on-disk format never changes (the field was reserved in ADR-0038).
- **Operator escape hatch.** Explicit override supports container/CI/provisioned
  topologies without weakening the default safe behavior.

### Negative

- **Fingerprint heuristics are imperfect.** The OS-identifier / absolute-path /
  `.git`-inode fingerprint can mis-fire: a working copy moved in place and moved
  back, or a path reused after deletion, may regenerate an id unnecessarily.
  Spurious regeneration is the _safe_ direction (a detectable fork, never silent
  sharing), but it can produce avoidable reconciliation work.
- **A new failure mode exists.** `ReplicaIdentityConflict` is a loud error an
  operator must understand and act on. It is by design preferable to silent
  corruption, but it adds an error path consumers of merge mode must handle.
- **Local identity is not portable.** Backing up or relocating a working copy
  does not carry its identity (by design). Operators who want a stable identity
  across moves must use the explicit override.
- **More moving parts than a committed id.** A committed-and-shared identity
  would be simpler to reason about in isolation — at the cost of being
  catastrophically wrong. The safety the design buys is worth the added
  machinery, but the machinery is real.

## Alternatives Considered

### Alternative 1: Commit `replica_id` to the repository

Store the working copy's identity in a tracked file so it travels with the
history and every reader sees the same id for the same clone.

**Rejected because** this _is_ the copy trap. `git clone` and `cp -r` duplicate
the committed bytes, so two independent working copies inherit the same identity.
Divergent forks they produce share an id, fork detection cannot distinguish them
from a single linear writer, and the store records a corrupt history that looks
linear. This is the single most dangerous outcome the feature can produce —
silent data corruption rather than a crash or a surfaced conflict — and the
reason identity must be machine-local and gitignored.

### Alternative 2: Derive identity purely from machine/hostname

Use a hostname, MAC address, or OS machine-id as the `replica_id` directly,
with no per-working-copy file.

**Rejected because** a single machine routinely holds multiple working copies of
the same repository (two clones, a worktree, a CI checkout alongside a developer
checkout). They would all derive the same id and silently share identity — the
copy trap again, by a different route. The machine identifier is therefore only
_one component_ of the fingerprint that decides whether to regenerate, never the
identity itself.

### Alternative 3: Derive identity from the absolute repository path alone

Hash the working copy's absolute path into a stable id, regenerating implicitly
whenever the path changes.

**Rejected because** paths are neither unique across machines (two machines can
mount the repo at the same path) nor stable for a given working copy (containers
remount, bind-mounts relocate, CI uses fixed checkout paths shared across runs).
Path is a useful _fingerprint component_ for detecting a `cp -r`, but as the sole
identity it both collides across machines and churns within one. A fresh random
`Uuid`, bound to but not derived from the fingerprint, avoids both problems.

### Alternative 4: Generate identity eagerly at `open()`

Create `.eventcore/replica_id` whenever a store is opened, regardless of whether
it will ever write.

**Rejected because** it allocates identity for read-only consumers that will
never write a transaction (a projection runner reading a fresh clone, a tool
inspecting history), it touches the filesystem on every open, and it provides no
benefit over lazy generation — the id is only ever stamped at write time.
Lazy-on-write keeps single-writer and read-only paths inert, consistent with the
"reserve fields, activate nothing" discipline of ADR-0038.

### Alternative 5: Trust identity blindly and skip the reconcile-time check

Assume `replica_id` is always correctly distinct and omit the collision check at
reconcile time, simplifying the merge path.

**Rejected because** defense in depth is cheap here and the failure it guards
against is catastrophic. Fingerprint binding makes accidental sharing unlikely
but not impossible (a sufficiently adversarial `cp -r` that copies the
gitignored file, a botched explicit override). The collision check is the only
mechanism that converts a slipped-through duplicate into a loud
`ReplicaIdentityConflict` instead of a silently merged corrupt history. Removing
it trades a small simplification for the reintroduction of the worst failure
class.

## Related Decisions

- ADR-0038: File-Based Event Store Format and Atomicity — reserves the
  `replica_id` header field in the immutable format so this decision requires no
  format change.
- ADR-0041: Merge Causality via Transaction DAG — consumes `replica_id` for fork
  attribution and is where the reconcile-time `ReplicaIdentityConflict` check
  lives.
- ADR-0046: Git Integration Contract for the File Store — defines the gitignore
  set that keeps `.eventcore/replica_id` out of the committed history, the
  primary copy-trap mitigation.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection — uses the
  recorded `replica_id` as the middle component of the deterministic tiebreak
  `(created_at, replica_id, transaction_id)`.
- ADR-005: Event Metadata Structure — establishes UUIDv7 identity/ordering
  conventions for `event_id`/`transaction_id`, distinct from the writer-level
  `replica_id` introduced here.
- ADR-010: Free-Function API and Explicit Dependencies — motivates the explicit
  `FsConfig` override rather than implicit environment magic.
