//! On-disk transaction-file format (ADR-0038).
//!
//! Each transaction is one immutable JSONL file: line 1 is a header, lines
//! 2..N are one event envelope each. The merge-mode header fields are recorded
//! from the first commit because the format is immutable.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use eventcore_types::{EventStoreError, Operation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::FsyncPolicy;
use crate::error::FsEventStoreError;

/// Current on-disk transaction-file format version.
///
/// Version 2 adds the `content_hash` integrity anchor (ADR-0046). The field is
/// optional and serde-defaulted, so version-1 files written before the anchor
/// existed still deserialize — they simply skip the read-time fsck.
pub(crate) const FORMAT_VERSION: u32 = 2;

/// One line of a transaction file: either the header (line 1) or an event.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum Line {
    Header(TransactionHeader),
    Event(EventEnvelope),
}

/// Header record (first line of every transaction file).
///
/// The merge-mode fields (`replica_id`, `parent_transaction_ids`,
/// `stream_bases`) are recorded from the first commit even in single-writer
/// mode, because the file format is immutable (ADR-0038). The linearization
/// engine reads `parent_transaction_ids` from day one (ADR-0039).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TransactionHeader {
    pub(crate) format_version: u32,
    pub(crate) transaction_id: Uuid,
    pub(crate) replica_id: Uuid,
    pub(crate) parent_transaction_ids: Vec<Uuid>,
    pub(crate) created_at: String,
    pub(crate) stream_bases: BTreeMap<String, usize>,
    /// SHA-256 integrity anchor over the event payload (ADR-0046, v2+). A
    /// read-time fsck recomputes this and rejects a file whose payload no
    /// longer matches — catching an illegal edit that `merge=union` would mask.
    /// Absent on legacy version-1 files, which skip the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_hash: Option<String>,
}

/// One event envelope (lines 2..N of a transaction file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EventEnvelope {
    pub(crate) event_id: Uuid,
    pub(crate) stream_id: String,
    /// Writer's locally-assigned version. Advisory: the authoritative version
    /// is computed at read time by linearization (ADR-0039).
    pub(crate) stream_version: usize,
    pub(crate) event_type: String,
    pub(crate) event_data: serde_json::Value,
    pub(crate) metadata: serde_json::Value,
}

/// The outcome of loading a transaction file at read time.
pub(crate) enum LoadedTransaction {
    /// The file parsed and its payload matched the integrity anchor (or the
    /// file is a legacy version-1 file with no anchor to check).
    Valid(TransactionHeader, Vec<EventEnvelope>),
    /// The payload did not match the header's integrity anchor — an illegal
    /// edit (ADR-0046). The transaction is rejected, not parsed as trustworthy.
    Integrity {
        transaction_id: Uuid,
        detail: String,
    },
}

/// The SHA-256 integrity anchor for a transaction's event payload, as a
/// lowercase hex string. The payload is the file content after the header line
/// — i.e. exactly the bytes a `merge=union` could splice (ADR-0046).
pub(crate) fn content_hash(payload: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(payload.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Split a transaction file into its header line and event payload. The header
/// is always line 1; the payload is everything after the first newline — the
/// exact bytes the content hash anchors.
fn split_header_payload<'a>(
    contents: &'a str,
    path: &Path,
) -> Result<(&'a str, &'a str), FsEventStoreError> {
    match contents.find('\n') {
        Some(index) => Ok((&contents[..index], &contents[index + 1..])),
        None => Err(FsEventStoreError::Corrupted {
            path: path.to_path_buf(),
            detail: "missing header line".to_string(),
        }),
    }
}

fn parse_header(header_line: &str, path: &Path) -> Result<TransactionHeader, FsEventStoreError> {
    match serde_json::from_str::<Line>(header_line) {
        Ok(Line::Header(header)) => Ok(header),
        Ok(Line::Event(_)) => Err(FsEventStoreError::Corrupted {
            path: path.to_path_buf(),
            detail: "first line is not a header".to_string(),
        }),
        Err(error) => Err(FsEventStoreError::Corrupted {
            path: path.to_path_buf(),
            detail: format!("invalid header line: {error}"),
        }),
    }
}

fn parse_events(payload: &str, path: &Path) -> Result<Vec<EventEnvelope>, FsEventStoreError> {
    let mut events: Vec<EventEnvelope> = Vec::new();
    for line in payload.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(line) {
            Ok(Line::Event(event)) => events.push(event),
            Ok(Line::Header(_)) => {
                return Err(FsEventStoreError::Corrupted {
                    path: path.to_path_buf(),
                    detail: "unexpected header record among events".to_string(),
                });
            }
            Err(error) => {
                return Err(FsEventStoreError::Corrupted {
                    path: path.to_path_buf(),
                    detail: format!("invalid line: {error}"),
                });
            }
        }
    }
    Ok(events)
}

/// Load a transaction file with a read-time integrity check (ADR-0046). A file
/// whose payload does not match its recorded `content_hash` anchor is reported
/// as [`LoadedTransaction::Integrity`] rather than parsed as trustworthy.
pub(crate) fn load_transaction(path: &Path) -> Result<LoadedTransaction, FsEventStoreError> {
    let contents = fs::read_to_string(path).map_err(|error| FsEventStoreError::Corrupted {
        path: path.to_path_buf(),
        detail: format!("read failed: {error}"),
    })?;
    let (header_line, payload) = split_header_payload(&contents, path)?;
    let header = parse_header(header_line, path)?;
    if let Some(expected) = &header.content_hash {
        let actual = content_hash(payload);
        if &actual != expected {
            return Ok(LoadedTransaction::Integrity {
                transaction_id: header.transaction_id,
                detail: format!(
                    "content hash mismatch: header anchor {expected} != computed {actual}"
                ),
            });
        }
    }
    let events = parse_events(payload, path)?;
    Ok(LoadedTransaction::Valid(header, events))
}

/// Parse a transaction known to be trustworthy (e.g. one already validated by a
/// prior scan). An integrity failure here is unexpected and surfaces as a
/// corruption error.
pub(crate) fn parse_transaction(
    path: &Path,
) -> Result<(TransactionHeader, Vec<EventEnvelope>), FsEventStoreError> {
    match load_transaction(path)? {
        LoadedTransaction::Valid(header, events) => Ok((header, events)),
        LoadedTransaction::Integrity {
            transaction_id,
            detail,
        } => Err(FsEventStoreError::Corrupted {
            path: path.to_path_buf(),
            detail: format!("integrity failure for {transaction_id}: {detail}"),
        }),
    }
}

pub(crate) fn serialize_transaction(
    header: &TransactionHeader,
    events: &[EventEnvelope],
) -> Result<String, EventStoreError> {
    let mut event_lines: Vec<String> = Vec::with_capacity(events.len());
    for event in events {
        event_lines.push(serialize_line(&Line::Event(event.clone()))?);
    }
    // The payload is the event lines plus the trailing newline — exactly the
    // bytes loaded as `payload` on read, so the anchor recomputes identically.
    let mut payload = event_lines.join("\n");
    payload.push('\n');

    let mut header = header.clone();
    header.content_hash = Some(content_hash(&payload));
    let header_line = serialize_line(&Line::Header(header))?;
    Ok(format!("{header_line}\n{payload}"))
}

fn serialize_line(line: &Line) -> Result<String, EventStoreError> {
    serde_json::to_string(line).map_err(|_| EventStoreError::StoreFailure {
        operation: Operation::AppendEvents,
    })
}

pub(crate) fn write_transaction_file(
    tmp: &Path,
    final_path: &Path,
    events_dir: &Path,
    body: &str,
    fsync: FsyncPolicy,
) -> std::io::Result<()> {
    {
        let mut file = File::create(tmp)?;
        file.write_all(body.as_bytes())?;
        if matches!(fsync, FsyncPolicy::Full) {
            file.sync_all()?;
        }
    }
    fs::rename(tmp, final_path)?;
    if matches!(fsync, FsyncPolicy::Full)
        && let Ok(dir) = File::open(events_dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}
