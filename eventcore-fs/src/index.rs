//! In-memory linearized read model (ADR-0039) and the linearization engine
//! that computes canonical order and stream versions at read time.

use std::collections::{HashMap, HashSet};
use std::fs;

use uuid::Uuid;

use crate::config::Roots;
use crate::error::FsEventStoreError;
use crate::format::{EventEnvelope, TransactionHeader, parse_transaction};

/// An event as held in the in-memory linearized index.
#[derive(Debug, Clone)]
pub(crate) struct IndexedEvent {
    pub(crate) event_id: Uuid,
    pub(crate) stream_id: String,
    pub(crate) event_type: String,
    pub(crate) event_data: serde_json::Value,
}

/// The linearized read model, rebuilt from `events/` on open.
#[derive(Debug, Default)]
pub(crate) struct Index {
    pub(crate) headers: HashMap<Uuid, TransactionHeader>,
    /// Events in canonical (linearized) global order.
    pub(crate) global: Vec<IndexedEvent>,
    /// Per-stream events in canonical order.
    pub(crate) streams: HashMap<String, Vec<IndexedEvent>>,
    /// Transactions with no children — the current head(s).
    pub(crate) tips: Vec<Uuid>,
}

impl Index {
    pub(crate) fn stream_head(&self, stream_id: &str) -> usize {
        self.streams.get(stream_id).map(Vec::len).unwrap_or(0)
    }
}

/// Compute the number of ancestor transactions reachable via parent pointers.
/// In single-writer mode (a chain) this is a strictly increasing total order.
fn transaction_depth(
    id: Uuid,
    headers: &HashMap<Uuid, TransactionHeader>,
    memo: &mut HashMap<Uuid, usize>,
) -> usize {
    if let Some(depth) = memo.get(&id) {
        return *depth;
    }
    let depth = match headers.get(&id) {
        None => 0,
        Some(header) => {
            let mut best = 0;
            for parent in &header.parent_transaction_ids {
                if headers.contains_key(parent) {
                    best = best.max(transaction_depth(*parent, headers, memo) + 1);
                }
            }
            best
        }
    };
    let _ = memo.insert(id, depth);
    depth
}

/// Order transactions into the canonical linear sequence.
///
/// Transactions are sorted by ancestor depth (so every parent precedes its
/// children — `depth(child) >= depth(parent) + 1` makes depth-order a valid
/// topological order), with concurrent transactions (equal depth) broken by
/// the deterministic tuple `(created_at, replica_id, transaction_id)`. Every
/// component is an immutable recorded value, so all replicas holding the same
/// file set compute the identical order (ADR-0039 convergence guarantee).
fn linearize(headers: &HashMap<Uuid, TransactionHeader>) -> Vec<Uuid> {
    let mut memo: HashMap<Uuid, usize> = HashMap::new();
    let mut keyed: Vec<(Uuid, (usize, String, Uuid, Uuid))> = headers
        .iter()
        .map(|(id, header)| {
            let depth = transaction_depth(*id, headers, &mut memo);
            (
                *id,
                (depth, header.created_at.clone(), header.replica_id, *id),
            )
        })
        .collect();
    keyed.sort_by(|(_, left), (_, right)| left.cmp(right));
    keyed.into_iter().map(|(id, _)| id).collect()
}

/// The tip transactions are those no other transaction lists as a parent.
pub(crate) fn compute_tips(headers: &HashMap<Uuid, TransactionHeader>) -> Vec<Uuid> {
    let mut referenced: HashSet<Uuid> = HashSet::new();
    for header in headers.values() {
        for parent in &header.parent_transaction_ids {
            let _ = referenced.insert(*parent);
        }
    }
    let mut tips: Vec<Uuid> = headers
        .keys()
        .copied()
        .filter(|id| !referenced.contains(id))
        .collect();
    tips.sort();
    tips
}

fn build_index(transactions: Vec<(TransactionHeader, Vec<EventEnvelope>)>) -> Index {
    let mut headers: HashMap<Uuid, TransactionHeader> = HashMap::new();
    let mut events_by_txn: HashMap<Uuid, Vec<EventEnvelope>> = HashMap::new();
    for (header, events) in transactions {
        let id = header.transaction_id;
        let _ = headers.insert(id, header);
        let _ = events_by_txn.insert(id, events);
    }

    let order = linearize(&headers);
    let tips = compute_tips(&headers);

    let mut global: Vec<IndexedEvent> = Vec::new();
    let mut streams: HashMap<String, Vec<IndexedEvent>> = HashMap::new();
    for transaction_id in order {
        let events = events_by_txn.remove(&transaction_id).unwrap_or_default();
        for envelope in events {
            let indexed = IndexedEvent {
                event_id: envelope.event_id,
                stream_id: envelope.stream_id,
                event_type: envelope.event_type,
                event_data: envelope.event_data,
            };
            streams
                .entry(indexed.stream_id.clone())
                .or_default()
                .push(indexed.clone());
            global.push(indexed);
        }
    }

    Index {
        headers,
        global,
        streams,
        tips,
    }
}

/// Build event envelopes and their index entries for a transaction, assigning
/// each event a contiguous per-stream version above the stream's current head.
/// Shared by the normal append path and the merge path so the version
/// arithmetic is computed — and tested — in exactly one place.
pub(crate) fn build_envelopes(
    index: &Index,
    items: Vec<(String, &'static str, serde_json::Value)>,
) -> (Vec<EventEnvelope>, Vec<IndexedEvent>) {
    let mut added: HashMap<String, usize> = HashMap::new();
    let mut envelopes: Vec<EventEnvelope> = Vec::with_capacity(items.len());
    let mut new_events: Vec<IndexedEvent> = Vec::with_capacity(items.len());
    for (key, event_type, event_data) in items {
        let head = index.stream_head(&key);
        let offset = added.entry(key.clone()).or_insert(0);
        *offset += 1;
        let stream_version = head + *offset;
        let event_id = Uuid::now_v7();
        envelopes.push(EventEnvelope {
            event_id,
            stream_id: key.clone(),
            stream_version,
            event_type: event_type.to_string(),
            event_data: event_data.clone(),
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        });
        new_events.push(IndexedEvent {
            event_id,
            stream_id: key,
            event_type: event_type.to_string(),
            event_data,
        });
    }
    (envelopes, new_events)
}

pub(crate) fn scan_index(roots: &Roots) -> Result<Index, FsEventStoreError> {
    let read_dir = fs::read_dir(&roots.events).map_err(|source| FsEventStoreError::InitFailed {
        path: roots.events.clone(),
        source,
    })?;
    let mut transactions: Vec<(TransactionHeader, Vec<EventEnvelope>)> = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| FsEventStoreError::InitFailed {
            path: roots.events.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        transactions.push(parse_transaction(&path)?);
    }
    Ok(build_index(transactions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn header(id: Uuid, parents: Vec<Uuid>) -> TransactionHeader {
        TransactionHeader {
            format_version: crate::format::FORMAT_VERSION,
            transaction_id: id,
            replica_id: Uuid::now_v7(),
            parent_transaction_ids: parents,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            stream_bases: BTreeMap::new(),
        }
    }

    #[test]
    fn linearize_orders_chain_by_parent_depth() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        let mut headers: HashMap<Uuid, TransactionHeader> = HashMap::new();
        // Insert out of order to prove ordering comes from parent links, not insertion.
        let _ = headers.insert(c, header(c, vec![b]));
        let _ = headers.insert(a, header(a, vec![]));
        let _ = headers.insert(b, header(b, vec![a]));

        assert_eq!(linearize(&headers), vec![a, b, c]);
        assert_eq!(compute_tips(&headers), vec![c]);
    }

    #[test]
    fn transaction_depth_takes_max_of_known_parents_and_ignores_dangling() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        let d = Uuid::now_v7();
        let e = Uuid::now_v7();
        let missing = Uuid::now_v7();
        let mut headers: HashMap<Uuid, TransactionHeader> = HashMap::new();
        let _ = headers.insert(a, header(a, vec![]));
        let _ = headers.insert(b, header(b, vec![a]));
        let _ = headers.insert(c, header(c, vec![b]));
        // d has a shallow parent (a, depth 0) and a deep parent (c, depth 2),
        // plus a dangling parent that must be ignored: depth = max(1, 3) = 3.
        let _ = headers.insert(d, header(d, vec![a, c, missing]));
        // e's only parent is dangling, so it is treated as a root: depth 0.
        let _ = headers.insert(e, header(e, vec![missing]));

        let mut memo: HashMap<Uuid, usize> = HashMap::new();
        assert_eq!(transaction_depth(a, &headers, &mut memo), 0);
        assert_eq!(transaction_depth(b, &headers, &mut memo), 1);
        assert_eq!(transaction_depth(c, &headers, &mut memo), 2);
        assert_eq!(transaction_depth(d, &headers, &mut memo), 3);
        assert_eq!(transaction_depth(e, &headers, &mut memo), 0);
    }
}
