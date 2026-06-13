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
pub(crate) const FORMAT_VERSION: u32 = 1;

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

pub(crate) fn parse_transaction(
    path: &Path,
) -> Result<(TransactionHeader, Vec<EventEnvelope>), FsEventStoreError> {
    let contents = fs::read_to_string(path).map_err(|error| FsEventStoreError::Corrupted {
        path: path.to_path_buf(),
        detail: format!("read failed: {error}"),
    })?;
    let mut header: Option<TransactionHeader> = None;
    let mut events: Vec<EventEnvelope> = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Line =
            serde_json::from_str(line).map_err(|error| FsEventStoreError::Corrupted {
                path: path.to_path_buf(),
                detail: format!("invalid line: {error}"),
            })?;
        match parsed {
            Line::Header(parsed_header) => header = Some(parsed_header),
            Line::Event(event) => events.push(event),
        }
    }
    let header = header.ok_or_else(|| FsEventStoreError::Corrupted {
        path: path.to_path_buf(),
        detail: "missing header line".to_string(),
    })?;
    Ok((header, events))
}

pub(crate) fn serialize_transaction(
    header: &TransactionHeader,
    events: &[EventEnvelope],
) -> Result<String, EventStoreError> {
    let mut lines: Vec<String> = Vec::with_capacity(events.len() + 1);
    lines.push(serialize_line(&Line::Header(header.clone()))?);
    for event in events {
        lines.push(serialize_line(&Line::Event(event.clone()))?);
    }
    let mut body = lines.join("\n");
    body.push('\n');
    Ok(body)
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
