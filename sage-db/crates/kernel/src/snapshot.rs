//! The concrete [`SnapshotView`] adapter over one immutable Store publication.
//!
//! # What this is
//!
//! `sage-db-types::snapshot` declares *what* a read-only view must answer. This module
//! answers it over the real store: one durable, fully replayed publication whose records
//! are **borrowed**, not copied, plus one [`BaseGraph`] built under the caller's ledger and
//! retained for the view's lifetime.
//!
//! There is no second record model here: terms, assertions, claims and session checkpoints
//! are the store's own records, borrowed from the retained fold for the same lifetime as the
//! `&Store` that produced them.
//!
//! # Admission and ownership
//!
//! A counted pass admits each borrowed-reference table before its fallible allocation.
//! A second pass fills those tables without growing them. Graph input arrays are admitted
//! before allocation; the graph builder admits only its derived indices. Existing credit is
//! transferred once into the view's retained lease. Temporary canonical hash bytes remain
//! attempt credit and are not transferred. These are reservation claims, not RSS measurements.

pub use sage_db_infer::validate::{ValidationReport, Violation};

use sage_db_store::fold::FoldedRecord;
use sage_db_store::handle::Store;
use sage_db_store::lease::{AttemptLedger, LeaseLedger, OwnedReservation};
use sage_db_store::typed;
use sage_db_store::view::{
    BlobReadError, CommitReadError, ImportReadiness, RecordReadError, RecordScanError,
    ViewDurability,
};
use sage_db_types::digest::Digest;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::graph::{BaseGraph, SnapshotId};
use sage_db_types::identity::{AssertionId, KbUuid, KbVersion, TermId};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    AssertionRecordFull, ClaimRecord, RecordKey, RecordKind, SessionCheckpointRecord, TermRecord,
};
use sage_db_types::record::{AssertionRecord, TermMeta};
use sage_db_types::snapshot::{
    CommittedRules, ReadRefusal, RecordRef, RecordView, ScanError, SnapshotFamily,
    SnapshotOpenError, SnapshotView,
};

/// One record slot as the store retains it.
///
/// Borrowed: the variant holds the store's own record for the view's lifetime, so no payload
/// bytes are copied and no reservation is silently created on the caller's behalf.
#[derive(Clone, Copy, Debug)]
enum BorrowedRecord<'a> {
    Term(&'a TermRecord),
    Assertion(&'a AssertionRecordFull),
    Claim(&'a ClaimRecord),
    SessionCheckpoint(&'a SessionCheckpointRecord),
}

impl<'a> BorrowedRecord<'a> {
    fn as_ref(self) -> RecordRef<'a> {
        match self {
            Self::Term(record) => RecordRef::Term(record),
            Self::Assertion(record) => RecordRef::Assertion(record),
            Self::Claim(record) => RecordRef::Claim(record),
            Self::SessionCheckpoint(record) => RecordRef::SessionCheckpoint(record),
        }
    }
}

/// Failure to construct a snapshot, preserving the upstream operation and remedy.
#[derive(Debug)]
pub enum SnapshotBuildError {
    /// Publication, rules interpretation or graph admission verdict.
    View(SnapshotOpenError),
    /// Actual metadata read failure, including internal retained-index defects.
    Metadata(CommitReadError),
    /// Actual committed-blob failure, including filesystem kind and effect.
    Blob(BlobReadError),
    /// Actual record-read failure.
    Record(RecordReadError),
    /// Verified rules disagree with the publication pin, an internal basis invariant failure.
    RulesBindingMismatch,
    /// A byte or slot extent cannot be represented.
    ExtentOverflow,
    /// A successful reservation exceeded the exact admitted capacity.
    UnexpectedCapacity {
        /// Slot count admitted before reservation.
        requested: usize,
        /// Actual reported capacity.
        actual: usize,
    },
    /// A counted immutable scan changed shape, or an invalid graph identity was retained.
    InvalidRecord(RecordKey),
    /// A counted immutable scan did not fill exactly its admitted slots.
    CountMismatch,
    /// Canonical graph assembly preserved without relabelling a structural defect.
    GraphDigest(typed::DigestAssemblyError),
    /// Recomputed active graph differs from the publication's declared identity.
    GraphDigestMismatch {
        /// Declared graph digest.
        expected: [u8; 32],
        /// Canonically recomputed graph digest.
        actual: [u8; 32],
    },
}

impl From<SnapshotOpenError> for SnapshotBuildError {
    fn from(error: SnapshotOpenError) -> Self {
        Self::View(error)
    }
}

fn refused(error: Refusal) -> SnapshotBuildError {
    SnapshotOpenError::Refused(error).into()
}

fn build_step(ledger: &mut dyn SharedLedger) -> Result<(), SnapshotBuildError> {
    ledger
        .checkpoint()
        .map_err(|stop| refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| refused(Refusal::Budget(error)))
}

/// Admit a vector's backing slots before making its one fallible reservation.
fn admitted_vector<T>(
    count: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<T>, SnapshotBuildError> {
    build_step(ledger)?;
    let bytes = count
        .checked_mul(core::mem::size_of::<T>())
        .ok_or(SnapshotBuildError::ExtentOverflow)?;
    let extent = u64::try_from(bytes).map_err(|_| SnapshotBuildError::ExtentOverflow)?;
    ledger
        .try_charge(extent)
        .map_err(|error| refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(u64::try_from(count).map_err(|_| SnapshotBuildError::ExtentOverflow)?)
        .map_err(|error| refused(Refusal::Budget(error)))?;
    let mut vector = Vec::new();
    vector
        .try_reserve_exact(count)
        .map_err(|_| refused(Refusal::Allocation { requested: bytes }))?;
    if vector.capacity() != count {
        return Err(SnapshotBuildError::UnexpectedCapacity {
            requested: count,
            actual: vector.capacity(),
        });
    }
    Ok(vector)
}

/// Borrowed slots; their payloads retain the Store's existing owners.
struct RecordSlots<'a> {
    terms: Vec<&'a TermRecord>,
    assertions: Vec<&'a AssertionRecordFull>,
    claims: Vec<&'a ClaimRecord>,
    checkpoints: Vec<&'a SessionCheckpointRecord>,
}

impl RecordSlots<'_> {
    fn slot_bytes(&self) -> Result<u64, SnapshotBuildError> {
        fn bytes<T>(vector: &Vec<T>) -> Result<u64, SnapshotBuildError> {
            let size = vector
                .capacity()
                .checked_mul(core::mem::size_of::<T>())
                .ok_or(SnapshotBuildError::ExtentOverflow)?;
            u64::try_from(size).map_err(|_| SnapshotBuildError::ExtentOverflow)
        }
        let mut total = 0u64;
        for extent in [
            bytes(&self.terms)?,
            bytes(&self.assertions)?,
            bytes(&self.claims)?,
            bytes(&self.checkpoints)?,
        ] {
            total = total
                .checked_add(extent)
                .ok_or(SnapshotBuildError::ExtentOverflow)?;
        }
        Ok(total)
    }
    fn total(&self) -> usize {
        self.terms.len() + self.assertions.len() + self.claims.len() + self.checkpoints.len()
    }
}

fn map_scan(error: RecordScanError<SnapshotBuildError>) -> SnapshotBuildError {
    match error {
        RecordScanError::Incomplete(_) => SnapshotOpenError::NotDurable.into(),
        RecordScanError::Read(error) => SnapshotBuildError::Record(error),
        RecordScanError::Visitor(error) => error,
    }
}

fn collect_records<'a, F: sage_db_store::fs::FileSystem>(
    store: &'a Store<F>,
    ledger: &mut dyn SharedLedger,
) -> Result<RecordSlots<'a>, SnapshotBuildError> {
    let mut counts = [0usize; 4];
    store
        .scan_identified_records(ledger, |record, _| {
            let family = match record {
                FoldedRecord::Term(_) => Some(0),
                FoldedRecord::Assertion(_) => Some(1),
                FoldedRecord::Claim(_) => Some(2),
                FoldedRecord::SessionCheckpoint(_) => Some(3),
                _ => None,
            };
            if let Some(i) = family {
                counts[i] = counts[i]
                    .checked_add(1)
                    .ok_or(SnapshotBuildError::ExtentOverflow)?;
            }
            Ok(())
        })
        .map_err(map_scan)?;
    let mut records = RecordSlots {
        terms: admitted_vector(counts[0], ledger)?,
        assertions: admitted_vector(counts[1], ledger)?,
        claims: admitted_vector(counts[2], ledger)?,
        checkpoints: admitted_vector(counts[3], ledger)?,
    };
    store
        .scan_identified_records(ledger, |record: &'a FoldedRecord, _| {
            match record {
                FoldedRecord::Term(value) => push_counted(&mut records.terms, value)?,
                FoldedRecord::Assertion(value) => push_counted(&mut records.assertions, value)?,
                FoldedRecord::Claim(value) => push_counted(&mut records.claims, value)?,
                FoldedRecord::SessionCheckpoint(value) => {
                    push_counted(&mut records.checkpoints, value)?
                }
                _ => {}
            }
            Ok(())
        })
        .map_err(map_scan)?;
    if [
        records.terms.len(),
        records.assertions.len(),
        records.claims.len(),
        records.checkpoints.len(),
    ] != counts
    {
        return Err(SnapshotBuildError::CountMismatch);
    }
    Ok(records)
}

fn push_counted<T>(slots: &mut Vec<T>, value: T) -> Result<(), SnapshotBuildError> {
    if slots.len() == slots.capacity() {
        return Err(SnapshotBuildError::CountMismatch);
    }
    slots.push(value);
    Ok(())
}

/// A graph plus the one lease that keeps everything it needs admitted.
struct OwnedGraph {
    graph: BaseGraph,
    lease: OwnedReservation,
}

impl OwnedGraph {
    fn graph(&self) -> &BaseGraph {
        &self.graph
    }

    fn retained_bytes(&self) -> u64 {
        self.lease.bytes()
    }
}

impl core::fmt::Debug for OwnedGraph {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OwnedGraph")
            .field("terms", &self.graph.term_count())
            .field("assertions", &self.graph.assertion_count())
            .field("retained_bytes", &self.lease.bytes())
            .finish()
    }
}

/// A read-only snapshot over one immutable, durable Store publication.
///
/// The `'a` lifetime is the Store borrow. While this value exists the store cannot commit or
/// be dropped, so every answer is from one version — enforced by the compiler rather than by a
/// convention.
pub struct Snapshot<'a, F: sage_db_store::fs::FileSystem> {
    store: &'a Store<F>,
    basis: SnapshotId,
    rules: CommittedRules,
    records: RecordSlots<'a>,
    graph: OwnedGraph,
}

impl<'a, F: sage_db_store::fs::FileSystem> core::fmt::Debug for Snapshot<'a, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.basis.version)
            .field("records", &self.records.total())
            .field("graph", &self.graph)
            .finish()
    }
}

/// Which publication a caller will accept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Expectation {
    /// The KB the caller requires, when it names one.
    pub kb: Option<KbUuid>,
    /// The exact version the caller requires, when it names one.
    pub version: Option<KbVersion>,
}

impl Expectation {
    /// Accept whatever durable publication the store holds.
    #[must_use]
    pub const fn latest() -> Self {
        Self {
            kb: None,
            version: None,
        }
    }

    /// Accept one specific KB and version.
    #[must_use]
    pub const fn at(kb: KbUuid, version: KbVersion) -> Self {
        Self {
            kb: Some(kb),
            version: Some(version),
        }
    }
}

impl<'a, F: sage_db_store::fs::FileSystem> Snapshot<'a, F> {
    /// Build a view over the store's current durable publication.
    ///
    /// `&mut Store` is required for construction only, because loading the committed rule
    /// bytes is a real blob read (`read_committed_blob`, which verifies length and hash); the
    /// resulting view then borrows the store immutably for its whole life.
    ///
    /// # Errors
    ///
    /// [`SnapshotBuildError`]: not durable, an unfinished import, the wrong publication, rule
    /// bytes that fail canonical verification, a graph that fails its own validation, or the
    /// caller's ledger refusing.
    pub fn open(
        store: &'a mut Store<F>,
        expectation: Expectation,
        ledger: &mut AttemptLedger<'_>,
    ) -> Result<Self, SnapshotBuildError> {
        // 1. One durable publication.
        let (basis_view, published) = {
            let view = store.view().ok_or(SnapshotOpenError::NotDurable)?;
            if view.durability() != ViewDurability::Durable {
                return Err(SnapshotOpenError::NotDurable.into());
            }
            (view.basis(), *view.snapshot().id())
        };
        if let Some(expected) = expectation.kb
            && expected != basis_view.kb_uuid
        {
            return Err(SnapshotOpenError::KbMismatch {
                expected,
                found: basis_view.kb_uuid,
            }
            .into());
        }
        if let Some(expected) = expectation.version
            && expected != basis_view.version
        {
            return Err(SnapshotOpenError::VersionMismatch {
                expected,
                found: basis_view.version,
            }
            .into());
        }

        // 2. An unfinished import is not a serving basis. Checked before anything else is
        //    read, so no view exists over a partially imported record set.
        match store.import_readiness(ledger) {
            Ok(ImportReadiness::Ready { .. }) => {}
            Ok(ImportReadiness::Incomplete { .. }) => {
                return Err(SnapshotOpenError::ImportIncomplete.into());
            }
            Err(error) => return Err(SnapshotBuildError::Metadata(error)),
        }

        // 3. The committed rules: scalar binding from the retained singleton, then the REAL
        //    bytes through `read_committed_blob` and the canonical decoder. A profile the
        //    build believes in is not authority; these bytes are.
        let commit = store
            .commit_basis(sage_db_types::persist::IdSpace::TermId, ledger)
            .map_err(SnapshotBuildError::Metadata)?;
        let declared = typed::DeclaredRules {
            rules_ref: commit.rules.rules_ref,
            rules_digest: commit.rules.rules_digest,
            profile: commit.rules.profile,
        };
        let blob = store
            .read_committed_blob(commit.rules.rules_ref, commit.rules_len, ledger)
            .map_err(SnapshotBuildError::Blob)?;
        let verified = match typed::verify_rules(blob.as_bytes(), &declared, &Limits::V1, ledger) {
            FormatOutcome::Ok(verified) => verified,
            FormatOutcome::Format(error) => return Err(SnapshotOpenError::Rules(error).into()),
            FormatOutcome::Refused(refusal) => return Err(refused(refusal)),
        };
        let rules = CommittedRules {
            profile: verified.profile,
            rules_digest: sage_db_types::digest::RulesDigest::new(Digest::from_bytes(
                verified.rules_digest,
            )),
            rules_ref: Digest::from_bytes(verified.rules_ref),
        };

        // The pin and the verified bytes must describe the same configuration.
        if published.rules.digest() != rules.rules_digest
            || published.rules.profile() != rules.profile
        {
            return Err(SnapshotBuildError::RulesBindingMismatch);
        }
        // The rules blob's own reservation is released here: the verified profile and digests
        // are copied out, and the bytes are not kept by the view.
        drop(blob);

        // Count, admit, allocate, then fill without growth under one immutable publication.
        let records = collect_records(store, ledger)?;
        let GraphInputs {
            mut terms,
            mut assertions,
        } = project_graph_inputs(&records, ledger)?;
        // Canonical graph identity orders by numeric identity. Sorting is in-place and bounded
        // by admitted inputs; std sort comparisons are a documented non-interruptible phase.
        build_step(ledger)?;
        terms.sort_unstable_by_key(|term| term.id);
        assertions.sort_unstable_by_key(|assertion| assertion.id);
        let actual = typed::graph_digest(&terms, &assertions, ledger)
            .map_err(SnapshotBuildError::GraphDigest)?;
        let expected = *published.graph_digest.as_digest().as_bytes();
        if actual != expected {
            return Err(SnapshotBuildError::GraphDigestMismatch { expected, actual });
        }
        let graph = BaseGraph::index_admitted_inputs(published, terms, assertions, ledger)
            .map_err(|error| SnapshotBuildError::View(SnapshotOpenError::Graph(error)))?;
        let total = records
            .slot_bytes()?
            .checked_add(graph.admitted_bound_bytes())
            .ok_or(SnapshotBuildError::ExtentOverflow)?;
        let lease = ledger.adopt_retained(total).map_err(|error| {
            let sage_db_store::lease::TransferError::Untransferred {
                available_bytes, ..
            } = error;
            SnapshotBuildError::View(SnapshotOpenError::TransferShortfall {
                requested: total,
                available: available_bytes,
            })
        })?;

        Ok(Self {
            store,
            basis: graph.snapshot(),
            rules,
            records,
            graph: OwnedGraph { graph, lease },
        })
    }

    /// Check graph rules against this exact admitted publication.
    ///
    /// # Errors
    ///
    /// Propagates the native validation work limit and control stop.
    pub fn validate_graph(
        &self,
        budget: sage_db_types::Budget,
        control: &dyn sage_db_types::QueryControl,
    ) -> Result<ValidationReport, sage_db_types::QueryError> {
        sage_db_infer::validate_candidate(self.graph(), budget, control)
    }

    /// Bytes this view keeps reserved for its whole life.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.graph.retained_bytes()
    }

    /// The store this view reads. The borrow is what keeps the version fixed.
    #[must_use]
    pub const fn store(&self) -> &'a Store<F> {
        self.store
    }

    /// The rule profile the committed bytes declare.
    #[must_use]
    pub const fn committed_profile(&self) -> sage_db_types::relation::RuleProfile {
        self.rules.profile
    }

    /// Counts of the records this view serves, by family.
    #[must_use]
    pub fn counts(&self) -> FamilyCounts {
        FamilyCounts {
            terms: self.records.terms.len(),
            assertions: self.records.assertions.len(),
            claims: self.records.claims.len(),
            checkpoints: self.records.checkpoints.len(),
        }
    }
}

/// How many records of each family a view holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FamilyCounts {
    /// Terms.
    pub terms: usize,
    /// Authored assertions.
    pub assertions: usize,
    /// Claims.
    pub claims: usize,
    /// Session checkpoints.
    pub checkpoints: usize,
}

/// Scalars projected from the current active record set, admitted before allocation.
struct GraphInputs {
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
}

fn project_graph_inputs(
    records: &RecordSlots<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<GraphInputs, SnapshotBuildError> {
    let mut term_count = 0usize;
    for term in &records.terms {
        build_step(ledger)?;
        if !term.retired {
            term_count = term_count
                .checked_add(1)
                .ok_or(SnapshotBuildError::ExtentOverflow)?;
        }
    }
    let mut assertion_count = 0usize;
    for assertion in &records.assertions {
        build_step(ledger)?;
        if !assertion.retracted {
            assertion_count = assertion_count
                .checked_add(1)
                .ok_or(SnapshotBuildError::ExtentOverflow)?;
        }
    }
    let mut terms = admitted_vector(term_count, ledger)?;
    let mut assertions = admitted_vector(assertion_count, ledger)?;
    for term in &records.terms {
        build_step(ledger)?;
        if !term.retired {
            let id = term_id_of(term.key).ok_or(SnapshotBuildError::InvalidRecord(term.key))?;
            push_counted(
                &mut terms,
                TermMeta {
                    id,
                    layer: term.layer,
                },
            )?;
        }
    }
    for assertion in &records.assertions {
        build_step(ledger)?;
        if assertion.retracted {
            continue;
        }
        if assertion.key.kind() != RecordKind::Assertion || !assertion.key.id_is_in_range() {
            return Err(SnapshotBuildError::InvalidRecord(assertion.key));
        }
        let from =
            term_id_of(assertion.from).ok_or(SnapshotBuildError::InvalidRecord(assertion.from))?;
        let target = term_id_of(assertion.target)
            .ok_or(SnapshotBuildError::InvalidRecord(assertion.target))?;
        // Active assertions with absent/retired endpoints remain in the projection. The shared
        // BaseGraph validator rejects UnknownTerm; they are never silently discarded here.
        push_counted(
            &mut assertions,
            AssertionRecord {
                id: AssertionId::new(assertion.key.id()),
                revision: assertion.revision,
                from,
                relation: assertion.relation,
                target,
                confidence: assertion.confidence,
            },
        )?;
    }
    build_step(ledger)?;
    Ok(GraphInputs { terms, assertions })
}

/// The term id behind a record key, when the key really names a term.
fn term_id_of(key: RecordKey) -> Option<TermId> {
    if key.kind() != RecordKind::Term {
        return None;
    }
    u32::try_from(key.id()).ok().map(TermId::new)
}

impl<F: sage_db_store::fs::FileSystem> SnapshotView for Snapshot<'_, F> {
    fn basis(&self) -> SnapshotId {
        self.basis
    }

    fn rules(&self) -> CommittedRules {
        self.rules
    }

    fn graph(&self) -> &BaseGraph {
        self.graph.graph()
    }

    fn record(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<RecordView<'_>, ReadRefusal> {
        ledger
            .checkpoint()
            .map_err(|stop| ReadRefusal::Refused(Refusal::Stopped(stop)))?;
        // A key whose id does not fit its kind's bound is malformed: a caller error, never a
        // statement about stored data.
        if !key.id_is_in_range() {
            return Err(ReadRefusal::InvalidKey(key));
        }
        let found = match key.kind() {
            RecordKind::Term => {
                find_slot(&self.records.terms, key, ledger)?.map(BorrowedRecord::Term)
            }
            RecordKind::Assertion => {
                find_slot(&self.records.assertions, key, ledger)?.map(BorrowedRecord::Assertion)
            }
            RecordKind::Claim => {
                find_slot(&self.records.claims, key, ledger)?.map(BorrowedRecord::Claim)
            }
            RecordKind::SessionCheckpoint => find_slot(&self.records.checkpoints, key, ledger)?
                .map(BorrowedRecord::SessionCheckpoint),
            // Every other family is committed but not interpreted here. Saying `Absent` would
            // claim the record is gone; the honest answer is that this view does not read it.
            other => return Err(ReadRefusal::UnsupportedFamily(other)),
        };
        match found {
            Some(record) => Ok(RecordView::Found(record.as_ref())),
            None => Ok(RecordView::Absent { key }),
        }
    }

    fn scan_family<'b, E>(
        &'b self,
        family: SnapshotFamily,
        ledger: &mut dyn SharedLedger,
        visitor: &mut dyn FnMut(RecordRef<'b>) -> Result<(), E>,
    ) -> Result<(), ScanError<E>> {
        // Control before the walk, so an empty family is not "success" for a stopped caller.
        ledger
            .checkpoint()
            .map_err(|stop| ScanError::Refused(ReadRefusal::Refused(Refusal::Stopped(stop))))?;
        match family {
            SnapshotFamily::Term => {
                for term in &self.records.terms {
                    step(ledger)?;
                    visitor(RecordRef::Term(term)).map_err(ScanError::Visitor)?;
                }
            }
            SnapshotFamily::Assertion => {
                for assertion in &self.records.assertions {
                    step(ledger)?;
                    visitor(RecordRef::Assertion(assertion)).map_err(ScanError::Visitor)?;
                }
            }
            SnapshotFamily::Claim => {
                for claim in &self.records.claims {
                    step(ledger)?;
                    visitor(RecordRef::Claim(claim)).map_err(ScanError::Visitor)?;
                }
            }
            SnapshotFamily::SessionCheckpoint => {
                for checkpoint in &self.records.checkpoints {
                    step(ledger)?;
                    visitor(RecordRef::SessionCheckpoint(checkpoint))
                        .map_err(ScanError::Visitor)?;
                }
            }
        }
        // One more control point: a caller that stopped during the last record must not be
        // told the walk completed.
        ledger
            .checkpoint()
            .map_err(|stop| ScanError::Refused(ReadRefusal::Refused(Refusal::Stopped(stop))))?;
        Ok(())
    }
}

/// The record key of a borrowed slot, for the four families in scope.
trait SlotRecord {
    fn slot_key(&self) -> RecordKey;
}

impl SlotRecord for TermRecord {
    fn slot_key(&self) -> RecordKey {
        self.key
    }
}
impl SlotRecord for AssertionRecordFull {
    fn slot_key(&self) -> RecordKey {
        self.key
    }
}
impl SlotRecord for ClaimRecord {
    fn slot_key(&self) -> RecordKey {
        self.key
    }
}
impl SlotRecord for SessionCheckpointRecord {
    fn slot_key(&self) -> RecordKey {
        self.key
    }
}

/// A ledger-checked linear search over borrowed slots.
///
/// Each comparison charges one entry and consults control, so a point lookup is bounded like
/// a walk rather than a free scan whose cost is invisible to the caller's allowance.
fn find_slot<'a, T: SlotRecord>(
    slots: &'a [&'a T],
    key: RecordKey,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<&'a T>, ReadRefusal> {
    for slot in slots {
        ledger
            .checkpoint()
            .map_err(|stop| ReadRefusal::Refused(Refusal::Stopped(stop)))?;
        ledger
            .try_charge_entries(1)
            .map_err(|error| ReadRefusal::Refused(Refusal::Budget(error)))?;
        if slot.slot_key() == key {
            return Ok(Some(*slot));
        }
    }
    Ok(None)
}

/// Per-record admission and control for a scan.
fn step<E>(ledger: &mut dyn SharedLedger) -> Result<(), ScanError<E>> {
    ledger
        .checkpoint()
        .map_err(|stop| ScanError::Refused(ReadRefusal::Refused(Refusal::Stopped(stop))))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| ScanError::Refused(ReadRefusal::Refused(Refusal::Budget(error))))
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use sage_db_types::ledger::BudgetUnit;
    use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

    #[test]
    fn slot_bytes_are_admitted_before_any_vector_is_returned() {
        let requested = 3 * core::mem::size_of::<TermMeta>() as u64;
        let mut short = AttemptLedger::open(requested - 1, 100, &RunToCompletion).unwrap();
        match admitted_vector::<TermMeta>(3, &mut short) {
            Err(SnapshotBuildError::View(SnapshotOpenError::Refused(Refusal::Budget(error)))) => {
                assert_eq!(error.unit(), BudgetUnit::Bytes);
                assert_eq!(error.requested(), requested);
                assert_eq!(error.remaining(), requested - 1);
            }
            other => panic!("expected the real byte admission refusal: {other:?}"),
        }
        assert_eq!(short.stats().live_bytes, 0);
        let mut exact = AttemptLedger::open(requested, 100, &RunToCompletion).unwrap();
        let vector = admitted_vector::<TermMeta>(3, &mut exact).unwrap();
        assert_eq!(vector.capacity(), 3);
        assert_eq!(exact.stats().live_bytes, requested);
    }

    struct Stopped;
    impl QueryControl for Stopped {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    #[test]
    fn even_empty_reservations_consult_caller_control() {
        let mut ledger = AttemptLedger::open(100, 100, &Stopped).unwrap();
        assert!(matches!(
            admitted_vector::<TermMeta>(0, &mut ledger),
            Err(SnapshotBuildError::View(SnapshotOpenError::Refused(
                Refusal::Stopped(ControlStop::Cancelled)
            )))
        ));
        assert_eq!(ledger.stats().live_bytes, 0);
    }
}
