# Keep Source Files Focused and Modular

Source files should hold a single, coherent responsibility. When a file
accumulates several distinct responsibilities — or simply grows large — split it
into modules organized by responsibility, not by arbitrary line cuts.

## The Rule

- A source file maps to one responsibility: one subsystem, one layer, or one
  cohesive set of types and the functions that operate on them.
- When a file grows past a few hundred lines, or starts mixing unrelated
  concerns, split it into modules **before** moving on. Do not let a single
  file accumulate the whole crate.
- Split by **responsibility**, never by arbitrary size alone. Each module gets a
  name that describes what it owns.

## How to Split (Rust crates)

Prefer a flat set of sibling modules under `src/`, each owning one concern, with
`lib.rs` as a thin facade that declares the modules, re-exports the public API,
and holds the primary type plus its trait implementations. For a
backend/adapter crate a typical split is:

- `error.rs` — the crate's error types
- `config.rs` — configuration and resolved layout
- `format.rs` — on-disk / wire format and (de)serialization
- `index.rs` / `<domain>.rs` — the core data model and its algorithms
- `coordination.rs` — locking, concurrency, external coordination
- `lib.rs` — the public facade type, trait implementations, and `pub use`
  re-exports

Keep cross-module data types `pub(crate)` with public accessor methods, and
expose only the intended public API via `pub use` from `lib.rs`. Unit tests live
in a `#[cfg(test)] mod tests` beside the code they exercise, so splitting a file
moves its pure-function tests into the new module.

## Why

A single monolithic file forces every reader — human or agent — to load and
scan the whole thing to find one concern, and makes diffs and reviews noisier.
Responsibility-named modules let a reader (or an agentic harness) open exactly
the file that owns the concern they care about. The cost of the split is a few
`pub(crate)` annotations and `mod` / `use` lines; the payoff is every subsequent
read, edit, and review. Build modular from the start rather than growing a file
until a reviewer asks for the split.
