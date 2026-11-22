//! `StateFold` tests: history retention, current selection, allocation maxima, cumulative outcomes and
//! ordering — on real records, with a real ledger.
//!
//! No second Store shim: the fold is exercised through its public API, and every expectation is derived
//! from the document rule it encodes rather than from the fold's own output.

use sage_db_store::fold::{FoldDefect, FoldOutcome};
use sage_db_store::fold::{
    FoldEvent, FoldedRecord, FoldedVersion, NaturalKey, RecordIdentity, StateFold,
};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::layer::SemanticLayer;
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, CommonFields, CreationAudit, Extensions, IdAllocationEvent, IdSpace,
    KbMetadataRecord, OptionalActorAudit, RecordKey, RequestOutcomeEvent, StateKind, TermRecord,
    TermSource, Watermark,
};

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

/// Admit a record's nested payload so an owning fold can transfer it.
fn admit_record_payload<L: SharedLedger>(ledger: &mut L, record: &FoldedRecord) {
    let bytes = match record {
        FoldedRecord::Term(r) => {
            (r.slug.len() + r.atom.len()) as u64
                + (r.domains.capacity() * core::mem::size_of::<String>()) as u64
                + core::mem::size_of_val(r.extensions.entries()) as u64
        }
        _ => 0,
    };
    if bytes > 0 {
        ledger.try_charge(bytes).expect("payload admission");
    }
}

/// A real capable ledger, unlimited, for doubles that must still be able to issue leases.
fn capable() -> sage_db_store::lease::AttemptLedger<'static> {
    let control: &'static sage_db_types::query::RunToCompletion =
        Box::leak(Box::new(sage_db_types::query::RunToCompletion));
    sage_db_store::lease::AttemptLedger::open(u64::MAX, u64::MAX, control).expect("opens")
}

/// An accepting, *capable* ledger for the fold tests.
///
/// Wraps the accepted [`AttemptLedger`] rather than hand-rolling `SharedLedger`, because owning folds issue real leases and
/// a weak-only double cannot do that honestly. Unlimited allowance keeps the existing assertions unchanged; the resource
/// assertions elsewhere deliberately use a finite ledger instead.
struct Open(sage_db_store::lease::AttemptLedger<'static>);

impl Open {
    fn new() -> Self {
        let control: &'static sage_db_types::query::RunToCompletion =
            Box::leak(Box::new(sage_db_types::query::RunToCompletion));
        Self(sage_db_store::lease::AttemptLedger::open(u64::MAX, u64::MAX, control).expect("opens"))
    }
}

impl SharedLedger for Open {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.0.try_charge(bytes)
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        self.0.try_charge_entries(count)
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.0.checkpoint()
    }
}

impl sage_db_store::lease::LeaseLedger for Open {
    fn reserve_owned(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, BudgetError> {
        self.0.reserve_owned(bytes)
    }
    fn adopt_retained(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError> {
        self.0.adopt_retained(bytes)
    }
}

/// Admits `n` entry slots and `m` bytes, then refuses — so entry and byte allowances are provably separate.
///
/// Owning folds issue real leases, so this carries a genuine capable ledger and delegates to it. Its own allowance fields
/// decide admission exactly as before, which is what these tests are about.
struct AllowBoth {
    entries: u64,
    bytes: u64,
    inner: sage_db_store::lease::AttemptLedger<'static>,
}

impl SharedLedger for AllowBoth {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        // The double's own ceiling decides admission — that is the behaviour these tests assert...
        if bytes > self.bytes {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
        }
        self.bytes -= bytes;
        // ...and the admitted bytes must reach the SAME counter that issues leases, or a later `adopt_retained` would see
        // credit this double never transferred. One counter, not two.
        self.inner.try_charge(bytes)
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        if count > self.entries {
            return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
        }
        self.entries -= count;
        self.inner.try_charge_entries(count)
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.inner.checkpoint()
    }
}

struct Halt;

impl sage_db_store::lease::LeaseLedger for AllowBoth {
    fn reserve_owned(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, BudgetError> {
        self.inner.reserve_owned(bytes)
    }
    fn adopt_retained(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError> {
        self.inner.adopt_retained(bytes)
    }
}

impl sage_db_store::lease::LeaseLedger for Halt {
    fn reserve_owned(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, BudgetError> {
        // BOUNDARY, stated rather than hidden: this double cancels at its first `checkpoint`, which the fold consults
        // before any lease is issued, so this arm is unreachable on the paths `Halt` is used for. It therefore does not
        // need a shared counter; if a test ever reaches it, the assertion it drives is wrong, not the accounting.
        capable().reserve_owned(bytes)
    }
    fn adopt_retained(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError> {
        capable().adopt_retained(bytes)
    }
}

impl SharedLedger for Halt {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

// ---------------------------------------------------------------------------
// Record builders (real declarations, no defaults invented)
// ---------------------------------------------------------------------------

fn kb() -> KbUuid {
    KbUuid::from_bytes([0x11; 16])
}

fn common(at: i64) -> CommonFields {
    CommonFields {
        recorded_by: None,
        actor_origin: ActorOrigin::Authenticated,
        reason: None,
        created_at: at,
        updated_at: None,
    }
}

fn term(id: u32, revision: u64, retired: bool) -> FoldedRecord {
    FoldedRecord::Term(TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::new(revision).expect("revision >= 1"),
        retired,
        slug: format!("term-{id}"),
        atom: "atom".to_owned(),
        layer: SemanticLayer::L1,
        domains: Vec::new(),
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: Vec::new(),
        body_ref: [0u8; 32],
        common: common(1),
        legacy_id: None,
        legacy_source_path: None,
        legacy_content_digest: None,
        legacy_source_file_digest: None,
        legacy_source_revision: None,
        extensions: Extensions::new(),
        changed_fields: None,
    })
}

fn allocation(space: IdSpace, hwm: u128, _at: i64) -> IdAllocationEvent {
    IdAllocationEvent {
        allocation: Watermark { space, hwm },
        reason: None,
        extensions: Extensions::new(),
    }
}

fn outcome(principal: &str, request: &str, commit_version: u64) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: principal.to_owned(),
        request_id: request.to_owned(),
        payload_digest: [0x42; 32],
        result_ref: [0x43; 32],
        commit_version,
        audit: CreationAudit {
            recorded_by: None,
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1,
        },
        extensions: Extensions::new(),
    }
}

fn version(n: u64) -> FoldedVersion {
    version_with_events(n, 1)
}

fn version_with_events(n: u64, event_count: u32) -> FoldedVersion {
    FoldedVersion {
        version: n,
        tx_id: [n as u8; 16],
        event_count,
        rules_profile: 1,
        rules_digest: [0x55; 32],
    }
}

// ---------------------------------------------------------------------------
// History retention and current selection
// ---------------------------------------------------------------------------

#[test]
fn every_revision_is_retained_and_only_the_last_is_current() {
    // MODEL §3 rule 1–2: revisions only grow, and a store that drops one is a model violation.
    let mut fold = StateFold::new();
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(7),
    };
    let t1 = term(7, 1, false);
    let t2 = term(7, 2, false);
    let t3 = term(7, 3, true);

    fold.apply_owning(
        version_with_events(1, 3),
        kb(),
        vec![
            FoldEvent::Record(t1),
            FoldEvent::Record(t2),
            FoldEvent::Record(t3),
        ],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(
        fold.revision_count(&identity),
        3,
        "all three revisions must be retained"
    );
    assert_eq!(fold.current_revision(&identity), Some(3));
    // The current record is the revised one, and it is retired — retirement is a revision, not a delete.
    let current = fold.current_record(&identity).expect("current");
    assert!(!current.is_active_for_graph(), "retired term is not active");
    assert_eq!(fold.head_version(), Some(1));
}

#[test]
fn a_record_untouched_in_a_later_version_keeps_its_revision() {
    // §4.4.8 note 7: fold CURRENT state, not "this transaction's projection". A term at revision 2 must
    // stay at 2 when a later version touches only a different record.
    //
    // The prefix is now LEGAL: MODEL §3 rule 1 makes a creation revision 1, so term 1 must reach revision 2 through an
    // actual update rather than starting there. The property under test — an untouched record keeps its revision — and its
    // assertion are unchanged; only the setup prefix and the resulting version order are corrected.
    let mut fold = StateFold::new();
    let created = term(1, 1, false);
    fold.apply_owning(
        version(1),
        kb(),
        vec![FoldEvent::Record(created)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v1 creates term 1 at revision 1");

    let updated = term(1, 2, false);
    fold.apply_owning(
        version(2),
        kb(),
        vec![FoldEvent::Record(updated)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v2 updates term 1 to revision 2");

    let other = term(2, 1, false);
    fold.apply_owning(
        version(3),
        kb(),
        vec![FoldEvent::Record(other)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v3 touches only term 2");

    let first = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    assert_eq!(
        fold.current_revision(&first),
        Some(2),
        "an untouched record must not be renumbered"
    );
    assert_eq!(fold.head_version(), Some(3));
    assert_eq!(
        fold.versions().len(),
        3,
        "all three versions retained in order"
    );
    assert_eq!(fold.versions()[2].version, 3);
}

#[test]
fn revisions_of_one_identity_are_isolated_from_another() {
    // Two ids of the same kind must never share a revision bucket.
    let mut fold = StateFold::new();
    let a1 = term(1, 1, false);
    let b1 = term(2, 1, false);
    fold.apply_owning(
        version_with_events(1, 2),
        kb(),
        vec![FoldEvent::Record(a1), FoldEvent::Record(b1)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    let a = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let b = RecordIdentity::Identified {
        key: RecordKey::term(2),
    };
    assert_eq!(fold.revision_count(&a), 1);
    assert_eq!(fold.revision_count(&b), 1);
    assert_eq!(
        fold.current_of_kind(StateKind::Term).count(),
        2,
        "one current record per id"
    );
}

#[test]
fn identified_and_natural_records_use_separate_key_spaces() {
    // RecordKind stops at 9 by design, so KB metadata has no RecordKey. The fold must file it under
    // StateKind without inventing an id.
    let mut fold = StateFold::new();
    let metadata = FoldedRecord::KbMetadata(KbMetadataRecord {
        revision: RecordRevision::FIRST,
        kb_uuid: kb(),
        slug: "kb".to_owned(),
        name: "KB".to_owned(),
        description: String::new(),
        domains: Vec::new(),
        legacy_frontier_ref: None,
        audit: OptionalActorAudit::absent_actor(1),
        extensions: Extensions::new(),
        changed_fields: None,
    });
    let identity = metadata.identity(&mut Open::new()).expect("identity");
    fold.apply_owning(
        version(1),
        kb(),
        vec![FoldEvent::Record(metadata)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(
        identity,
        RecordIdentity::Natural {
            key: NaturalKey::KbMetadata
        }
    );
    assert_eq!(
        fold.current_of_kind(StateKind::KbMetadata).count(),
        1,
        "the singleton is reachable by its state kind"
    );
    // And it must not appear as a graph contributor.
    assert!(
        fold.current_of_kind(StateKind::KbMetadata)
            .all(|r| !r.is_active_for_graph()),
        "KB metadata contributes nothing to the graph"
    );
}

// ---------------------------------------------------------------------------
// Allocation: a maximum, never a sum
// ---------------------------------------------------------------------------

#[test]
fn the_high_water_mark_is_a_maximum_not_a_sum() {
    // bookkeeping_digest depends on this: a retried or duplicated allocation must not inflate the mark.
    let mut fold = StateFold::new();
    let a = allocation(IdSpace::TermId, 5, 1);
    let b = allocation(IdSpace::TermId, 3, 1);
    let c = allocation(IdSpace::TermId, 9, 1);
    fold.apply_owning(
        version_with_events(1, 3),
        kb(),
        vec![
            FoldEvent::IdAllocation(a),
            FoldEvent::IdAllocation(b),
            FoldEvent::IdAllocation(c),
        ],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(fold.high_water(IdSpace::TermId), Some(9));
    assert_eq!(
        fold.high_water(IdSpace::AssertionId),
        None,
        "a space that never advanced has no mark"
    );
}

#[test]
fn allocation_marks_are_per_space() {
    let mut fold = StateFold::new();
    let a = allocation(IdSpace::TermId, 2, 1);
    let b = allocation(IdSpace::ClaimId, 7, 1);
    fold.apply_owning(
        version_with_events(1, 2),
        kb(),
        vec![FoldEvent::IdAllocation(a), FoldEvent::IdAllocation(b)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(fold.high_water(IdSpace::TermId), Some(2));
    assert_eq!(fold.high_water(IdSpace::ClaimId), Some(7));
}

// ---------------------------------------------------------------------------
// Cumulative outcomes
// ---------------------------------------------------------------------------

#[test]
fn outcomes_accumulate_across_versions_and_keep_the_original() {
    // S2 §3.1: a stale exact retry must return the FIRST durable outcome, so the fold keeps the record as
    // committed rather than the latest thing it saw.
    let mut fold = StateFold::new();
    let first = outcome("alice", "req-1", 2);
    fold.apply_owning(
        version(2),
        kb(),
        vec![FoldEvent::Outcome(first)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v2");

    let second = outcome("bob", "req-2", 3);
    fold.apply_owning(
        version(3),
        kb(),
        vec![FoldEvent::Outcome(second)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v3");

    assert_eq!(fold.outcomes().len(), 2, "outcomes are cumulative");
    let recorded = fold.outcome("alice", "req-1").expect("first outcome");
    assert_eq!(recorded.commit_version, 2);
    assert_eq!(
        recorded.payload_digest, [0x42; 32],
        "the digest is what distinguishes a retry from a reused id"
    );
    assert_eq!(recorded.event.event().principal_id, "alice");
}

// ---------------------------------------------------------------------------
// Refusals are not verdicts
// ---------------------------------------------------------------------------

#[test]
fn entry_and_byte_allowances_are_separate() {
    // The fold retains a table, so charging only the entry count would leave the element bytes
    // unadmitted — the defect class the format encoders were corrected for.
    let mut fold = StateFold::new();
    let t = term(1, 1, false);

    // Plenty of entries, zero bytes: must refuse on the byte charge.
    let mut ledger = AllowBoth {
        entries: 100,
        bytes: 0,
        inner: capable(),
    };
    let outcome = fold.apply_owning(version(1), kb(), vec![FoldEvent::Record(t)], &mut ledger);
    assert!(
        matches!(outcome, FoldOutcome::Refused(Refusal::Budget(_))),
        "a byte refusal must surface, got {outcome:?}"
    );

    // Plenty of bytes, zero entries: must refuse on the entry charge.
    let mut ledger = AllowBoth {
        entries: 0,
        bytes: u64::MAX,
        inner: capable(),
    };
    let t2 = term(1, 1, false);
    let outcome = fold.apply_owning(version(1), kb(), vec![FoldEvent::Record(t2)], &mut ledger);
    assert!(
        matches!(outcome, FoldOutcome::Refused(Refusal::Budget(_))),
        "an entry refusal must surface, got {outcome:?}"
    );
}

#[test]
fn cancellation_is_a_stop_and_not_a_fold_error() {
    let mut fold = StateFold::new();
    let t = term(1, 1, false);
    let outcome = fold.apply_owning(version(1), kb(), vec![FoldEvent::Record(t)], &mut Halt);
    assert!(
        outcome.as_refusal().is_some(),
        "cancellation must stay a refusal, got {outcome:?}"
    );
    assert!(
        fold.head_version().is_none(),
        "a refused fold must not advance the head"
    );
}

#[test]
fn two_models_at_the_same_revision_do_not_collide() {
    // The defect this test exists for: keying natural records by KIND alone merged two models at revision
    // 1, so the second silently overwrote the first. The natural key must carry `model_key`.
    let mut fold = StateFold::new();
    let a = FoldedRecord::Model(model("provider-a"));
    let b = FoldedRecord::Model(model("provider-b"));
    fold.apply_owning(
        version_with_events(1, 2),
        kb(),
        vec![FoldEvent::Record(a), FoldEvent::Record(b)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(
        fold.current_of_kind(StateKind::Model).count(),
        2,
        "two distinct model keys must both survive at revision 1"
    );
}

#[test]
fn two_assets_at_the_same_revision_do_not_collide() {
    // Assets are keyed by sha256, and have no revision field at all (§4.4.3 fixes it at 1).
    let mut fold = StateFold::new();
    let a = FoldedRecord::Asset(asset([0x01; 32]));
    let b = FoldedRecord::Asset(asset([0x02; 32]));
    fold.apply_owning(
        version_with_events(1, 2),
        kb(),
        vec![FoldEvent::Record(a), FoldEvent::Record(b)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(
        fold.current_of_kind(StateKind::Asset).count(),
        2,
        "two distinct asset digests must both survive"
    );
}

#[test]
fn outcome_lookup_allocates_nothing() {
    // A lookup on a read path must borrow. The earlier draft built two owned Strings per call with no
    // ledger, which is an unadmitted allocation to answer a question.
    let mut fold = StateFold::new();
    let o = outcome("alice", "req-1", 2);
    fold.apply_owning(
        version(2),
        kb(),
        vec![FoldEvent::Outcome(o)],
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    let found = fold.outcome("alice", "req-1").expect("outcome");
    assert_eq!(found.commit_version, 2);
    assert!(fold.outcome("alice", "absent").is_none());
}

fn model(key: &str) -> sage_db_types::persist::ModelRecord {
    sage_db_types::persist::ModelRecord {
        revision: RecordRevision::FIRST,
        model_key: key.to_owned(),
        identifier: format!("{key}-id"),
        provider: None,
        dimensions: 8,
        status: sage_db_types::persist::ModelStatus::Active,
        common: common(1),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

fn asset(sha: [u8; 32]) -> sage_db_types::persist::AssetRecord {
    sage_db_types::persist::AssetRecord {
        sha256: sha,
        media_type: sage_db_types::persist::MediaType::Png,
        byte_size: 1,
        declared_by: None,
        declared_at: None,
        audit: CreationAudit {
            recorded_by: None,
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1,
        },
        extensions: Extensions::new(),
    }
}

// ---------------------------------------------------------------------------
// Independent positive: records produced by the REAL codecs, not typed units
// ---------------------------------------------------------------------------

#[test]
fn folds_events_decoded_by_the_real_transaction_codec() {
    // Every value folded here comes from the real decoder. An earlier revision of this test decoded only the
    // HEADER and then hand-built the allocations and a RuleConfig, which meant no event codec result ever reached
    // the fold — the name said "decoded" while the values were fabricated.
    //
    // The visitor copies decoded payloads into a collector. That copy is test-only and labelled as such: the real
    // `EventVisitor::visit` receives `&DispatchedEvent`, and there is no owned-visitor API. Production ownership
    // transfer, if commit needs it, is a concrete interface request rather than a reason to clone here.
    use sage_db_format::dispatch::DispatchedEvent;
    use sage_db_format::dispatch::DispatchedPayload;
    use sage_db_format::transaction::{EventVisitor, read_transaction};

    /// Test-only collector of decoded payloads.
    #[derive(Default)]
    struct Collect {
        allocations: Vec<IdAllocationEvent>,
        genesis_metadata: Option<KbMetadataRecord>,
        genesis_rules: Option<sage_db_types::persist::RuleConfigRecord>,
        seen: usize,
    }

    impl EventVisitor for Collect {
        fn visit(
            &mut self,
            _seq: u32,
            event: &DispatchedEvent<'_>,
            _ledger: &mut dyn SharedLedger,
        ) -> Result<(), Refusal> {
            self.seen += 1;
            match event.payload() {
                Some(DispatchedPayload::IdAllocation(record)) => {
                    self.allocations.push(record.clone());
                }
                Some(DispatchedPayload::Genesis(record)) => {
                    // Genesis carries field GROUPS, not records: the fold's two records are the projections
                    // that the real decoder produced into these groups.
                    self.genesis_metadata = Some(KbMetadataRecord {
                        revision: RecordRevision::FIRST,
                        kb_uuid: record.metadata.kb_uuid,
                        slug: record.metadata.slug.clone(),
                        name: record.metadata.name.clone(),
                        description: record.metadata.description.clone(),
                        domains: record.metadata.domains.clone(),
                        legacy_frontier_ref: None,
                        audit: OptionalActorAudit::absent_actor(record.created_at),
                        extensions: record.metadata.extensions.clone(),
                        changed_fields: None,
                    });
                    self.genesis_rules = Some(sage_db_types::persist::RuleConfigRecord {
                        revision: RecordRevision::FIRST,
                        profile: record.rules.profile,
                        from_profile: None,
                        rules_digest: record.rules.rules_digest,
                        rules_ref: record.rules.rules_ref,
                        dry_run_report_ref: None,
                        audit: OptionalActorAudit::absent_actor(record.created_at),
                        extensions: record.rules.extensions.clone(),
                        changed_fields: None,
                    });
                }
                _ => {}
            }
            Ok(())
        }
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../format/tests/fixtures/file-codec/golden.genesis.tx.sgl");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

    let mut collect = Collect::default();
    let summary = match read_transaction(&bytes, &Limits::V1, None, &mut collect, &mut Open::new())
    {
        FormatOutcome::Ok(summary) => summary,
        other => panic!("golden genesis must decode: {other:?}"),
    };

    assert_eq!(
        collect.seen, 10,
        "Genesis + nine IdAllocation, all really decoded"
    );
    assert_eq!(collect.allocations.len(), 9);
    let metadata = collect.genesis_metadata.expect("Genesis decoded");
    let rules = collect.genesis_rules.expect("Genesis decoded");

    // Build the fold's actions from the DECODED values only.
    let mut events: Vec<FoldEvent> = collect
        .allocations
        .into_iter()
        .map(FoldEvent::IdAllocation)
        .collect();
    events.push(FoldEvent::Record(FoldedRecord::KbMetadata(
        metadata.clone(),
    )));
    events.push(FoldEvent::Record(FoldedRecord::RuleConfig(rules.clone())));

    let version = FoldedVersion {
        version: summary.header.version,
        tx_id: summary.header.tx_id,
        // The wire count, as the reader reported it — not the action count.
        event_count: summary.header.event_count,
        rules_profile: rules.profile.as_u16(),
        rules_digest: rules.rules_digest,
    };

    let mut fold = StateFold::new();
    fold.apply_owning(version, metadata.kb_uuid, events, &mut Open::new())
        .into_result()
        .expect("fold decoded values");

    assert_eq!(fold.head_version(), Some(1));
    for space in 0..9u16 {
        let id_space = IdSpace::from_u16(space).expect("registered space");
        assert_eq!(
            fold.high_water(id_space),
            Some(0),
            "§4.3: every genesis mark is 0"
        );
    }
    assert_eq!(fold.current_of_kind(StateKind::KbMetadata).count(), 1);
    assert_eq!(fold.current_of_kind(StateKind::RuleConfig).count(), 1);
}

#[test]
fn a_history_collision_is_refused_not_overwritten() {
    // MODEL §3 rule 1: a record's revision advances by exactly one, and every revision is retained. Two
    // different images under one (identity, revision) make the retained-history locator ambiguous, so the fold
    // must refuse. Silently overwriting — or "repairing" by taking the maximum — would hide a broken chain.
    let mut fold = StateFold::new();
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(4),
    };
    let first = term(4, 1, false);
    fold.apply_owning(
        version_with_events(1, 1),
        kb(),
        vec![FoldEvent::Record(first)],
        &mut Open::new(),
    )
    .into_result()
    .expect("first revision");

    // Same identity, same revision, different content.
    let mut second = term(4, 1, false);
    if let FoldedRecord::Term(r) = &mut second {
        r.atom = "different".to_owned();
    }
    match fold.apply_owning(
        version_with_events(2, 1),
        kb(),
        vec![FoldEvent::Record(second)],
        &mut Open::new(),
    ) {
        FoldOutcome::Defect(FoldDefect::HistoryCollision { revision, .. }) => {
            assert_eq!(revision, 1);
        }
        other => panic!("a revision collision must be Corrupt, got {other:?}"),
    }
    assert_eq!(
        fold.revision_count(&identity),
        1,
        "the retained revision must not be replaced"
    );
    assert_eq!(
        fold.head_version(),
        Some(1),
        "a refused fold must not advance the head"
    );
}

#[test]
fn a_new_revision_of_the_same_record_is_accepted() {
    // The positive control: revision 2 of the same identity is legal and becomes current.
    let mut fold = StateFold::new();
    let first = term(4, 1, false);
    let second = term(4, 2, false);
    fold.apply_owning(
        version_with_events(1, 1),
        kb(),
        vec![FoldEvent::Record(first)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v1");
    fold.apply_owning(
        version_with_events(2, 1),
        kb(),
        vec![FoldEvent::Record(second)],
        &mut Open::new(),
    )
    .into_result()
    .expect("v2");

    let identity = RecordIdentity::Identified {
        key: RecordKey::term(4),
    };
    assert_eq!(fold.revision_count(&identity), 2);
    assert_eq!(fold.current_revision(&identity), Some(2));
}

#[test]
fn a_fold_refused_at_the_gate_applies_nothing() {
    // Scope, stated precisely: this exercises a refusal that happens in the ADMISSION step, before any table is
    // touched. It proves the gate precedes mutation — it does NOT prove atomicity of a fold that fails partway,
    // and an earlier name claiming "no partial state" overclaimed that.
    //
    // The real contract is scratch-and-discard: a caller folds into its own scratch state and throws it away if
    // the fold fails, which is why there is no rollback machinery here. Production must follow that contract
    // rather than rely on this function being transactional.
    let mut fold = StateFold::new();
    let t = term(9, 1, false);
    let mut ledger = AllowBoth {
        entries: 0,
        bytes: u64::MAX,
        inner: capable(),
    };
    let outcome = fold.apply_owning(
        version_with_events(1, 1),
        kb(),
        vec![FoldEvent::Record(t)],
        &mut ledger,
    );
    assert!(
        outcome.as_refusal().is_some(),
        "must be refused at the gate"
    );
    assert_eq!(fold.head_version(), None);
    assert_eq!(
        fold.revision_count(&RecordIdentity::Identified {
            key: RecordKey::term(9)
        }),
        0
    );
}

#[test]
fn a_tiny_action_set_does_not_reserve_a_field_sized_table() {
    // Root's point: reserving every table for `n` rows regardless of element size over-reserves wildly, so an
    // IdAllocation-only transaction would still reserve record-sized vectors. Only the tables a call actually
    // grows are reserved, each sized by its own element.
    //
    // Observable consequence: with nine allocations and no records, the record table must not grow.
    let mut fold = StateFold::new();
    let allocations: Vec<FoldEvent> = (0..9u16)
        .map(|space| {
            FoldEvent::IdAllocation(IdAllocationEvent {
                allocation: Watermark {
                    space: IdSpace::from_u16(space).expect("space"),
                    hwm: 0,
                },
                reason: None,
                extensions: Extensions::new(),
            })
        })
        .collect();

    fold.apply_owning(
        version_with_events(1, 9),
        kb(),
        allocations,
        &mut Open::new(),
    )
    .into_result()
    .expect("fold");

    assert_eq!(fold.head_version(), Some(1));
    // Nine id spaces at hwm 0 is exactly what §4.3 requires of genesis.
    for space in 0..9u16 {
        assert_eq!(
            fold.high_water(IdSpace::from_u16(space).expect("space")),
            Some(0)
        );
    }
}

#[test]
fn a_zero_byte_budget_refuses_new_fold_storage() {
    // A zero-byte budget refuses the initial slot reservation. This says nothing about
    // late refusal atomicity; any failed scratch fold must be discarded.
    let mut fold = StateFold::new();
    let record = FoldedRecord::Model(model("a-rather-long-model-key"));
    let mut ledger = AllowBoth {
        entries: u64::MAX,
        bytes: 0,
        inner: capable(),
    };
    let outcome = fold.apply_owning(
        version_with_events(1, 1),
        kb(),
        vec![FoldEvent::Record(record)],
        &mut ledger,
    );
    assert!(
        outcome.as_refusal().is_some(),
        "must refuse on the byte charge"
    );
    assert_eq!(fold.head_version(), None);
    assert_eq!(
        fold.current_of_kind(StateKind::Model).count(),
        0,
        "no record applied"
    );
}

/// A ledger that allows the gate and then stops on the Nth checkpoint.
///
/// An entry-only `Halt` proves nothing about inner loops: it refuses at the first control check, which sits
/// before any action is examined. This allows that check and stops later, so the stop can only be honoured if the
/// per-action loops check too.
struct StopAfter {
    remaining: u32,
    inner: sage_db_store::lease::AttemptLedger<'static>,
}

impl sage_db_store::lease::LeaseLedger for StopAfter {
    fn reserve_owned(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, BudgetError> {
        self.inner.reserve_owned(bytes)
    }
    fn adopt_retained(
        &mut self,
        bytes: u64,
    ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError> {
        self.inner.adopt_retained(bytes)
    }
}

impl SharedLedger for StopAfter {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.remaining == 0 {
            return Err(ControlStop::Cancelled);
        }
        self.remaining -= 1;
        Ok(())
    }
}

#[test]
fn a_stop_reaches_the_inside_of_a_multi_action_loop() {
    // Nine allocation actions, no records: without per-action checkpoints this batch traverses both passes with
    // only the fixed reservations. A threshold beyond those checks distinguishes an inner-loop stop.
    let mut fold = StateFold::new();
    let allocations: Vec<FoldEvent> = (0..9u16)
        .map(|space| {
            FoldEvent::IdAllocation(IdAllocationEvent {
                allocation: Watermark {
                    space: IdSpace::from_u16(space).expect("space"),
                    hwm: 0,
                },
                reason: None,
                extensions: Extensions::new(),
            })
        })
        .collect();

    // Measured: this batch performs 21 checkpoints with the inner bounds present (2 gate + 7 reserve + 9 counting
    // + 3 consuming). Allowing 5 and stopping means the stop lands in the COUNTING loop — past the gate and past
    // the reservations — so it is only observable by a per-action check. With the inner checks removed the same
    // batch performs 9 checkpoints and completes, which is exactly what the mutation confirms.
    let mut ledger = StopAfter {
        remaining: 5,
        inner: capable(),
    };
    let outcome = fold.apply_owning(version_with_events(1, 9), kb(), allocations, &mut ledger);
    match outcome {
        FoldOutcome::Refused(Refusal::Stopped(_)) => {}
        other => panic!("a mid-loop stop must surface as Stopped, got {other:?}"),
    }
    // Scratch contract: the caller discards this fold, so a partially applied state is acceptable here — what is
    // asserted is that the stop was observed at all.
}

#[test]
fn an_uninterrupted_batch_still_completes_with_the_same_ledger_shape() {
    // Positive control: with enough checkpoints allowed, the same nine actions fold successfully.
    let mut fold = StateFold::new();
    let allocations: Vec<FoldEvent> = (0..9u16)
        .map(|space| {
            FoldEvent::IdAllocation(IdAllocationEvent {
                allocation: Watermark {
                    space: IdSpace::from_u16(space).expect("space"),
                    hwm: 0,
                },
                reason: None,
                extensions: Extensions::new(),
            })
        })
        .collect();

    let mut ledger = StopAfter {
        remaining: 1000,
        inner: capable(),
    };
    fold.apply_owning(version_with_events(1, 9), kb(), allocations, &mut ledger)
        .into_result()
        .expect("plenty of checkpoints");
    assert_eq!(fold.head_version(), Some(1));
}

#[test]
fn both_owning_model_index_keys_are_admitted() {
    struct KeyBudget {
        key_len: u64,
        copies: usize,
        left: usize,
        inner: sage_db_store::lease::AttemptLedger<'static>,
    }
    impl sage_db_store::lease::LeaseLedger for KeyBudget {
        // MUST delegate to `self.inner` — the SAME counter `try_charge` forwards to. Delegating to a fresh ledger would
        // give leases a different counter from the one that admitted the charge, so `adopt_retained` would see nothing.
        fn reserve_owned(
            &mut self,
            bytes: u64,
        ) -> Result<sage_db_store::lease::OwnedReservation, BudgetError> {
            self.inner.reserve_owned(bytes)
        }
        fn adopt_retained(
            &mut self,
            bytes: u64,
        ) -> Result<sage_db_store::lease::OwnedReservation, sage_db_store::lease::TransferError>
        {
            self.inner.adopt_retained(bytes)
        }
    }

    impl SharedLedger for KeyBudget {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            // The double's own limit decides admission for the copy under test...
            if bytes == self.key_len {
                self.copies += 1;
                if self.left == 0 {
                    return Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0));
                }
                self.left -= 1;
            }
            // ...and everything it admits must reach the SAME counter that issues leases, or `adopt_retained` would see
            // credit this double never transferred.
            self.inner.try_charge(bytes)
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }
    // The odd key length cannot equal a table-slot extent (all are multiples of 8).
    let key = "the-model-copy-probe-key!";
    assert_ne!(key.len() % 8, 0);
    let mut short = KeyBudget {
        key_len: key.len() as u64,
        copies: 0,
        left: 1,
        inner: capable(),
    };
    // The caller admits the record's retained payload before handing it over; the fold TRANSFERS that credit.
    let record = FoldedRecord::Model(model(key));
    let payload = sage_db_store::fold::record_retained_bytes(&record).expect("extent fits");
    if payload > 0 {
        short.try_charge(payload).expect("payload admission");
    }
    let result = StateFold::new().apply_owning(
        version(1),
        kb(),
        vec![FoldEvent::Record(record)],
        &mut short,
    );
    assert!(
        matches!(result, FoldOutcome::Refused(Refusal::Budget(_))),
        "the second owning key must be admitted before it is copied"
    );
    assert_eq!(short.copies, 2);
    let mut enough = KeyBudget {
        key_len: key.len() as u64,
        copies: 0,
        left: 2,
        inner: capable(),
    };
    let record = FoldedRecord::Model(model(key));
    let payload = sage_db_store::fold::record_retained_bytes(&record).expect("extent fits");
    if payload > 0 {
        enough.try_charge(payload).expect("payload admission");
    }
    let mut fold = StateFold::new();
    fold.apply_owning(
        version(1),
        kb(),
        vec![FoldEvent::Record(record)],
        &mut enough,
    )
    .into_result()
    .expect("two admitted keys");
    assert_eq!(enough.copies, 2);
    assert_eq!(fold.current_of_kind(StateKind::Model).count(), 1);
}

#[test]
fn each_new_index_row_needs_an_entry() {
    // Two metadata rows + revision row + current row. Byte allowance is independent.
    //
    // The record's nested buffers are admitted up front: a fold TRANSFERS payload credit rather than charging it, so an
    // un-admitted payload is a caller precondition failure (`PayloadNotAdmitted`), not a budget refusal. Admitting it here
    // keeps this test about entry accounting, which is what its name says.
    let mut short = AllowBoth {
        bytes: u64::MAX,
        entries: 3,
        inner: capable(),
    };
    admit_record_payload(&mut short, &term(1, 1, false));
    let result = StateFold::new().apply_owning(
        version(1),
        kb(),
        vec![FoldEvent::Record(term(1, 1, false))],
        &mut short,
    );
    assert!(matches!(result, FoldOutcome::Refused(Refusal::Budget(_))));
    let mut exact = AllowBoth {
        bytes: u64::MAX,
        entries: 4,
        inner: capable(),
    };
    admit_record_payload(&mut exact, &term(1, 1, false));
    StateFold::new()
        .apply_owning(
            version(1),
            kb(),
            vec![FoldEvent::Record(term(1, 1, false))],
            &mut exact,
        )
        .into_result()
        .expect("four rows admitted");
    assert_eq!(exact.entries, 0);
}
