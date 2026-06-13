# ADR-0045: Merge Mode Outside the EventStore Trait

## Status

Accepted

## Date

2026-06-12

## Context

The `eventcore-fs` backend (Layer 2, "merge mode") introduces operations that no other backend can support: detecting forks across a git-merged set of immutable transaction files, deterministically linearizing a transaction DAG, and reconciling divergent histories through a domain-owned resolver. The design question this ADR settles is **where those operations live in the API surface** — specifically, whether they belong on the cross-backend `EventStore` / `EventReader` traits or on the `eventcore-fs` backend type itself.

**The shared traits are a cross-backend contract.**

`EventStore` and `EventReader` (in `eventcore-types`) are the primary abstraction boundary between EventCore and its storage backends. Today they are implemented by `eventcore-postgres`, `eventcore-sqlite`, and `eventcore-memory`, and they are the contract that ADR-002 established and ADR-013 verifies via the reusable contract test suite. Anything added to these traits becomes a requirement that **every** backend must satisfy.

**Merge mode is structurally impossible for the relational and in-memory backends.**

The Postgres, SQLite, and in-memory stores each maintain a single authoritative linear log _by construction_:

- A row's `stream_version` is assigned at write time inside an ACID transaction (or under a mutex), against the one canonical head. There is no second writer producing a divergent claim on the same version.
- The global order is the insertion order in a single table (or vector). There is no DAG of transactions, no concurrent branches, and therefore nothing to topologically sort or linearize.
- A "fork" — two transactions sharing `stream_bases[s]` where neither is an ancestor of the other (ADR-0041) — cannot arise, because these backends never accept a write that builds on a stale, non-head base. They reject it as a `VersionConflict` (ADR-007) at write time.

Forks are a property of the file store's offline-collaboration model, where two clones append independently and a later `git merge` produces a pure additive union of immutable files (ADR-0038). That union, and the read-time linearization that resolves it (ADR-0039), are what make forks representable at all. The other backends have no analog and no way to manufacture one.

**Putting merge operations on the shared trait would force meaningless or panicking implementations.**

If `detect_forks()`, `reconcile()`, and `status()` were trait methods, the three relational/in-memory backends would each need an implementation. Their only honest options would be:

1. **Panic** (`unimplemented!()` / `todo!()`) — a direct violation of `no-panics-in-production` and a runtime trap for any consumer who calls the method through a `dyn EventStore`.
2. **Return a degenerate "no forks ever" stub** — dead code that exists only to satisfy the trait, carrying no behavior, untestable in any meaningful way, and misleading to readers who expect the method to mean something.
3. **Return a "not supported" error variant** — which would force the shared `EventStoreError` to grow a merge-specific variant that is nonsensical for the backends that own the only authoritative log, leaking a file-store concept into the cross-backend error vocabulary.

Each option violates a project invariant. Panicking breaks no-panics-in-production. Stub implementations break `no-dead-code-workarounds`. A shared error variant breaks **infrastructure neutrality** (design-principles): the library must not impose one backend's domain model on the abstraction boundary. The trait would also become harder to implement for _future_ backends, which would all inherit a method they cannot meaningfully honor.

**There is one subtlety: the `EventReader` cursor is genuinely shared surface.**

`EventReader::read_events` already exposes ordering through `EventFilter`'s `after_position` cursor, where `StreamPosition` is a `nutype(Uuid)` (UUID7). The file store reinterprets that ordering as _linearization order_ rather than raw UUID7 order. This reinterpretation must not break the shared contract. It does not: in single-writer mode the transaction DAG is a chain with no forks, so the computed linearization equals append order equals UUID7 order (ADR-0039). The existing `EventReader` contract tests — exclusive `after_position` filtering, prefix filtering, type filtering before `take(limit)`, global ordering — therefore pass unchanged. The merge-specific cursor concern (a per-replica local-ingestion-order cursor that prevents a live projection from rewinding when a merge inserts causally-earlier events, ADR-0043) is an **fs-specific extension**, not a change to the shared `after_position` semantics. The shared cursor keeps its contract; the local-ingestion cursor is additional fs surface.

**Why this decision now.**

Layer 1's immutable on-disk format reserves Layer 2's merge header fields from day one (ADR-0038), and the single read-time-linearization path is built in Layer 1 (ADR-0039). Before Layer 2 adds any merge operations, we must decide their home so the file backend can pass the ADR-013 contract suite **unchanged** in single-writer mode — the firewall that proves merge mode is purely additive and the shared traits are untouched.

## Decision

Merge-mode operations are **not** added to the cross-backend `EventStore` / `EventReader` traits. They are exposed as **`eventcore-fs`-specific API** — free functions and/or methods on the file backend type — consistent with ADR-010's free-function philosophy of explicit dependencies over trait-method proliferation.

**The off-trait merge surface.**

The file store exposes (as fs-specific API, not trait methods):

```rust
// Inspect the merged file set for divergent histories.
pub fn detect_forks(&self) -> Vec<Fork>;

// Reconcile detected forks using a domain-owned resolver (ADR-0042);
// the resolver returns a command whose handle() produces the merge events,
// and the library records an N-parent merge transaction.
pub fn reconcile<R: ForkResolver>(&self, resolver: R) -> ReconcileReport;

// Report current store health: outstanding forks, dangling transactions,
// in-conflict streams (ADR-0046).
pub fn status(&self) -> StoreStatus;
```

These names and shapes are illustrative of the surface, not a frozen signature; the point is that they live on the `eventcore-fs` type, take their dependencies explicitly, and are reachable **only** when a consumer has chosen the file backend.

**The shared traits remain exactly as they are.**

`EventStore`, `EventReader`, `CheckpointStore`, and `ProjectorCoordinator` gain no merge-aware methods. The file backend implements them with the same semantics as `eventcore-memory` and `eventcore-sqlite`, computing canonical `StreamVersion` and global order via read-time linearization that degenerates to contiguous append order in single-writer mode (ADR-0039). `execute()` (ADR-010) drives normal appends through the shared `EventStore` path with no awareness of merge mode.

**The firewall is the ADR-013 contract suite.**

The guarantee that normal mode is untouched is **not** an assertion in this ADR — it is enforced mechanically. The file backend MUST pass the existing 19-test `eventcore-testing` contract suite **unchanged** in single-writer mode, exactly as the other backends do. Because that suite exercises only the shared trait surface (append/version-conflict/atomicity/ordering/cursor/checkpoint/coordinator), passing it proves that:

- The shared trait behavior is byte-for-byte the cross-backend contract, with no merge concepts leaking in.
- Merge mode is **purely additive** — it adds new fs-specific entry points without altering any shared-trait method.
- A consumer using the file store as a plain `EventStore` (no merging) sees identical behavior to using Postgres, SQLite, or memory.

The workspace-wide requirement that `cargo nextest run --workspace` stays green — with the memory/sqlite/postgres contract suites unaffected — is the standing proof of this property.

## Consequences

### Positive

- **No backend is forced to implement a method it cannot honor.** Postgres, SQLite, and in-memory keep their single-authoritative-log model with no merge stubs, no panics, and no nonsensical "no forks" implementations.
- **Infrastructure neutrality is preserved.** The shared abstraction boundary stays free of a single backend's domain model; the cross-backend `EventStoreError` vocabulary gains no merge-specific variant.
- **No-panics and no-dead-code invariants hold.** There is no `unimplemented!()` on any backend and no dead stub that exists only to satisfy a trait.
- **The contract suite mechanically proves additivity.** "Merge mode does not touch normal mode" is verified by an unchanged 19-test pass, not merely asserted in prose. Regressions surface as contract-test failures.
- **Future backends are unburdened.** Anyone implementing a new `EventStore` inherits no merge obligation. The trait stays as small as ADR-002 intended.
- **Consistent with the free-function API direction (ADR-010).** Merge operations take their dependencies explicitly and live where they make sense — on the backend that can actually perform them — rather than being smeared across a shared trait via dynamic dispatch.
- **The shared cursor contract is genuinely shared.** Reinterpreting `after_position` ordering as linearization order is contract-compatible in single-writer mode, so no cross-backend change is needed; the merge-specific local-ingestion cursor is cleanly isolated as fs surface.

### Negative

- **Merge operations are not reachable through `dyn EventStore`.** A consumer holding a generic `Box<dyn EventStore>` cannot call `detect_forks()` / `reconcile()` / `status()` — they must name the concrete `eventcore-fs` type. This is the intended cost: those operations are meaningful only for the file store, so binding them to the concrete type is correct, not a limitation.
- **Application code that uses merge mode is coupled to `eventcore-fs`.** Swapping the file store for another backend means losing merge capability entirely (which is unavoidable — no other backend has the semantics) and removing the merge call sites. The shared-trait portion of the application remains backend-agnostic.
- **Two surfaces to document.** The file backend has both a shared-trait surface (normal mode) and an fs-specific surface (merge mode). Documentation must clearly delineate which operations are portable across backends and which are file-store-only.
- **The "purely additive" guarantee depends on discipline.** It holds only as long as the file backend keeps passing the unchanged contract suite. If a future change to merge mode altered shared-trait behavior, the suite would catch it — but the discipline of running it (CI gate) is what enforces the firewall.

## Alternatives Considered

### Alternative 1: Add merge methods to the `EventStore` trait

Make `detect_forks()`, `reconcile()`, and `status()` required methods on the shared `EventStore` trait, implemented by every backend.

**Rejected Because:**

- Postgres/SQLite/in-memory have a single authoritative linear log by construction and cannot produce or detect forks. Their implementations could only panic, return a meaningless "no forks ever" stub, or return a "not supported" error.
- Panicking violates `no-panics-in-production` and traps consumers calling through `dyn EventStore`.
- Stub implementations violate `no-dead-code-workarounds`: code that exists only to satisfy the trait, with no behavior and no meaningful test.
- A shared "not supported" error variant would force a file-store concept into the cross-backend `EventStoreError`, violating infrastructure neutrality (design-principles) and burdening every present and future backend with a method it cannot honor.

### Alternative 2: Add merge methods with default trait implementations

Give the merge methods default implementations on the trait (e.g., `detect_forks()` defaults to returning an empty `Vec`), so non-fs backends inherit a no-op.

**Rejected Because:**

- A default "no forks ever" is exactly the misleading dead-code stub from Alternative 1, just relocated to the trait definition. It silently makes `detect_forks()` a no-op for backends where the concept is undefined, hiding the fact that the operation is nonsensical there.
- It pollutes the shared trait with merge-specific types (`Fork`, `ReconcileReport`, `StoreStatus`, `ForkResolver`) that have no meaning for relational/in-memory backends, leaking the file store's domain model into the abstraction boundary.
- Default-method no-ops make it impossible to distinguish "this backend cannot fork" from "this backend has no forks right now," which is precisely the kind of ambiguity that causes subtle consumer bugs.

### Alternative 3: A separate `MergeableEventStore` trait that fs implements and the others do not

Define a second trait (e.g., `MergeableEventStore: EventStore`) carrying the merge methods, implemented only by `eventcore-fs`.

**Rejected Because:**

- It adds abstraction with no current second implementor. Merge mode is intrinsically tied to the file store's immutable-file-per-transaction, git-union model (ADR-0038); no other planned backend can satisfy it. A trait with exactly one implementor is ceremony, not abstraction.
- It runs against ADR-010's free-function direction, which prefers explicit free functions and concrete dependencies over speculative trait hierarchies introduced before a second implementor exists.
- If a future backend ever genuinely needs portable merge semantics, the trait can be extracted then, driven by the second implementor's actual requirements rather than guessed at now. Keeping the surface concrete keeps it honest.

### Alternative 4: Change `EventReader`'s cursor semantics to be merge-aware on the shared trait

Modify `after_position` / `StreamPosition` on the shared `EventReader` trait to carry merge/linearization-aware cursor semantics so projections behave correctly after a merge.

**Rejected Because:**

- The shared `after_position` semantics already work for the file store in single-writer mode: linearization order equals UUID7 order when the DAG is a fork-free chain, so the existing contract tests pass unchanged. There is nothing to fix on the shared trait.
- The genuinely merge-specific concern — preventing a live projection from rewinding when a merge inserts causally-earlier events — is handled by a per-replica local-ingestion-order cursor that is an fs-specific extension (ADR-0043), not a change to the cross-backend cursor contract.
- Pushing merge-aware cursor semantics onto the shared trait would force the relational/in-memory backends to reason about a linearization model they do not have, re-introducing the meaningless-implementation problem at the `EventReader` boundary.

## Related Decisions

- ADR-010: Free-Function API Design — establishes the free-function-with-explicit-dependencies philosophy that this ADR applies by placing merge operations on the fs backend rather than on a shared trait.
- ADR-013: EventStore Contract Testing Approach — the reusable contract suite that the file backend must pass unchanged in single-writer mode, serving as the mechanical firewall proving merge mode is purely additive.
- ADR-0039: Read-Time Linearization and StreamVersion as Projection — defines how the file store computes canonical order/version, degenerating to contiguous append order in single-writer mode so the shared `EventReader` cursor contract is honored.
- ADR-0043: Projection Behavior After Structural Merge — defines the per-replica local-ingestion-order cursor that is the fs-specific extension to projection behavior, kept off the shared `EventReader` surface.
- ADR-002: Event Store Trait Design — the original cross-backend trait boundary this ADR declines to widen.
- ADR-007: Optimistic Concurrency Control — the version-conflict mechanism by which relational/in-memory backends prevent forks from arising at all.
- ADR-0041: Merge Causality via Transaction DAG — defines what a "fork" is, the concept the shared traits' backends cannot represent.
- ADR-0042: Domain-Owned Reconciliation API — defines the `ForkResolver`-driven `reconcile()` operation exposed on the off-trait fs surface.
