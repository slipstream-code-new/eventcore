# EventCore Agent Cheatsheet

## Project Configuration

- **Language:** Rust (2024 edition)
- **Test runner:** `cargo nextest run --workspace`
- **Build:** `cargo build --workspace`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Format:** `cargo fmt --all`
- **Mutation testing:** `cargo mutants` (zero surviving mutants required)
- **Architecture docs:** `docs/manual/01-introduction/04-architecture.md`
- **ADRs:** `docs/adr/`

## Development Rules

1. Enter `nix develop` for pinned toolchains; start Postgres via `docker-compose up -d` only when running postgres backend tests.
2. Format every change with `cargo fmt --all` before attempting a commit or PR.
3. Run `cargo clippy --all-targets --all-features -- -D warnings` to satisfy the lint gate.
4. Execute the full test suite with `cargo nextest run --workspace` (fallback: `cargo test --workspace`).
5. Target a single test via `cargo nextest run --workspace -E 'test(module::case)'` or `cargo test module::case`.
6. Target a single integration test file via `cargo nextest run --test feature_name_test`.
7. Use Rust 2024 edition conventions: 4-space indent, trailing commas, and prefer early returns over nested branching.
8. Naming: snake_case modules/functions, PascalCase types/traits/enums, SCREAMING_SNAKE_CASE for consts/macros, descriptive async test names.
9. Import order: std -> external crates -> internal (prefixed with `crate::`); consolidate re-exports through `lib.rs`.
10. Types: lean on `nutype` for domain primitives, derive `Debug`, `Clone`, `serde`, and `thiserror`; reach for associated types ahead of generics.
11. Errors: use `thiserror` enums, return `Result<T, CommandError>` from command logic, propagate via `?`, and document failure cases.
12. Domain structs should validate invariants in constructors, own their data, and avoid lifetimes when cloning is cheap.
13. Unit tests live beside source in `#[cfg(test)] mod tests`; integration tests live in each crate's `tests/` directory, organized by feature.
14. Integration tests must read like docs — Given/When/Then comments, only public APIs, no private hooks or mocks of internals.
15. Duplication inside tests is acceptable when it mirrors how downstream users compose commands and stores.
16. Prefer existing tracing/logging helpers over ad-hoc `println!` debugging noise.
17. All work items are tracked in **Forgejo Issues** at `git.johnwilger.com/Slipstream/eventcore`; use the `tea` CLI or direct REST calls for automation.
18. Keep pre-commit hooks green: rerun fmt/clippy/nextest locally until clean before committing.
19. Use Conventional Commits for all git commit messages and PR titles (type/scope: summary) so history stays machine-readable.

## Issue Tracking with Forgejo Issues

**IMPORTANT**: This project uses **Forgejo Issues** for ALL issue tracking,
hosted at `git.johnwilger.com/Slipstream/eventcore`.

### Labels

**Priority labels:**

- `P0-critical` - Security, data loss, broken builds
- `P1-high` - Major features, important bugs
- `P2-medium` - Default priority
- `P3-low` - Polish, optimization
- `P4-backlog` - Future ideas

**Type labels:**

- `bug` - Something broken
- `enhancement` - New feature or request
- `task` - Work item (refactoring, tests, tooling)
- `epic` - Large feature with sub-issues
- `chore` - Maintenance (audits, cleanup)
- `research` - Investigation / spike
- `documentation` - Docs improvements

### Quick Reference

The `tea` CLI (Forgejo's official client) is the recommended interface. It
must be configured once with a Forgejo PAT via `tea login add`. Direct
`curl` calls to `/api/v1/repos/Slipstream/eventcore/...` are equivalent and
work without `tea` installed. Use the canonical `Slipstream/eventcore` path:
`jwilger/eventcore` 301-redirects, and a POST following that redirect becomes
a GET, so write operations silently fail.

**Check for work:**

```bash
tea issues list --labels "P1-high"     # High priority issues
tea issues list --assignee @me         # Your assigned issues
tea issues list --state open           # All open issues
```

**Create issues:**

```bash
tea issues create --title "Issue title" --labels "enhancement,P2-medium"
```

**Claim and update:**

```bash
tea issues edit 42 --assignees @me
tea comment 42 "Starting work on this"
```

**Complete work:**

```bash
tea issues close 42
tea comment 42 "Completed in #PR_NUMBER"
```

## Git Workflow

This project uses standard feature branches with squash merges.

### Branch Workflow

1. Create a feature branch: `git checkout -b type/description`
2. Make commits using Conventional Commits
3. Push and create a PR: `git push -u origin <branch>` then `tea pr create`
   (or open the PR via the Forgejo web UI link printed by `git push`)
4. PRs are squash-merged into `main`
