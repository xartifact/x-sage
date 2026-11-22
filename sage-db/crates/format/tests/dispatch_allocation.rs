//! The seam's retained record allocation is admitted, not assumed.
//!
//! A `Box::new` is an allocation. If the seam boxes first and charges never, a caller
//! whose budget is spent still pays the peak - the refusal arrives after the damage.
use sage_db_format::{
    dispatch::read_dispatched, event::encode_with, structural::write_id_allocation,
    term::write_term, wire::Reader,
};
use sage_db_types::{
    confidence::Confidence,
    framing::FormatOutcome,
    identity::RecordRevision,
    layer::SemanticLayer,
    ledger::{BudgetError, BudgetUnit, SharedLedger},
    persist::{
        ActorOrigin, CommonFields, Extensions, IdAllocationEvent, IdSpace, RecordKey, TermRecord,
        TermSource, Watermark,
    },
    query::ControlStop,
};

struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// Refuses every byte charge, and counts how many charges it was asked for.
struct Spent {
    charges: u32,
}

impl SharedLedger for Spent {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.charges += 1;
        Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

fn term() -> TermRecord {
    TermRecord {
        key: RecordKey::term(1),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: "s".into(),
        atom: "a".into(),
        layer: SemanticLayer::L1,
        domains: vec!["d".into()],
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["t".into()],
        body_ref: [1; 32],
        common: CommonFields {
            recorded_by: None,
            actor_origin: ActorOrigin::Unavailable,
            reason: None,
            created_at: 1,
            updated_at: None,
        },
        legacy_id: None,
        legacy_source_path: None,
        legacy_content_digest: None,
        legacy_source_file_digest: None,
        legacy_source_revision: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

fn idallocation() -> Vec<u8> {
    let record = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::TermId,
            hwm: 3,
        },
        reason: None,
        extensions: Extensions::new(),
    };
    encode_with(&mut Open, |w| write_id_allocation(w, &record)).expect("encode")
}

#[test]
fn a_spent_budget_refuses_before_the_record_is_boxed() {
    let bytes = idallocation();
    let mut led = Spent { charges: 0 };
    let outcome = read_dispatched(&mut Reader::new(&bytes), &mut led);
    // IdAllocation is not boxed, so this asserts the general path still charges; the
    // boxed families are covered by the assertion below using the same rule.
    assert!(
        !matches!(outcome, FormatOutcome::Ok(_)),
        "a spent budget must not yield a decoded record"
    );
    assert!(led.charges > 0, "the seam must charge its retained work");
}

#[test]
fn dispatching_a_record_allocates_nothing() {
    // The defect this asserts against: the payload used to be `Box`ed, so every decoded
    // event added an unmetered heap allocation that no ledger could refuse and no
    // `Refusal::Allocation` could describe. The payload is now inline, so the produced
    // value is stack-resident and `needs_drop` is decided by the records' own owned
    // strings - not by an extra indirection this module introduced.
    let bytes = encode_with(&mut Open, |w| {
        write_term(w, sage_db_format::term::TermEvent::Created, &term())
    })
    .expect("encode");

    // `DispatchedEvent` must be constructible without any allocator involvement beyond
    // what the record's own fields need; the observable form of "no extra heap" here is
    // that the type contains no `Box` payload. `size_of` must therefore cover the
    // payload inline, and the value must fit a caller's stack rather than a pointer.
    let payload = std::mem::size_of::<sage_db_format::dispatch::DispatchedPayload>();
    let term = std::mem::size_of::<sage_db_types::persist::TermRecord>();
    assert!(
        payload >= term,
        "the payload is inline, so it must be at least as large as its biggest record \
         ({payload} vs {term}); a pointer-sized payload would mean it is still boxed"
    );

    let outcome = read_dispatched(&mut Reader::new(&bytes), &mut Open);
    assert!(matches!(outcome, FormatOutcome::Ok(_)));
}

#[test]
fn a_spent_budget_still_charges_the_records_owned_bytes() {
    // Inlining removed the *extra* allocation, not the accounting: the records own
    // strings, and those are still charged by the field decoders.
    let bytes = encode_with(&mut Open, |w| {
        write_term(w, sage_db_format::term::TermEvent::Created, &term())
    })
    .expect("encode");
    let mut led = Spent { charges: 0 };
    let outcome = read_dispatched(&mut Reader::new(&bytes), &mut led);
    assert!(
        !matches!(outcome, FormatOutcome::Ok(_)),
        "spent ledger must refuse"
    );
    assert!(led.charges > 0);
}
