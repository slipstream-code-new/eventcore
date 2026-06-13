//! Merge mode (Layer 2): fork detection and the reconciliation API types
//! (ADR-0041, ADR-0042, ADR-0045).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use eventcore_types::{StreamId, StreamVersion};
use nutype::nutype;
use uuid::Uuid;

use crate::error::FsEventStoreError;
use crate::format::TransactionHeader;

/// Identifies a transaction by its UUID7.
#[nutype(derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Ord,
    PartialOrd,
    Display,
    Into,
    Serialize,
    Deserialize
))]
pub struct TransactionId(Uuid);

/// A divergence on a stream: two or more concurrent transactions that each
/// extended the stream from the same base version (ADR-0041). A fork only
/// arises after a `git merge` unions histories written offline; single-writer
/// histories never fork.
#[derive(Debug, Clone)]
pub struct Fork {
    stream_id: StreamId,
    base_version: StreamVersion,
    transactions: Vec<TransactionId>,
}

impl Fork {
    /// The stream that diverged.
    pub fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    /// The version both branches built on before diverging.
    pub fn base_version(&self) -> StreamVersion {
        self.base_version
    }

    /// The concurrent transactions, in deterministic order.
    pub fn transactions(&self) -> &[TransactionId] {
        &self.transactions
    }
}

/// A transaction whose `parent_transaction_ids` reference files not present on
/// disk — a partial or aborted `git merge` (ADR-0046). It is reported, never
/// crashed on or silently dropped; when the missing files arrive the reference
/// resolves on the next read.
#[derive(Debug, Clone)]
pub struct DanglingTransaction {
    pub(crate) transaction_id: TransactionId,
    pub(crate) missing_parents: Vec<TransactionId>,
}

impl DanglingTransaction {
    /// The transaction with one or more absent parents.
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// The parent transactions referenced but not present on disk.
    pub fn missing_parents(&self) -> &[TransactionId] {
        &self.missing_parents
    }
}

/// A transaction file rejected by the read-time fsck because its payload did
/// not match the recorded content-hash anchor — an illegal edit a `merge=union`
/// would otherwise mask (ADR-0046).
#[derive(Debug, Clone)]
pub struct IntegrityFailure {
    pub(crate) transaction_id: TransactionId,
    pub(crate) detail: String,
}

impl IntegrityFailure {
    /// The rejected transaction.
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// A human-readable description of the integrity mismatch.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// A snapshot of the store's reconciliation state.
#[derive(Debug, Clone)]
pub struct StoreStatus {
    pub(crate) forks: Vec<Fork>,
    pub(crate) dangling: Vec<DanglingTransaction>,
    pub(crate) integrity_failures: Vec<IntegrityFailure>,
}

impl StoreStatus {
    /// All currently-detected forks.
    pub fn forks(&self) -> &[Fork] {
        &self.forks
    }

    /// Transactions referencing absent parents (a partial/aborted git merge).
    pub fn dangling(&self) -> &[DanglingTransaction] {
        &self.dangling
    }

    /// Transaction files rejected by the read-time fsck.
    pub fn integrity_failures(&self) -> &[IntegrityFailure] {
        &self.integrity_failures
    }

    /// True when there are no unresolved forks, no dangling transactions, and
    /// no integrity failures.
    pub fn is_clean(&self) -> bool {
        self.forks.is_empty() && self.dangling.is_empty() && self.integrity_failures.is_empty()
    }
}

/// The divergent branches of a fork, presented to a resolver (ADR-0042).
#[derive(Debug, Clone)]
pub struct ForkContext<E> {
    pub(crate) stream_id: StreamId,
    pub(crate) base_version: StreamVersion,
    pub(crate) branches: Vec<BranchView<E>>,
}

impl<E> ForkContext<E> {
    /// The stream that diverged.
    pub fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    /// The version both branches built on before diverging.
    pub fn base_version(&self) -> StreamVersion {
        self.base_version
    }

    /// The divergent branches.
    pub fn branches(&self) -> &[BranchView<E>] {
        &self.branches
    }
}

/// One divergent branch: the events a single transaction contributed to the
/// forked stream.
#[derive(Debug, Clone)]
pub struct BranchView<E> {
    pub(crate) transaction_id: TransactionId,
    pub(crate) events: Vec<E>,
}

impl<E> BranchView<E> {
    /// The transaction that produced this branch.
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// The events this branch contributed to the forked stream.
    pub fn events(&self) -> &[E] {
        &self.events
    }
}

/// A resolver's decision for one fork (ADR-0042).
#[derive(Debug, Clone)]
pub enum ResolutionOutcome<E> {
    /// Record these events as an N-parent merge transaction collapsing the
    /// fork. The events are produced by the application's domain logic.
    Resolve(Vec<E>),
    /// Leave the fork in place; it needs human or later attention.
    Unresolvable(String),
}

/// The outcome of a [`crate::FileEventStore::reconcile`] pass.
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub(crate) resolved: usize,
    pub(crate) unresolved: Vec<StreamId>,
}

impl ReconcileReport {
    /// How many forks were collapsed into merge transactions.
    pub fn resolved_count(&self) -> usize {
        self.resolved
    }

    /// The streams whose forks the resolver declined to resolve.
    pub fn unresolved_streams(&self) -> &[StreamId] {
        &self.unresolved
    }
}

/// Returns true if `ancestor` is reachable from `descendant` via parent links.
fn is_ancestor(
    ancestor: Uuid,
    descendant: Uuid,
    headers: &HashMap<Uuid, TransactionHeader>,
) -> bool {
    let mut stack: Vec<Uuid> = vec![descendant];
    let mut seen: HashSet<Uuid> = HashSet::new();
    while let Some(current) = stack.pop() {
        if let Some(header) = headers.get(&current) {
            for parent in &header.parent_transaction_ids {
                if *parent == ancestor {
                    return true;
                }
                if seen.insert(*parent) {
                    stack.push(*parent);
                }
            }
        }
    }
    false
}

/// A fork is resolved once some transaction descends from every one of its
/// heads — i.e. a merge node joins the divergent branches (ADR-0042).
fn fork_is_resolved(heads: &[Uuid], headers: &HashMap<Uuid, TransactionHeader>) -> bool {
    headers.keys().any(|&candidate| {
        heads
            .iter()
            .all(|&head| is_ancestor(head, candidate, headers))
    })
}

/// Find transactions that reference parents not present on disk (ADR-0046).
/// Linearization already tolerates missing parents (they are treated as absent
/// edges); this reports them so a partial/aborted merge is observable.
pub(crate) fn detect_dangling_in(
    headers: &HashMap<Uuid, TransactionHeader>,
) -> Vec<DanglingTransaction> {
    let mut dangling: Vec<DanglingTransaction> = Vec::new();
    for header in headers.values() {
        let missing: Vec<TransactionId> = header
            .parent_transaction_ids
            .iter()
            .filter(|parent| !headers.contains_key(*parent))
            .map(|parent| TransactionId::new(*parent))
            .collect();
        if !missing.is_empty() {
            dangling.push(DanglingTransaction {
                transaction_id: TransactionId::new(header.transaction_id),
                missing_parents: missing,
            });
        }
    }
    dangling.sort_by_key(DanglingTransaction::transaction_id);
    dangling
}

/// Fail loud if two concurrent transactions share a `replica_id` (ADR-0044).
///
/// A single linear writer's transactions form a chain — every pair is in an
/// ancestor relation. Two transactions that share a `replica_id` yet are
/// concurrent (neither is an ancestor of the other) therefore cannot have come
/// from one writer: the copy trap manifested. Rather than silently merging a
/// corrupt-but-linear-looking history, surface the colliding pair.
fn check_replica_collisions(
    headers: &HashMap<Uuid, TransactionHeader>,
) -> Result<(), FsEventStoreError> {
    let mut by_replica: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for header in headers.values() {
        by_replica
            .entry(header.replica_id)
            .or_default()
            .push(header.transaction_id);
    }
    for (replica_id, mut transactions) in by_replica {
        if transactions.len() < 2 {
            continue;
        }
        transactions.sort();
        for i in 0..transactions.len() {
            for j in (i + 1)..transactions.len() {
                let (first, second) = (transactions[i], transactions[j]);
                if !is_ancestor(first, second, headers) && !is_ancestor(second, first, headers) {
                    return Err(FsEventStoreError::ReplicaIdentityConflict {
                        first,
                        second,
                        replica_id,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn detect_forks_in(
    headers: &HashMap<Uuid, TransactionHeader>,
) -> Result<Vec<Fork>, FsEventStoreError> {
    check_replica_collisions(headers)?;

    // Group transactions by the (stream, base) they claimed to extend.
    let mut groups: BTreeMap<(String, usize), Vec<Uuid>> = BTreeMap::new();
    for header in headers.values() {
        for (stream, base) in &header.stream_bases {
            groups
                .entry((stream.clone(), *base))
                .or_default()
                .push(header.transaction_id);
        }
    }

    let mut forks: Vec<Fork> = Vec::new();
    for ((stream, base), candidates) in groups {
        // Two or more transactions claiming the same (stream, base) each
        // extended the stream from that version independently — a fork. A
        // descendant would have recorded a higher base, so same-base siblings
        // are always concurrent.
        if candidates.len() < 2 {
            continue;
        }
        if fork_is_resolved(&candidates, headers) {
            continue;
        }
        let stream_id =
            StreamId::try_new(&stream).map_err(|error| FsEventStoreError::Corrupted {
                path: PathBuf::from(&stream),
                detail: format!("invalid stream id in transaction header: {error}"),
            })?;
        let mut transactions: Vec<TransactionId> =
            candidates.into_iter().map(TransactionId::new).collect();
        transactions.sort();
        forks.push(Fork {
            stream_id,
            base_version: StreamVersion::new(base),
            transactions,
        });
    }
    Ok(forks)
}
