//! Documentation accuracy guard.
//!
//! This test scans the user-facing prose documentation (the manual, the
//! architecture blueprints, and the published READMEs) and fails if any Rust
//! code block references an EventCore API that does not exist. It exists
//! because the manual's code blocks are illustrative snippets that are *not*
//! compiled by `cargo test --doc`, so fabricated types and methods can silently
//! creep back in (which is exactly what happened before the 1.0 documentation
//! overhaul).
//!
//! The guard is deliberately a denylist of known-fabricated symbols and a few
//! wrong-crate import paths rather than a full compiler: most manual blocks are
//! intentionally partial (axum handlers, application-owned structs) and cannot
//! compile standalone, so a blanket "compile everything" check would drown in
//! false positives. Self-contained, compile-checked examples live in the other
//! `eventcore-examples` integration tests and in crate rustdoc (`cargo test
//! --doc`); this guard covers the prose docs those cannot reach.
//!
//! When EventCore genuinely gains one of these names, remove it from the
//! relevant list below.

use std::path::{Path, PathBuf};

/// Bare type identifiers that do not exist anywhere in EventCore's public API.
/// Matched as whole words inside Rust code blocks.
const FABRICATED_TYPES: &[&str] = &[
    "EventToWrite",
    "EventMetadata",
    "EventVersion",
    "EventId",
    "StreamEvents",
    "WriteResult",
    "ReadOptions",
    "ExpectedVersion", // NB: ConflictingExpectedVersions / SetExpectedVersions are real and excluded by word-boundary matching
    "PoolConfig",
    "MigrationConfig",
    "MetricsConfig",
    "TracingConfig",
    "SamplingConfig",
    "LoggingConfig",
    "SecurityConfig",
    "EventCoreConfig",
    "ConfigBuilder",
    "TlsConfig",
    "AuthConfig",
    "EncryptionConfig",
    "CheckpointConfig",
    "SchemaRegistry",
    "EventSerializer",
    "TypeRegistry",
    "VersionedPayload",
];

/// Symbols that live in `eventcore_types`, not the `eventcore` facade. A doc
/// that writes `eventcore::EventStore` will not compile for a consumer.
const WRONG_CRATE_PATHS: &[&str] = &[
    "eventcore::EventStore",
    "eventcore::EventReader",
    "eventcore::CheckpointStore",
    "eventcore::ProjectorCoordinator",
    "eventcore::StreamVersion",
    "eventcore::MaxRetries",
    "eventcore::MaxRetryAttempts",
    "eventcore::StreamWrites",
];

/// Other fabricated call/path patterns. Matched as substrings (each is specific
/// enough that a substring match has no legitimate hits).
const FABRICATED_PATTERNS: &[&str] = &[
    "StreamId::from(",
    "StreamId::new(",
    "StreamId::from_static",
    "MaxRetries::try_new",
    "CommandError::Unauthorized",
    ".events_written",
    ".affected_streams",
    "emit!(",
];

/// Receivers that denote an event store / reader / checkpoint store, paired with
/// methods that do NOT exist on the real traits. Flagged as `<receiver>.<method>(`.
const STORE_RECEIVERS: &[&str] = &[
    "event_store",
    "store",
    "backend",
    "reader",
    "checkpoint_store",
    "coordinator",
];
const FABRICATED_STORE_METHODS: &[&str] = &[
    "write_events",
    "list_all_streams",
    "read_events_since",
    "read_events_batch",
    "read_next_event",
    "read_all_events",
    "write_versioned_events",
    "read_versioned_stream",
    "clear_all",
    "sample_events",
    "health_check",
    "migration_status",
    "initialize",
    "append", // the real method is append_events
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eventcore-examples has a parent dir")
        .to_path_buf()
}

/// Collect the markdown files that make up the *current* user-facing docs.
/// ADRs and the development archive are intentionally excluded: they are
/// historical records that legitimately describe APIs that no longer exist.
fn doc_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    collect_md(&root.join("docs/manual"), &mut files);
    collect_md(&root.join("blueprints"), &mut files);
    push_if_exists(&root.join("README.md"), &mut files);
    // Per-crate READMEs.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("eventcore-") {
                push_if_exists(&entry.path().join("README.md"), &mut files);
            }
        }
    }
    files
}

fn push_if_exists(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
    }
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// A Rust code block extracted from a markdown file, with the 1-based line
/// number of its first content line (for diagnostics).
struct CodeBlock {
    start_line: usize,
    text: String,
}

/// Extract fenced ```rust code blocks. `ignore`/`text`/non-rust fences are
/// skipped. The opening fence info string is checked for "rust".
fn rust_blocks(markdown: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            let info = info.trim();
            let is_rust = info == "rust"
                || info.starts_with("rust,")
                || info.starts_with("rust ")
                || info == "rust,no_run"
                || info == "rust,should_panic";
            // Find the closing fence.
            let mut j = i + 1;
            let mut body = String::new();
            while j < lines.len() && lines[j].trim_start() != "```" {
                body.push_str(lines[j]);
                body.push('\n');
                j += 1;
            }
            if is_rust {
                blocks.push(CodeBlock {
                    start_line: i + 2, // first body line, 1-based
                    text: body,
                });
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    blocks
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whole-word occurrences of `needle` in `hay` (boundaries are non-`[A-Za-z0-9_]`).
fn word_match(hay: &str, needle: &str) -> bool {
    let bytes = hay.as_bytes();
    let nbytes = needle.as_bytes();
    if nbytes.is_empty() {
        return false;
    }
    let mut idx = 0;
    while let Some(pos) = hay[idx..].find(needle) {
        let start = idx + pos;
        let end = start + needle.len();
        let before_ok =
            start == 0 || !is_word_char(hay[..start].chars().next_back().unwrap_or(' '));
        let after_ok =
            end >= bytes.len() || !is_word_char(hay[end..].chars().next().unwrap_or(' '));
        if before_ok && after_ok {
            return true;
        }
        idx = start + 1;
    }
    false
}

/// Does the block call `<receiver>.<method>(` for any store-like receiver?
fn calls_store_method(block: &str, method: &str) -> bool {
    let pat = format!(".{method}(");
    let mut idx = 0;
    while let Some(pos) = block[idx..].find(&pat) {
        let dot = idx + pos;
        // Walk back over the receiver identifier immediately before the dot.
        let prefix = &block[..dot];
        let recv: String = prefix
            .chars()
            .rev()
            .take_while(|c| is_word_char(*c))
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if STORE_RECEIVERS.iter().any(|r| *r == recv) {
            return true;
        }
        idx = dot + 1;
    }
    false
}

#[test]
fn docs_reference_only_real_eventcore_apis() {
    let mut violations: Vec<String> = Vec::new();
    let root = workspace_root();

    for file in doc_files() {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
        let content =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("failed to read {rel}: {e}"));
        for block in rust_blocks(&content) {
            let loc = format!("{rel}:{}", block.start_line);
            let b = &block.text;

            for ty in FABRICATED_TYPES {
                if word_match(b, ty) {
                    violations.push(format!("{loc}: fabricated type `{ty}`"));
                }
            }
            for path in WRONG_CRATE_PATHS {
                if b.contains(path) {
                    violations.push(format!(
                        "{loc}: `{path}` is not re-exported by the `eventcore` facade (use `eventcore_types::`)"
                    ));
                }
            }
            for pat in FABRICATED_PATTERNS {
                if b.contains(pat) {
                    violations.push(format!("{loc}: fabricated API `{pat}`"));
                }
            }
            for method in FABRICATED_STORE_METHODS {
                if calls_store_method(b, method) {
                    violations.push(format!(
                        "{loc}: fabricated store method `.{method}()` (real trait methods: read_stream, append_events, read_events, load, save)"
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "documentation references EventCore APIs that do not exist:\n{}\n\n\
         If one of these is now a real public API, update the lists in \
         eventcore-examples/tests/doc_accuracy_test.rs.",
        violations.join("\n")
    );
}
