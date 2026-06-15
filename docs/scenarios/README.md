# GWT Scenarios

This directory contains Given-When-Then scenarios organized by feature area.

## Structure

Organize scenarios by feature area. An example layout might look like:

```
scenarios/
├── commands/
│   ├── single-stream-execution.md
│   └── multi-stream-atomic.md
├── projections/
│   └── projection-runner.md
└── ...
```

(This is an illustrative layout; create subdirectories and files as your
scenarios grow.)

## Creating Scenarios

Scenarios are created during Phase 2 (Domain Discovery) of the development
workflow. Use the BDD skills (`bdd:bdd-scenarios`, `bdd:bdd-principles`) to
model the feature and write Given-When-Then scenarios. See the "Required
Development Workflow" section in `CLAUDE.md` for details.

## Scenario Format

```gherkin
Feature: [Feature Name]
  As a [role]
  I want [capability]
  So that [benefit]

  Scenario: [Name]
    Given [precondition]
    When [action]
    Then [outcome]
```

## Integration Tests

Scenarios in this directory often map to integration tests in:

- `eventcore/tests/<feature>_test.rs` (e.g. `single_stream_command_test.rs`,
  `multi_stream_atomic_test.rs`)
- `eventcore-postgres/tests/<feature>_test.rs` (e.g. `atomic_multi_stream_test.rs`,
  `concurrency_retry_test.rs`)
