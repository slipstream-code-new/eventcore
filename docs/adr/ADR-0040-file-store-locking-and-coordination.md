# ADR-0040: File-Store Locking and Projector Coordination

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend (ADR-0038, ADR-0039) persists each `append_events`
transaction as a single immutable JSONL file under `<root>/events/`, with all
derived state (the rebuildable index, checkpoints, coordinator locks, replica
identity) living in gitignored sibling directories. The backend must satisfy
the same 19-test contract suite (ADR-013) that `eventcore-memory`,
`eventcore-sqlite`, and `eventcore-postgres` satisfy — including optimistic
concurrency (ADR-007), multi-stream atomicity (ADR-001), and the projector
coordination contract.

Unlike a database backend, a file store has no transaction manager, no
connection pool, and no server process mediating concurrent access. The
correctness guarantees that PostgreSQL gets "for free" from ACID transactions
and `pg_try_advisory_lock` must be reconstructed from filesystem primitives.
Three distinct concurrency hazards arise, each at a different scope, and each
demands its own mechanism:

1. **Concurrent appends within one process.** EventCore's `execute()` retries on
   `VersionConflict` (ADR-008), and applications may drive many commands against
   a single `FileEventStore` instance from many tokio tasks. The atomic-write
   path (ADR-0038) reads the current per-stream head from the in-memory index,
   validates `expected_versions()`, assigns contiguous versions, then writes a
   file and mutates the index. If two appends interleave between the version
   check and the index mutation, both could observe the same head and both could
   write "version 6" for the same stream — a lost-update / duplicate-version bug
   that the contract suite's `concurrent_version_conflicts` and
   `conflict_preserves_atomicity` tests are designed to catch. The check and the
   write must be one indivisible critical section, and conflict detection must be
   **deterministic**: of two racing appends with the same `expected_version`,
   exactly one succeeds and the other receives `VersionConflict`.

2. **Concurrent opens across processes (or two opens of one root in one
   process).** Two `FileEventStore` instances rooted at the same directory each
   hold their own `Arc<RwLock<Index>>` and their own in-process append mutex.
   Neither knows about the other's appends. Two processes interleaving writes
   would corrupt the version sequence and the index's view of the DAG, producing
   files that disagree about stream heads. Because the on-disk format is the
   single source of truth and the index is a per-instance cache, two writers on
   one root is never safe in single-writer mode. (Genuine multi-writer
   collaboration is the explicit, off-trait concern of merge mode — ADR-0041
   through ADR-0046 — reached through `git merge`, never through two live
   processes sharing a working tree.) The store must refuse the second opener
   loudly rather than silently corrupt state.

3. **Concurrent projector leadership across instances.** EventCore's
   `ProjectorCoordinator::try_acquire` (ADR-023) elects a single leader per
   subscription so that exactly one projection runner advances a checkpoint at a
   time. ADR-028 settled the acquisition semantics for the PostgreSQL backend:
   **non-blocking try-acquire** (`pg_try_advisory_lock`), returning immediately
   with failure when another instance holds leadership, never blocking and never
   retrying inside the library. The contract suite encodes this as behavior:
   a second `try_acquire` for the same subscription must be refused while the
   first guard lives, and dropping the first guard must release leadership so a
   subsequent `try_acquire` succeeds. The file store must reproduce that exact
   behavior using filesystem primitives, including the non-obvious requirement
   that a _second acquisition attempt within the same process_ is also blocked.

A further wrinkle threads through layers 2 and 3: EventCore subscription names
contain reserved characters — the `::` path separator used in Rust module paths
(ADR-017 reserves characters in `StreamId`, and subscription names follow the
same convention). A subscription named `accounts::balance_projection` cannot be
used verbatim as a filename on every target filesystem. Both the checkpoint
files (ADR-0038) and the coordinator lock files must derive safe filenames from
these names without collisions.

The decision must also keep durability and locking concerns cleanly separated.
Durability (the atomic tmp-write + fsync + rename of ADR-0038) is achievable
with the standard library alone (`std::fs::File::sync_all`, `std::fs::rename`).
Advisory file locking, however, is not in `std` — it requires `flock`/`LockFileEx`
syscalls. We must decide whether to pull in a dependency for locking, and which
one, without letting it bleed into the durability path.

## Decision

The file store uses **three lock layers**, each scoped to exactly one hazard
above. Durability uses `std` only; an advisory-locking crate (`fs4`) is
introduced solely for layers 2 and 3.

### Layer 1 — In-process append serialization (`tokio::sync::Mutex`)

`FileEventStore` holds a `tokio::sync::Mutex` that is acquired at the start of
`append_events` and **held across the entire append** — the version check, file
write, fsync/rename, and index mutation all occur inside one critical section.
This linearizes all concurrent appends issued against a single store instance,
making the atomic-write path of ADR-0038 indivisible.

Because the mutex serializes the _whole_ operation rather than just the index
mutation, version-conflict detection is deterministic: when two tasks race with
the same `expected_version`, the first to acquire the mutex reads the head,
validates, writes its file, and advances the in-memory head before releasing;
the second then reads the now-advanced head, finds `expected != actual`, and
returns `VersionConflict { stream_id, expected, actual }` **before writing any
file**. This satisfies `concurrent_version_conflicts` (exactly one writer wins)
and `conflict_preserves_atomicity` (the loser writes nothing). A
`tokio::sync::Mutex` (not `std::sync::Mutex`) is used because the critical
section performs `.await` points (filesystem I/O via `spawn_blocking` or async
file ops), and holding a std mutex across an await is unsound.

This layer is purely intra-process; it provides no protection against a second
OS process. That is layer 2's job.

### Layer 2 — Cross-process store lock (`<root>/.lock`)

`FileEventStore::open()` (and `open_with_config`) acquires an **exclusive
advisory lock** on `<root>/.lock` and holds it for the entire lifetime of the
store instance. The lock file lives at the store root (it is gitignored — only
`events/` is committed, per ADR-0046). Acquisition is non-blocking: if the lock
is already held — by another process, or by a second `open()` of the same root
within the same process — `open()` returns `FsEventStoreError::StoreLocked`
rather than proceeding.

This makes "one live writer per root" a structural invariant. A second writer
can never observe a stale index or interleave its version assignments with the
first, because it never gets a store handle at all. The guarantee is essential:
the in-memory index is a per-instance CQRS read model (ADR-0039), and two
instances mutating `events/` independently would each hold a divergent,
silently-wrong view of stream heads. `StoreLocked` converts that corruption
risk into an immediate, legible failure at `open()` time.

The store lock is released when the `FileEventStore` is dropped (the held
`File`'s lock is released by the OS on close), so a process that finishes and
drops its store frees the root for the next opener.

### Layer 3 — Per-subscription projector leadership (`fs4` OS advisory locks)

`FileProjectorCoordinator::try_acquire(subscription)` attempts an **exclusive,
non-blocking** OS advisory lock on
`<root>/locks/<sanitized-subscription>.lock`, using the `fs4` crate's
`try_lock_exclusive`. This directly mirrors ADR-028's non-blocking contract:

- On success, it returns a leadership guard that **owns the open `File`**. While
  the guard lives, the `flock` is held.
- On contention (`WouldBlock`), it returns failure immediately. The library does
  **not** block and does **not** retry — the caller decides what to do, exactly
  as ADR-028 mandates for the PostgreSQL backend.
- On `Drop`, the guard releases the advisory lock (by closing the owned `File`)
  **and removes the lock file** from `<root>/locks/`, leaving the directory
  clean for the next leader.

A subtle but load-bearing property makes this satisfy the contract suite's
in-process tests: **`flock` advisory locks are associated with the open file
description, not the process.** A second `try_acquire` for the same
subscription — even from the same process, and even though the OS would let the
_same_ descriptor re-lock — opens a **fresh `File`** (a new file description) and
therefore a fresh, independent `flock` attempt, which the kernel correctly
refuses while the first guard's descriptor still holds the lock. This is why the
coordinator opens a new file handle per `try_acquire` rather than caching one:
it is precisely what makes the contract suite's _second-instance-blocked_ test
pass within a single process, and the _released-on-guard-drop_ test pass once the
first guard is dropped.

`fs4` is used **only** for these advisory locks (layers 2 and 3). Durability —
the tmp-write, `sync_all`, and `rename` of ADR-0038 — uses the standard library
exclusively. The two concerns do not share code paths.

### Filename sanitization

Subscription names contain `::` and potentially other characters that are not
portable filenames. Both the coordinator lock files (`<root>/locks/`) and the
checkpoint files (`<root>/checkpoints/`, ADR-0038) derive their filenames from
subscription names via a single shared, injective sanitization function. The
sanitization must be **collision-free**: two distinct subscription names must
never map to the same filename, or two unrelated subscriptions could falsely
contend for one lock (a silent leadership bug) or overwrite each other's
checkpoints (a silent progress-loss bug). The same function is reused by the
checkpoint store and the coordinator so the two never disagree.

## Consequences

### Positive

- **Deterministic conflict detection.** Holding the append mutex across the
  whole operation makes `VersionConflict` a function of acquisition order, not a
  race — exactly one of two racing appends wins, satisfying the contract suite
  without backend-specific timing assumptions.
- **Corruption is structurally impossible, not merely unlikely.** The
  `<root>/.lock` store lock turns "two writers on one root" from a silent
  data-corruption path into an immediate `StoreLocked` error at `open()`. The
  failure is loud and early, where it is cheapest to diagnose.
- **Faithful to ADR-028.** Layer 3 reproduces the established non-blocking,
  no-library-retry leadership contract using filesystem primitives, so the file
  store behaves identically to PostgreSQL coordination from the caller's
  perspective and passes the shared coordinator contract tests unchanged.
- **Correct same-process semantics for free.** Because `flock` is per-file-
  description and the coordinator opens a fresh `File` per attempt, the
  second-instance-blocked and released-on-drop behaviors hold even when both
  attempts come from one process — no extra bookkeeping required.
- **Self-cleaning lock files.** Guards remove their lock file on `Drop`, so
  `<root>/locks/` does not accumulate stale entries across leadership churn.
- **Clean separation of durability and locking.** `std`-only durability keeps
  the crash-safety path (ADR-0038) dependency-free and auditable; `fs4` is
  confined to advisory locks, so a locking-crate change can never affect write
  durability.
- **Consistent filename derivation.** A single shared sanitizer used by both
  checkpoints and coordinator locks eliminates the class of bugs where the two
  subsystems disagree about how a subscription name maps to a file.

### Negative

- **New dependency (`fs4`).** The crate adds `fs4` purely for advisory locks.
  This is an additional supply-chain surface, mitigated by confining it to two
  well-scoped call sites and keeping durability on `std`.
- **Append throughput is serialized per instance.** The single append mutex
  means appends against one store do not parallelize. For a file-based,
  developer-tooling backend this is acceptable (throughput is not the design
  goal — correctness and git-friendliness are, per the design's stated
  priorities), but it is a real ceiling that a database backend does not have.
- **Store lock does not protect against external file tampering.** The
  `<root>/.lock` guards against concurrent _EventCore_ writers; it does not stop
  a user editing `events/` by hand or a `git merge` rewriting files out from
  under a running store. Those are addressed separately by merge mode's
  read-time fsck and dangling-transaction handling (ADR-0046), not by this lock.
- **Advisory-lock portability caveats.** OS advisory locks behave differently
  across platforms and especially over network filesystems (NFS `flock`
  semantics are notoriously unreliable). `fs4` abstracts the common cases, but
  the store-lock and coordinator guarantees are only as strong as the underlying
  filesystem's advisory-lock implementation. This is documented as a constraint:
  the file store targets local filesystems.
- **Caller must handle `StoreLocked` and leadership failure.** Consistent with
  ADR-028, the library does not retry; applications opening a contended root, or
  attempting to acquire contended leadership, must handle the failure
  themselves.

## Alternatives Considered

### Alternative 1: No locking — rely on append order and UUID7 monotonicity

Skip all three layers and trust that appends are naturally ordered by their
UUIDv7 `transaction_id` and `event_id`, resolving conflicts at read time via the
linearizer (ADR-0039).

**Rejected because:** without the in-process mutex, two concurrent appends can
both pass the version check and both write "version 6" for one stream within a
single instance — a duplicate-version write that the contract suite explicitly
forbids and that read-time linearization cannot retroactively turn into a
`VersionConflict` (the conflicting command already committed its file).
Optimistic concurrency (ADR-007) requires that the _loser_ be rejected at write
time, not silently merged. And without the store lock, two processes corrupt the
index and version sequence outright. "No lock" produces exactly the corruption
the three layers exist to prevent.

### Alternative 2: PID-based lockfiles for the store and coordinator locks

Implement layers 2 and 3 by writing the holder's PID into a lockfile and treating
the file's existence as the lock, avoiding any locking crate.

**Rejected because:** PID lockfiles suffer the classic **stale-lock problem**. If
the holder process crashes or is killed (SIGKILL, OOM, power loss) without
cleaning up, the lockfile persists and every future opener must guess whether the
recorded PID is a live holder or a corpse. Liveness checks (`kill(pid, 0)`) are
racy (PIDs are reused) and non-portable. The result is either spurious
`StoreLocked` errors that require manual lockfile deletion, or unsafe
"steal the lock if the PID looks dead" heuristics that reintroduce the
two-writer corruption risk. OS advisory locks (`flock`) are released
**automatically** by the kernel when the holding file description is closed —
including on process death — eliminating stale locks entirely. That automatic
release is exactly the property the store lock and the leadership guard depend
on.

### Alternative 3: A pure-std advisory lock via `try_lock` on `std::fs::File`

Use only the standard library and hand-roll advisory locking, avoiding `fs4`.

**Rejected because:** the standard library does not expose `flock`/`LockFileEx`.
Hand-rolling it means writing platform-specific `unsafe` FFI for Unix `flock` and
Windows `LockFileEx`, plus the per-file-description and automatic-release
semantics the design relies on — reimplementing precisely what `fs4` already
provides, tested across platforms. The maintenance and correctness cost of
bespoke locking syscalls outweighs the cost of one well-scoped dependency.
Durability remains `std`-only because `std` _does_ provide everything atomic
writes need (`sync_all`, `rename`); locking is the one place `std` falls short,
so it is the one place we add a dependency.

### Alternative 4: A single `std::sync::Mutex` instead of `tokio::sync::Mutex` for appends

Use a synchronous mutex for layer 1.

**Rejected because:** the append critical section spans `.await` points (async
filesystem I/O), and holding a `std::sync::Mutex` guard across an await either
fails to compile (the guard is not `Send`) or, if forced, can deadlock the tokio
runtime by blocking a worker thread while the held task is suspended. A
`tokio::sync::Mutex` is await-aware and is the correct primitive for a critical
section containing I/O.

### Alternative 5: Blocking lock acquisition (wait for leadership / wait for the store lock)

Make `try_acquire` and `open()` block until the lock becomes available, giving
instant failover.

**Rejected because:** this contradicts ADR-028, which deliberately chose
non-blocking acquisition for projector coordination so the library makes no
assumptions about the deployment's restart orchestration and wastes no resources
on idle waiters. Reproducing blocking semantics in the file store would diverge
from the established cross-backend contract and the coordinator contract tests.
For the store lock specifically, blocking would mask the genuine error condition
("another writer owns this root") behind an indefinite hang; failing fast with
`StoreLocked` is the legible behavior.

## Related Decisions

- ADR-028: Advisory Lock Acquisition Behavior — establishes the non-blocking,
  no-library-retry `try_acquire` contract that layer 3 reproduces with `fs4`.
- ADR-013: EventStore Contract Testing Approach — defines the projector
  coordination tests (second-instance-blocked, released-on-guard-drop) that the
  three lock layers must satisfy unchanged in single-writer mode.
- ADR-0038: File-Based Event Store Format and Atomicity — defines the
  immutable-file format and the `std`-only atomic-write durability path that
  layer 1 serializes and that the store lock protects.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection — defines
  the per-instance index (the CQRS read model) whose integrity the cross-process
  store lock guarantees, and the deterministic version computation that the
  serialized append path feeds.
- ADR-001: Multi-Stream Atomicity — one transaction file is the all-or-nothing
  multi-stream write unit that layer 1's critical section makes indivisible.
- ADR-007: Optimistic Concurrency Control — the write-time `VersionConflict`
  guarantee that layer 1's serialization makes deterministic.
- ADR-008: Command Executor Retry Logic — the retry-on-conflict behavior that
  drives concurrent appends against one store instance, motivating layer 1.
- ADR-017: StreamId Reserved Characters — the reserved-character convention that
  motivates subscription-name sanitization for checkpoint and lock filenames.
- ADR-023: Projector Coordination — the `ProjectorCoordinator` leadership model
  that layer 3 implements for the file backend.
