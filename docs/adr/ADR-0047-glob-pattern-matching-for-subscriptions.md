# ADR-0047: Glob Pattern Matching for Subscriptions

## Status

Accepted

## Date

2026-06-13

## Context

ADR-017 reserved the POSIX glob metacharacters (`*`, `?`, `[`, `]`) in `StreamId`
and `StreamPrefix` precisely so that a future "distinct type" could carry glob
patterns without ambiguity or escaping complexity. That future is now: projection
consumers need to subscribe to events from a _family_ of streams identified by a
pattern (for example `account-*`, `order-2024-[0-9]*`) rather than a single literal
prefix.

`EventFilter` (in `eventcore-types`) already supports literal prefix filtering via
`StreamPrefix` and an optional `event_type` predicate. Both are pushed down to the
storage backend so that filtering happens _before_ the pagination `LIMIT` is
applied. That pushdown is not a performance nicety — it is a correctness
requirement. `read_events` is paginated: it returns at most `BatchSize` events per
call and the caller advances a cursor. If a filter were applied _after_ the
backend applied `LIMIT`, a page full of non-matching events would return zero
matches even though matching events exist further along, and the cursor would
still advance past them — the matches would be silently skipped. This is the same
reasoning that drove issue #372 (the `event_type` filter): the filter must run at
the query level so that non-matching events never consume batch slots.

Glob pattern filtering must follow the same rule, which means every backend has to
express the pattern in terms its query layer understands.

ADR-017 also specified that `StreamPrefix` would reject glob metacharacters so that
a literal prefix can never be confused with a pattern. That validation was never
actually wired up — `StreamPrefix` accepted metacharacters. This ADR closes that
gap as part of introducing the pattern type, so the literal/pattern type boundary
ADR-017 promised is finally enforced.

## Decision

### 1. A distinct `StreamPattern` domain type

Add `StreamPattern` to `eventcore-types` alongside `StreamPrefix`, defined with
nutype:

- `sanitize(trim)`
- `validate(not_empty, len_char_max = 255, predicate = is_valid_glob_pattern)`

The custom `is_valid_glob_pattern` predicate accepts a string only if it compiles
as a `glob::Pattern`. This is parse-don't-validate (ADR-003): an invalid pattern —
for example an unclosed character class `account-[` — can never be constructed, so
matching code never has to recover from a compile error. The predicate is covered
by property tests (literal strings always compile; common wildcards `*`, `?`,
`[0-9]`, `[a-z]*` compile; an unclosed `[` never compiles).

`StreamPattern` exposes a `matches(&self, stream_id: &str) -> bool` method that
compiles the glob and tests the whole stream id. Because construction guarantees
the pattern compiles, the theoretically-impossible compile failure inside `matches`
returns `false` rather than panicking (no-panics-in-production rule).

### 2. The `glob` crate, with `*` crossing `/`

Matching uses the [`glob`](https://crates.io/crates/glob) crate's
`Pattern::matches`. Its default behavior is `require_literal_separator = false`,
which means `*` and `?` match the path separator `/` like any other character.
EventCore treats `/` as an _ordinary_ character in stream ids — it carries no
hierarchical meaning to the store — so this is exactly the desired behavior:
`account-*` matches `account-1/sub` as readily as `account-1`. We deliberately rely
on this default rather than configuring `MatchOptions`.

Supported syntax:

- `*` — any sequence of characters (including `/`)
- `?` — exactly one character
- `[...]` / `[!...]` — one character from a set or range (e.g. `[0-9]`, `[a-z]`),
  with `[!` meaning negation

### 3. `EventFilter` gains a mutually-exclusive pattern field

`EventFilter` gets a `stream_pattern: Option<StreamPattern>` field, a
`EventFilter::pattern(StreamPattern)` constructor, and a
`stream_pattern(&self) -> Option<&StreamPattern>` accessor. A filter selects streams
by **either** a literal prefix **or** a glob pattern (the two are mutually exclusive
because each constructor sets only one), optionally narrowed by `event_type`.

### 4. Per-backend pushdown, applied before `LIMIT`

Each backend applies the pattern filter at the query level, exactly where it already
applies the prefix and event-type filters:

- **PostgreSQL** — pushdown via the POSIX-regex match operator `stream_id ~ $n`. The
  glob is translated to an **anchored** POSIX regex (`^...$`): `*` → `.*`, `?` →
  `.`, `[...]` kept as a regex character class (`[!` normalized to `[^`), and every
  other character treated as a literal with all regex metacharacters escaped. The
  escaping prevents regex injection: a literal `.` in a stream pattern matches a
  literal `.`, not "any character". The four-branch hand-written query is replaced
  by a dynamic `sqlx::QueryBuilder` so prefix XOR pattern + cursor + event_type
  compose without combinatorial query strings.
- **SQLite** — pushdown via the native `GLOB` operator (`stream_id GLOB ?`), which
  implements POSIX glob semantics directly, so no translation is needed. The
  match-based SQL builder is refactored into a dynamic WHERE-clause builder for the
  same compositional reason.
- **In-memory** (`eventcore-memory`) and **file store** (`eventcore-fs`) — in-process
  filtering via `StreamPattern::matches`, placed in the filter chain before
  `.take(limit)` so non-matching streams never consume batch slots.

Cross-backend behavior is verified by new contract tests in `eventcore-testing`
(`*` wildcard, `?` single-char, `[0-9]` character class), wired into
`backend_contract_tests!` so every backend runs them. The `*`-wildcard test
deliberately appends more non-matching events than the page limit before the
matching events, proving the filter runs before `LIMIT`.

## Consequences

### Positive

- Projection consumers can subscribe to stream families with familiar glob syntax.
- Invalid patterns are impossible to construct; no runtime pattern-compile failures.
- Filtering is correct under pagination on every backend (matches are never skipped).
- The literal/pattern type boundary ADR-017 designed is now fully enforced:
  `StreamPrefix` rejects metacharacters, `StreamPattern` requires valid glob syntax.
- The Postgres and SQLite query builders are now dynamic, eliminating the
  branch-per-filter-combination query duplication.

### Negative

- `eventcore-types` gains a dependency on the `glob` crate.
- The Postgres glob→regex translation is hand-written and must stay faithful to glob
  semantics; it is covered by unit tests (translation output) and the cross-backend
  contract tests (observable behavior against a real database).
- `StreamPattern::matches` recompiles the glob on each call for the in-process
  backends. This is acceptable for the current poll-based projection workloads; if it
  becomes a hot path, the compiled pattern can be cached.

### Closing the ADR-017 gap

`StreamPrefix` now validates with `no_glob_metacharacters` (the predicate already
used by `StreamId`). Blast radius is minimal: the only `StreamPrefix` construction
sites in the workspace use the literal `"account-"`, which contains no
metacharacters, so no existing call is affected.

## References

- **ADR-003**: Type System Patterns for Domain Safety (parse-don't-validate)
- **ADR-017**: Reserved Characters for StreamId and StreamPrefix (reserved the
  metacharacters and specified a distinct `StreamPattern` type for the future)
- **ADR-016**: Event Subscription Model
- Issue #372: event_type filter must be applied before the pagination limit
  (the same pushdown-for-correctness reasoning)
- Issue #246: Glob Pattern Matching for Subscriptions (this work)
- POSIX Glob Pattern Syntax: IEEE Std 1003.1-2017
