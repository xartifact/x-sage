//! Owned event storage for an attempt, and a borrowing view the encoder can consume.
//!
//! # What this is, and what it is not
//!
//! A Store preparing a commit must hold an attempt's events across the period before it
//! writes their bytes. [`OwnedEvent`] is that storage: event tag plus owned field bytes.
//! [`view_for_encode`] admits and builds the encoder inputs ([`EncodableView`]), and
//! [`encode_view`] hands them to the canonical encoder core together with a borrowed
//! header, which is the shape [`crate::encode::encode_tx`] accepts.
//!
//! This module deliberately contains **no** commit carrier. The shared carrier belongs to
//! the types crate; these are transient input adapters that exist only for the length of
//! one encode call.
//!
//! # Why views rather than clones
//!
//! An earlier draft cloned the header (`String`s and an extension map) and collected a
//! fresh `Vec<Field>` per event, then told the caller to charge its ledger for the result.
//! That is admission after allocation: the peak already happened, and no later number can
//! undo it. Views have no such cost - they borrow what the caller already owns, and the
//! only allocation left is the field table, which is admitted before it is reserved.

use crate::dispatch::DispatchedPayload;
use crate::encode::EventToEncode;
use crate::encode::TxHeaderToEncode;
use crate::event::Field;
use sage_db_types::framing::{BytesSink, FormatError, FormatOutcome, LimitKind, Limits, Refusal};
use sage_db_types::ledger::SharedLedger;

/// Build an [`OwnedEvent`] from owned parts.
///
/// A local function rather than an inherent `new`: `OwnedEvent` is declared in the types
/// crate, so an `impl` here would be an inherent impl on a foreign type and would not
/// compile. This keeps the construction in one place without adding a constructor to the
/// shared declaration.
#[cfg(test)]
fn owned_event(tag: u16, fields: Vec<(u16, Vec<u8>)>) -> OwnedEvent {
    OwnedEvent { tag, fields }
}

/// One event as owned bytes: the attempt-lifetime twin of [`EventToEncode`].
///
/// **Re-exported from `sage_db_types::prepared`**: one declaration, so the carrier and the
/// encoder cannot drift. Field order is not validated by the type; an event is meaningful
/// only once an encoder has judged it.
pub use sage_db_types::prepared::OwnedEvent;

/// A borrowing view of one [`OwnedEvent`] as the encoder's [`crate::encode::EventToEncode`].
///
/// Holds its own `Vec<Field>` because the encoder takes one; that table is admitted before
/// it is reserved. No field *value* is copied.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EventView<'a> {
    tag: u16,
    fields: Vec<Field<'a>>,
}

impl<'a> EventView<'a> {
    /// Borrow `event` as an encoder input, admitting the field-table slots first.
    ///
    /// # Errors
    ///
    /// [`Refusal`] when the slot charge, cancellation or the reservation is refused.
    /// Nothing is returned in that case, so a partial table cannot escape.
    /// Private: the only reachable path to a view is [`view_for_encode`], which applies the
    /// normative count gate first. A public constructor here would be a second way to build
    /// field tables without any `Limits` check, and that is precisely the uncapped loop this
    /// module must not offer.
    fn new(event: &'a OwnedEvent, ledger: &mut dyn SharedLedger) -> Result<Self, Refusal> {
        // Admit the FIELD SLOTS as entries, one per field, not one per table.
        //
        // A table of 20 fields retains 20 `Field` slots; charging a single entry for the
        // table understates it by 19 and lets a tight entry budget build a large table as
        // long as its bytes fit. The per-event table is not itself an entry: it is the
        // `Vec` header inside `EventToEncode`, whose slot bytes `reserve_fields_narrow`
        // charges. Entries here are the retained field descriptors.
        ledger.try_charge_entries(event.fields.len() as u64)?;
        ledger.checkpoint()?;
        let mut fields = crate::encode::reserve_fields_narrow(event.fields.len(), ledger)?;
        for (tag, value) in &event.fields {
            fields.push(Field::new(*tag, value));
        }
        Ok(Self {
            tag: event.tag,
            fields,
        })
    }

    /// The event tag.
    #[must_use]
    pub const fn tag(&self) -> u16 {
        self.tag
    }

    /// The field table.
    #[must_use]
    pub fn fields(&self) -> &[Field<'a>] {
        &self.fields
    }

    /// Consume the view, yielding the encoder's own input type.
    ///
    /// `encode_tx` takes `&TxToEncode`, so the caller assembles that struct from these
    /// owned views; this method exists so the mapping is explicit and cannot drift.
    #[must_use]
    pub fn into_encode(self) -> crate::encode::EventToEncode<'a> {
        crate::encode::EventToEncode {
            tag: self.tag,
            fields: self.fields,
        }
    }
}

/// Assemble an encoder input from owned events and a caller-owned header.
///
/// The header is **borrowed**, not cloned: `encode_tx` only reads it, so a clone would be
/// an unadmitted copy of exactly the kind this module exists to avoid. The returned
/// [`crate::encode::TxToEncode`] owns its header field by design, so this function returns
/// the pieces separately instead of forging that struct with a copy.
///
/// # Errors
///
/// [`Refusal`] when admitting the event-table slots is refused.
pub fn view_for_encode<'a>(
    header: &'a TxHeaderToEncode,
    events: &'a [OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<EncodableView<'a>> {
    // ---- Normative counts, checked BEFORE any allocation ----
    //
    // The `OwnedEvent` declaration performs no validation — it is a data record — so a
    // caller can hand this factory a slice larger than the format allows. Building the
    // table first and letting the encoder reject it later would allocate on credit for
    // input that can never be legal.
    if events.len() > limits.max_events_per_tx as usize {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::EventsPerTx));
    }
    for event in events {
        if event.fields.len() > usize::from(limits.max_event_fields) {
            return FormatOutcome::Format(FormatError::Limit(LimitKind::EventFields));
        }
    }

    // ---- Admission: entries AND bytes, both before the table exists ----
    //
    // The table is `Vec<EventToEncode>`, so the retained slot size is that element's, and
    // each slot is also an entry. Charging only bytes would let a zero-entry budget build
    // an arbitrarily long table.
    let table_bytes =
        (events.len() as u64).saturating_mul(core::mem::size_of::<EventToEncode<'a>>() as u64);
    if let Err(error) = ledger.try_charge_entries(events.len() as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(error) = ledger.try_charge(table_bytes) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut built: Vec<EventToEncode<'a>> = Vec::new();
    if built.try_reserve_exact(events.len()).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation {
            requested: table_bytes as usize,
        });
    }
    // Each event's field table is admitted by the same helper the encoder uses; the loop
    // checkpoint keeps cancellation observable on a large transaction.
    for event in events {
        if let Err(stop) = ledger.checkpoint() {
            return FormatOutcome::Refused(Refusal::Stopped(stop));
        }
        match EventView::new(event, ledger) {
            Ok(view) => built.push(view.into_encode()),
            Err(refusal) => return FormatOutcome::Refused(refusal),
        }
    }
    FormatOutcome::Ok(EncodableView {
        header,
        events: built,
    })
}

/// A ready-to-encode transaction assembled from borrowed parts.
///
/// Deliberately not `TxToEncode`: that struct owns its header, and owning it here would
/// reintroduce the clone. [`encode_prepared`] feeds these borrowed parts straight into the
/// canonical encoder core, so there is no clone and no second serialization.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EncodableView<'a> {
    header: &'a TxHeaderToEncode,
    /// Encoder inputs, built once with every field table admitted. Storing `EventToEncode`
    /// directly - rather than a per-event wrapper that must be converted again at encode
    /// time - is what makes `encode_view` copy nothing: it hands `&events` to the core.
    events: Vec<EventToEncode<'a>>,
}

impl<'a> EncodableView<'a> {
    /// The borrowed header.
    #[must_use]
    pub const fn header(&self) -> &'a TxHeaderToEncode {
        self.header
    }

    /// The per-event encoder inputs.
    #[must_use]
    pub fn events(&self) -> &[EventToEncode<'a>] {
        &self.events
    }

    /// How many events the transaction declares.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

/// Encode a prepared transaction, borrowing every part.
///
/// This is the entry point a Store uses: it holds the header and the owned events for the
/// length of an attempt, then calls this once. No header clone, no second event table
/// beyond the field tables that [`view_for_encode`] already admitted, and the bytes come
/// from the same canonical core that [`crate::encode::encode_tx`] uses — the two cannot
/// drift because there is one serialization.
///
/// # Errors
///
/// Same classes as [`crate::encode::encode_tx`]: a `Corrupt`/`Limit`/`UnsupportedFormat`
/// verdict, or a [`Refusal`] for budget, cancellation or allocation.
pub fn encode_prepared(
    header: &TxHeaderToEncode,
    events: &[OwnedEvent],
    sink: &mut dyn BytesSink,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    // One factory, then one encode. The counts, the entry charge, the byte charge and the
    // per-event field tables are all `view_for_encode`'s job; restating that arithmetic
    // here is how the two paths drifted before.
    match view_for_encode(header, events, limits, ledger) {
        FormatOutcome::Ok(view) => encode_view(&view, sink, limits, ledger),
        FormatOutcome::Format(error) => FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
    }
}

/// Decode one owned event into its typed payload.
///
/// This is what a Store needs to consume an exact decoded record — an outcome, a fold
/// record, a Genesis — instead of indexing fields by hand, inventing defaults, or cloning
/// an owned payload it cannot produce fallibly. The bytes are written by the real
/// [`crate::event::write_event`] and read by the real
/// [`crate::dispatch::read_dispatched`], so the payload is exactly what a fresh decode of
/// the same event yields: one policy, not a second interpretation.
///
/// The normative count gate runs **before** any allocation, and the field table is admitted
/// as entries plus bytes before it is reserved. `Format` verdicts stay `Format` verdicts and
/// [`Refusal`] stays [`Refusal`]; the two are never collapsed.
///
/// # Errors
///
/// [`FormatOutcome::Format`] for a ceiling breach or for bytes the family reader refuses
/// (an event that is legal shape but not a complete after-image is refused here, not
/// repaired); [`FormatOutcome::Refused`] for budget, cancellation or allocation.
pub fn decode_owned_event<'a>(
    event: &'a OwnedEvent,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<DispatchedPayload> {
    if event.fields.len() > usize::from(limits.max_event_fields) {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::EventFields));
    }
    // The field table is a retained allocation: entries and bytes, before the reservation.
    let slot_bytes =
        (event.fields.len() as u64).saturating_mul(core::mem::size_of::<Field<'a>>() as u64);
    if let Err(error) = ledger.try_charge_entries(event.fields.len() as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(error) = ledger.try_charge(slot_bytes) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut fields: Vec<Field<'a>> = Vec::new();
    if fields.try_reserve_exact(event.fields.len()).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation {
            requested: slot_bytes as usize,
        });
    }
    for (tag, value) in &event.fields {
        fields.push(Field::new(*tag, value));
    }
    crate::encode::decode_fields_typed(event.tag, &fields, ledger)
}

/// Encode from an already-built [`EncodableView`].
///
/// This is the entry point for a caller that keeps the view across several steps (admit
/// once, encode once): the header is borrowed from the view and the per-event tables were
/// already admitted by [`view_for_encode`], so nothing here allocates or copies. The bytes
/// come from the same canonical core as every other path.
///
/// # Errors
///
/// Same classes as [`crate::encode::encode_tx`].
pub fn encode_view(
    view: &EncodableView<'_>,
    sink: &mut dyn BytesSink,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    // Nothing is copied: the view already holds admitted encoder inputs, so the core takes
    // them by reference. There is no allocation on this path for a ledger to admit.
    crate::encode::encode_tx_parts(view.header(), view.events(), sink, limits, ledger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetError;
    use sage_db_types::persist::Extensions;
    use sage_db_types::query::ControlStop;

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

    /// Records every charge and refuses from the Nth one onwards.
    ///
    /// A ledger that refuses everything cannot distinguish "admitted before allocating"
    /// from "allocated then charged" through call counts alone, so the assertions below
    /// compare the charge AMOUNT against the retained slot size.
    struct Recorder {
        charges: Vec<u64>,
        refuse_from: usize,
    }

    impl SharedLedger for Recorder {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            self.charges.push(bytes);
            if self.charges.len() > self.refuse_from {
                return Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Bytes,
                    bytes,
                    0,
                ));
            }
            Ok(())
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    fn header() -> TxHeaderToEncode {
        TxHeaderToEncode {
            kb_uuid: sage_db_types::identity::KbUuid::from_bytes([7u8; 16]),
            version: 1,
            tx_id: [9u8; 16],
            prev_version: None,
            created_at: 1,
            principal_id: "system".to_owned(),
            request_id: "genesis".to_owned(),
            payload_digest: [0u8; 32],
            rules_profile: 1,
            rules_digest: [3u8; 32],
            request_canon_version: crate::encode::RequestCanonVersion::NonRequest,
            expect_version: None,
            extensions: Extensions::new(),
        }
    }

    #[test]
    fn an_event_view_borrows_field_bytes_without_copying_them() {
        let event = owned_event(crate::term::TERM_CREATED, vec![(17u16, b"atom".to_vec())]);
        let view = EventView::new(&event, &mut Open).expect("admitted");
        assert_eq!(view.tag(), event.tag);
        assert_eq!(view.fields().len(), 1);
        // Same bytes, borrowed: the view points into the owned value.
        assert_eq!(view.fields()[0].value(), b"atom");
        let original_ptr = event.fields[0].1.as_ptr();
        assert_eq!(
            view.fields()[0].value().as_ptr(),
            original_ptr,
            "the view must borrow, not copy"
        );
    }

    #[test]
    fn a_refused_slot_charge_returns_a_refusal_not_a_classification() {
        // The only claim made about ordering: the charge comes before the reservation, so a
        // refused ledger means nothing was allocated. Structural claim plus the real budget
        // control — not an allocator oracle. (`EventView::new` is private, so this exercises
        // the internal builder rather than a public path.)
        let event = owned_event(crate::term::TERM_CREATED, vec![(17u16, b"atom".to_vec())]);
        let mut led = Recorder {
            charges: Vec::new(),
            refuse_from: 0,
        };
        match EventView::new(&event, &mut led) {
            Err(Refusal::Budget(_)) => {}
            other => panic!("a spent ledger must refuse, got {other:?}"),
        }
        assert_eq!(led.charges.len(), 1, "exactly one charge was attempted");
    }

    #[test]
    fn the_header_is_borrowed_never_cloned() {
        // If this function cloned the header, the returned view's header pointer would
        // differ from the caller's. Comparing addresses is the direct test.
        let h = header();
        let events = [owned_event(
            crate::term::TERM_CREATED,
            vec![(17u16, b"atom".to_vec())],
        )];
        let view = match view_for_encode(&h, &events, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(view) => view,
            other => panic!("admitted: {other:?}"),
        };
        assert_eq!(view.event_count(), 1);
        assert!(
            core::ptr::eq(view.header(), &h),
            "the header must be borrowed from the caller, not cloned"
        );
    }
    /// A ledger with independent byte and entry allowances, both spendable.
    struct Allowance {
        bytes: u64,
        entries: u64,
    }

    impl SharedLedger for Allowance {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.bytes {
                return Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Bytes,
                    bytes,
                    self.bytes,
                ));
            }
            self.bytes -= bytes;
            Ok(())
        }
        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            if count > self.entries {
                return Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Entries,
                    count,
                    self.entries,
                ));
            }
            self.entries -= count;
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    fn one_field_event(tag: u16) -> OwnedEvent {
        owned_event(tag, vec![(17u16, b"a".to_vec())])
    }

    #[test]
    fn field_slots_need_entries_even_when_the_event_table_is_paid() {
        // The defect this discriminates: charging ONE entry per table instead of one per field
        // slot. With 20 fields, an entry allowance of 2 (event table + a token) was previously
        // enough; the true cost is 20 field entries. Amount alone cannot separate the two, so
        // this fixes the exact allowance just below the field count.
        let h = header();
        let wide = OwnedEvent {
            tag: crate::term::TERM_CREATED,
            fields: (0..20u16).map(|i| (100 + i, vec![0u8])).collect(),
        };
        let events = [wide];

        // Allowance: ample bytes, ample per-event allowance, but only 2 entries.
        let mut led = Allowance {
            bytes: u64::MAX,
            entries: 2,
        };
        match view_for_encode(&h, &events, &Limits::V1, &mut led) {
            FormatOutcome::Refused(Refusal::Budget(_)) => {}
            other => panic!("20 field slots must not fit in 2 entries: {other:?}"),
        }

        // Positive control: exactly enough entries succeeds. The total is 21 — one event-table
        // slot plus the 20 field slots — which is also the number root's probe found sufficient.
        let mut led = Allowance {
            bytes: u64::MAX,
            entries: 21,
        };
        assert!(
            matches!(
                view_for_encode(&h, &events, &Limits::V1, &mut led),
                FormatOutcome::Ok(_)
            ),
            "21 entries (1 event table + 20 field slots) must be sufficient"
        );
    }

    #[test]
    fn a_byte_budget_cannot_build_tables_without_entry_allowance() {
        // The gap this closes: a factory that charges only bytes lets a zero-entry ledger
        // build an arbitrarily long table. Entries are a separate unit and must be refused.
        let h = header();
        let events = vec![one_field_event(crate::term::TERM_CREATED); 4];
        let mut led = Allowance {
            bytes: u64::MAX,
            entries: 0,
        };
        // The internal builder also admits its own field slots, so the common path cannot
        // bypass the entry unit even from inside the module.
        match EventView::new(&events[0], &mut led) {
            Err(Refusal::Budget(_)) => {}
            other => panic!("a zero-entry ledger must refuse a table: {other:?}"),
        }
        // Factory: same rule, before any table exists.
        match view_for_encode(&h, &events, &Limits::V1, &mut led) {
            FormatOutcome::Refused(Refusal::Budget(_)) => {}
            other => panic!("factory must refuse on entries first: {other:?}"),
        }
    }

    #[test]
    fn the_normative_event_ceiling_is_enforced_before_allocating() {
        // max_events_per_tx is a FORMAT_LIMIT, not a budget refusal: the input is legal
        // shape but over a declared ceiling, so it must not be reported as retry-with-more.
        let h = header();
        let mut limits = Limits::V1;
        limits.max_events_per_tx = 2;
        let events = vec![one_field_event(crate::term::TERM_CREATED); 3];
        let mut led = Allowance {
            bytes: u64::MAX,
            entries: u64::MAX,
        };
        match view_for_encode(&h, &events, &limits, &mut led) {
            FormatOutcome::Format(FormatError::Limit(LimitKind::EventsPerTx)) => {}
            other => panic!("over-ceiling event count must be a limit: {other:?}"),
        }
        // Exactly at the ceiling is legal.
        let ok_events = vec![one_field_event(crate::term::TERM_CREATED); 2];
        assert!(matches!(
            view_for_encode(&h, &ok_events, &limits, &mut led),
            FormatOutcome::Ok(_)
        ));
    }

    #[test]
    fn the_normative_field_ceiling_is_enforced_before_allocating() {
        let h = header();
        let mut limits = Limits::V1;
        limits.max_event_fields = 2;
        let wide = owned_event(
            crate::term::TERM_CREATED,
            vec![
                (17u16, b"a".to_vec()),
                (18u16, b"b".to_vec()),
                (19u16, b"c".to_vec()),
            ],
        );
        let mut led = Allowance {
            bytes: u64::MAX,
            entries: u64::MAX,
        };
        match view_for_encode(&h, &[wide], &limits, &mut led) {
            FormatOutcome::Format(FormatError::Limit(LimitKind::EventFields)) => {}
            other => panic!("over-ceiling field count must be a limit: {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_caller_stops_the_factory() {
        struct Halt;
        impl SharedLedger for Halt {
            fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Err(ControlStop::Cancelled)
            }
        }
        let h = header();
        let events = vec![one_field_event(crate::term::TERM_CREATED)];
        match view_for_encode(&h, &events, &Limits::V1, &mut Halt) {
            FormatOutcome::Refused(Refusal::Stopped(_)) => {}
            other => panic!("cancellation must not become a verdict: {other:?}"),
        }
    }
    #[test]
    fn a_decoded_outcome_keeps_its_full_audit_and_extensions() {
        // The defect this exists for: a Store-side helper that indexes fields by hand drops
        // audit fields and extension bytes. Decoding through the real reader keeps them.
        use sage_db_types::persist::{ActorOrigin, Extensions};

        let mut ext = Extensions::new();
        ext.insert("k", b"v".to_vec()).expect("extension");
        let outcome = sage_db_types::persist::RequestOutcomeEvent {
            principal_id: "alice".to_owned(),
            request_id: "req-1".to_owned(),
            payload_digest: [5u8; 32],
            result_ref: [6u8; 32],
            commit_version: 7,
            audit: sage_db_types::persist::CreationAudit {
                recorded_by: Some("alice".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some("because".to_owned()),
                created_at: 11,
            },
            extensions: ext.clone(),
        };
        let bytes = crate::event::encode_with(&mut Open, |w| {
            crate::natural::write_request_outcome(w, &outcome)
        })
        .expect("encode");
        let mut r = crate::wire::Reader::new(&bytes);
        let event = crate::event::read_event(&mut r, &mut Open).expect("re-read");
        let owned = owned_event(
            event.tag(),
            event
                .fields()
                .iter()
                .map(|f| (f.tag(), f.value().to_vec()))
                .collect(),
        );

        match decode_owned_event(&owned, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(DispatchedPayload::RequestOutcome(decoded)) => {
                assert_eq!(decoded.principal_id, "alice");
                assert_eq!(decoded.request_id, "req-1");
                assert_eq!(decoded.result_ref, [6u8; 32]);
                assert_eq!(decoded.commit_version, 7);
                assert_eq!(
                    decoded.audit.recorded_by.as_deref(),
                    Some("alice"),
                    "audit must survive, not be defaulted"
                );
                assert_eq!(decoded.audit.reason.as_deref(), Some("because"));
                assert_eq!(decoded.extensions, ext, "extension bytes must survive");
            }
            other => panic!("expected a decoded outcome, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_event_is_refused_rather_than_defaulted() {
        // A TermCreated carrying only `17 atom` is not a complete after-image; the family
        // reader refuses it. A hand-indexing helper would have filled the rest with defaults.
        let owned = owned_event(crate::term::TERM_CREATED, vec![(17u16, b"atom".to_vec())]);
        match decode_owned_event(&owned, &Limits::V1, &mut Open) {
            FormatOutcome::Format(_) => {}
            other => panic!("a partial event must be refused, got {other:?}"),
        }
    }

    #[test]
    fn the_field_count_ceiling_is_a_limit_before_any_allocation() {
        let mut limits = Limits::V1;
        limits.max_event_fields = 2;
        let owned = owned_event(
            crate::term::TERM_CREATED,
            vec![(17u16, vec![0]), (18u16, vec![0]), (19u16, vec![0])],
        );
        match decode_owned_event(&owned, &limits, &mut Open) {
            FormatOutcome::Format(FormatError::Limit(LimitKind::EventFields)) => {}
            other => panic!("over-ceiling field count must be a limit: {other:?}"),
        }
    }

    #[test]
    fn a_refusal_stays_a_refusal_and_cancellation_is_observed() {
        struct Halt;
        impl SharedLedger for Halt {
            fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Err(ControlStop::Cancelled)
            }
        }
        let owned = owned_event(crate::term::TERM_CREATED, vec![(17u16, b"atom".to_vec())]);
        match decode_owned_event(&owned, &Limits::V1, &mut Halt) {
            FormatOutcome::Refused(Refusal::Stopped(_)) => {}
            other => panic!("cancellation must not become a verdict: {other:?}"),
        }

        struct Spent;
        impl SharedLedger for Spent {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Bytes,
                    bytes,
                    0,
                ))
            }
            fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }
        match decode_owned_event(&owned, &Limits::V1, &mut Spent) {
            FormatOutcome::Refused(Refusal::Budget(_)) => {}
            other => panic!("a spent ledger must refuse, not classify: {other:?}"),
        }
    }
}
