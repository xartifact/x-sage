//! Shared plumbing for record codecs: field assembly, allowed-field policy, and the
//! resource discipline every family inherits.
//!
//! This module exists because the same three mistakes were about to be copied into every
//! record family:
//!
//! 1. **Assembling fields without admitting them.** A `Vec<(tag, bytes)>` that grows by
//!    `push` is invisible to the ledger, and then a second `Vec<Field>` is collected from
//!    it - two unadmitted allocations per event. [`FieldSet`] reserves both slots under
//!    the shared ledger before it grows.
//! 2. **Treating registry acceptance as schema acceptance.** A field tag that is globally
//!    known (`evidence_kind`, `dimensions`, …) is still *foreign* to a Term event. Accepting
//!    it and quietly not storing it is the worst outcome: the bytes are authoritative and
//!    the record no longer round-trips. Each family therefore declares its **permitted
//!    field set**, and a recognized-but-foreign tag is refused with
//!    [`DecodeError::ForbiddenField`].
//! 3. **Guessing revision history in the codec.** A codec knows one thing about revisions:
//!    the first revision belongs to a creation event. Whether revision N follows N-1 is a
//!    fold question, answered by replay against the log, not by a decoder inspecting a
//!    single event.
//!
//! ## The four independent axes a record field can have
//!
//! | axis | meaning | where enforced |
//! |---|---|---|
//! | permitted | this family/event may carry the tag at all | [`FieldPolicy::permitted`] |
//! | required | the tag must appear | [`FieldPolicy::required`] |
//! | forbidden | the tag must not appear for this event kind | each family's own check, or [`check_policy`] for foreign tags |
//! | conditional | required only under a stated condition | each family's own check |
//!
//! Keeping them separate matters: `reason` is *permitted* on every record event,
//! *required* on terminal events, and *forbidden* nowhere. Collapsing these into one
//! "optional" flag is how a mandatory audit field becomes droppable.

use sage_db_types::ledger::SharedLedger;

use crate::event::{Event, Field, write_event};
use crate::wire::{DecodeError, EncodeError, ReadError, Reader, WriteError, Writer};

/// The field tags a record family may carry, and which are required for a given event.
#[derive(Clone, Copy)]
pub struct FieldPolicy {
    /// Tags this family may carry at all. A recognized tag outside this list is foreign.
    permitted: &'static [u16],
    /// Tags that must appear in every event of this family.
    required: &'static [u16],
}

impl FieldPolicy {
    /// Build a policy from the family's field table.
    #[must_use]
    pub const fn new(permitted: &'static [u16], required: &'static [u16]) -> Self {
        Self {
            permitted,
            required,
        }
    }

    /// Whether this family may carry `tag`.
    #[must_use]
    pub fn permitted(&self, tag: u16) -> bool {
        self.permitted.contains(&tag)
    }

    /// Tags that must be present.
    #[must_use]
    pub const fn required(&self) -> &'static [u16] {
        self.required
    }
}

/// Accumulates encoded field values under the ledger, then writes one event.
///
/// Two allocations are admitted per field: the value buffer in [`FieldSet::push`], and a
/// slot in the field table reserved by [`FieldSet::with_capacity`] at construction time. The
/// final `Vec<Field>` borrows the value buffers, so it is the only other allocation and it
/// is reserved once, not grown by `collect`.
pub struct FieldSet {
    entries: Vec<(u16, Vec<u8>)>,
    reserved: usize,
}

impl FieldSet {
    /// An empty set whose slot capacity is admitted up front.
    ///
    /// # Errors
    ///
    /// [`WriteError::Budget`] or [`WriteError::Stopped`] from the ledger, or
    /// [`WriteError::Allocation`] when the slot array cannot be reserved.
    pub fn with_capacity(
        capacity: usize,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, WriteError> {
        ledger.try_charge_entries(capacity as u64)?;
        // The slot array is retained memory: charge its bytes, not just its length.
        let bytes = (capacity as u64).saturating_mul(core::mem::size_of::<(u16, Vec<u8>)>() as u64);
        ledger.try_charge(bytes)?;
        ledger.checkpoint()?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| WriteError::Allocation {
                requested: bytes as usize,
            })?;
        Ok(Self {
            entries,
            reserved: capacity,
        })
    }

    /// Encode one field value and remember it under `tag`.
    ///
    /// # Errors
    ///
    /// Whatever `encode` returns, or [`WriteError::Internal`] when more fields are pushed
    /// than were admitted - which means the caller's capacity declaration was wrong.
    pub fn push<T>(
        &mut self,
        ledger: &mut dyn SharedLedger,
        tag: u16,
        value: T,
        encode: impl FnOnce(&mut Writer<'_>, T) -> Result<(), WriteError>,
    ) -> Result<(), WriteError> {
        if self.entries.len() == self.reserved {
            return Err(WriteError::Internal {
                what: "FieldSet::push exceeded the capacity that was admitted",
            });
        }
        let mut buf = Vec::new();
        {
            let mut writer = Writer::new(&mut buf, ledger);
            encode(&mut writer, value)?;
        }
        self.entries.push((tag, buf));
        Ok(())
    }

    /// Encode an optional field; absent values add no field at all.
    ///
    /// # Errors
    ///
    /// As [`Self::push`].
    pub fn push_opt<T>(
        &mut self,
        ledger: &mut dyn SharedLedger,
        tag: u16,
        value: Option<T>,
        encode: impl FnOnce(&mut Writer<'_>, T) -> Result<(), WriteError>,
    ) -> Result<(), WriteError> {
        match value {
            Some(value) => self.push(ledger, tag, value, encode),
            None => Ok(()),
        }
    }

    /// Write these fields as one required event.
    ///
    /// # Errors
    ///
    /// [`WriteError::Data`] for a non-ascending or empty field, plus budget and
    /// allocation refusals for the envelope. The collected `Vec<Field>` is reserved
    /// exactly, so it cannot grow into an unadmitted allocation.
    pub fn write_event(self, writer: &mut Writer<'_>, event_tag: u16) -> Result<(), WriteError> {
        debug_assert!(
            self.entries.windows(2).all(|w| w[0].0 < w[1].0),
            "record field tags must be pushed in ascending order"
        );
        // The borrowed reference table is a SECOND allocation, distinct from the owned
        // slots admitted by `with_capacity`: one holds (tag, bytes), the other holds
        // (tag, &bytes). Fallible reservation alone is not admission, so this table is
        // charged and checkpointed on the same ledger before it is reserved.
        let slots = self.entries.len();
        let slot_bytes =
            (slots as u64).saturating_mul(core::mem::size_of::<Field<'static>>() as u64);
        let ledger = writer.ledger();
        ledger.try_charge_entries(slots as u64)?;
        ledger.try_charge(slot_bytes)?;
        ledger.checkpoint()?;
        let mut fields: Vec<Field<'_>> = Vec::new();
        fields
            .try_reserve_exact(slots)
            .map_err(|_| WriteError::Allocation {
                // Bytes, matching the field's documentation.
                requested: slot_bytes as usize,
            })?;
        for (tag, value) in &self.entries {
            fields.push(Field::new(*tag, value));
        }
        write_event(
            writer,
            event_tag,
            crate::event::EVENT_FLAG_REQUIRED,
            &fields,
        )
    }
}

/// The field's bytes, or [`DecodeError::MissingField`].
///
/// # Errors
///
/// [`ReadError::Data`] when the event does not carry `tag`.
pub fn require<'a>(event: &Event<'a>, tag: u16) -> Result<&'a [u8], ReadError> {
    event
        .field(tag)
        .ok_or(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: tag,
        }))
}

/// Refuse a field the event may not carry.
///
/// # Errors
///
/// [`ReadError::Data`] when `tag` is present.
pub fn forbid(event: &Event<'_>, tag: u16) -> Result<(), ReadError> {
    if event.field(tag).is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: event.tag(),
            field: tag,
        }));
    }
    Ok(())
}

/// Enforce a family's permitted/required field policy on a decoded event.
///
/// This is the step that makes per-event schema real. Registry acceptance says "this
/// major knows this tag"; this says "this record may carry it". A recognized tag that
/// belongs to another family is **foreign**, and foreign authoritative bytes are refused
/// rather than dropped.
///
/// # Errors
///
/// [`ReadError::Data`] with [`DecodeError::ForbiddenField`] for a foreign tag, or
/// [`DecodeError::MissingField`] for a required one.
pub fn check_policy(event: &Event<'_>, policy: &FieldPolicy) -> Result<(), ReadError> {
    for field in event.fields() {
        if !policy.permitted(field.tag()) {
            return Err(ReadError::Data(DecodeError::ForbiddenField {
                event: event.tag(),
                field: field.tag(),
            }));
        }
    }
    for tag in policy.required() {
        if event.field(*tag).is_none() {
            return Err(ReadError::Data(DecodeError::MissingField {
                event: event.tag(),
                field: *tag,
            }));
        }
    }
    Ok(())
}

/// Decode a sub-value from a field's bytes, requiring it to consume them exactly.
///
/// # Errors
///
/// Whatever `decode` returns, or [`DecodeError::Trailing`] when the value was shorter
/// than the field.
pub fn decode_field<'a, T>(
    event: &Event<'a>,
    tag: u16,
    decode: impl FnOnce(&mut Reader<'a>) -> Result<T, ReadError>,
) -> Result<T, ReadError> {
    let raw = require(event, tag)?;
    let mut reader = Reader::new(raw);
    let value = decode(&mut reader)?;
    reader.finish()?;
    Ok(value)
}

/// [`decode_field`] for an optional field.
///
/// # Errors
///
/// As [`decode_field`].
pub fn decode_opt_field<'a, T>(
    event: &Event<'a>,
    tag: u16,
    decode: impl FnOnce(&mut Reader<'a>) -> Result<T, ReadError>,
) -> Result<Option<T>, ReadError> {
    match event.field(tag) {
        Some(raw) => {
            let mut reader = Reader::new(raw);
            let value = decode(&mut reader)?;
            reader.finish()?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// Decode the `changed_fields` advisory list.
///
/// Shared because the same mistake is easy to repeat: charging entries but not the
/// element bytes, and skipping the count-versus-remaining feasibility check. Those three
/// obligations live here once.
///
/// # Errors
///
/// [`DecodeError::CountTooLong`] or [`DecodeError::Truncated`] for an impossible list,
/// [`DecodeError::FieldOrder`] for unsorted or duplicate tags, plus budget/control.
pub fn decode_changed_fields<'a>(
    event: &Event<'a>,
    tag: u16,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<Vec<u16>>, ReadError> {
    match event.field(tag) {
        None => Ok(None),
        Some(raw) => {
            let mut reader = Reader::new(raw);
            let count = reader.count(64)?;
            // Feasibility: each element is two bytes.
            let minimum = (count as usize).saturating_mul(2);
            if minimum > reader.remaining() {
                return Err(ReadError::Data(DecodeError::Truncated {
                    offset: reader.offset(),
                    wanted: minimum,
                }));
            }
            ledger.try_charge_entries(u64::from(count))?;
            ledger.try_charge(minimum as u64)?;
            ledger.checkpoint()?;
            let mut tags = Vec::new();
            tags.try_reserve_exact(count as usize)
                .map_err(|_| ReadError::Allocation { requested: minimum })?;
            let mut previous: Option<u16> = None;
            for _ in 0..count {
                let next = reader.u16()?;
                if previous.is_some_and(|prev| next <= prev) {
                    return Err(ReadError::Data(DecodeError::FieldOrder { tag: next }));
                }
                previous = Some(next);
                tags.push(next);
            }
            reader.finish()?;
            ledger.checkpoint()?;
            Ok(Some(tags))
        }
    }
}

/// Write the `changed_fields` advisory list.
///
/// # Errors
///
/// [`WriteError::Data`] when the list is not ascending or too long, plus budget.
pub fn write_changed_fields(writer: &mut Writer<'_>, tags: &[u16]) -> Result<(), WriteError> {
    if u32::try_from(tags.len()).map_or(true, |n| n > 64) {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    if !tags.windows(2).all(|w| w[0] < w[1]) {
        return Err(WriteError::Data(EncodeError::FieldOrder {
            tag: tags.windows(2).find(|w| w[0] >= w[1]).map_or(0, |w| w[1]),
        }));
    }
    writer.count(tags.len())?;
    for tag in tags {
        writer.u16(*tag)?;
    }
    Ok(())
}

/// Events whose specification makes `reason` mandatory.
///
/// Terminal or administrative events: retiring, retracting, rejecting, superseding,
/// deciding, and rule/profile changes all alter what the KB asserts, so "why" is part of
/// the record rather than a nicety. Centralised here so no family forgets it.
pub const REASON_REQUIRED_EVENTS: &[u16] = &[3, 18, 34, 35, 51, 81, 97, 147, 160, 208];

/// Whether `reason` must be present for `event_tag`.
#[must_use]
pub fn reason_required(event_tag: u16) -> bool {
    REASON_REQUIRED_EVENTS.contains(&event_tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EVENT_FLAG_REQUIRED, encode_with, read_event, write_event};
    use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop};

    struct Open;
    impl SharedLedger for Open {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    struct NoEntries;
    impl SharedLedger for NoEntries {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            Err(BudgetError::new(BudgetUnit::Entries, count, 0))
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    struct NoBytes;
    impl SharedLedger for NoBytes {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes == 0 {
                Ok(())
            } else {
                Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
            }
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    #[test]
    fn a_field_set_admits_its_slots_before_growing() {
        let mut led = NoEntries;
        let err = match FieldSet::with_capacity(4, &mut led) {
            Err(e) => e,
            Ok(_) => panic!("no entry budget must refuse"),
        };
        assert!(matches!(err, WriteError::Budget(_)), "got {err:?}");

        let mut led = NoBytes;
        let err = match FieldSet::with_capacity(4, &mut led) {
            Err(e) => e,
            Ok(_) => panic!("no byte budget must refuse"),
        };
        assert!(
            matches!(err, WriteError::Budget(e) if e.unit() == BudgetUnit::Bytes),
            "slot bytes must be charged too: {err:?}"
        );
    }

    /// The borrowed reference table is a second allocation and must be admitted too.
    ///
    /// Fallible reservation without admission was the defect: the slot array was
    /// reserved on the allocator but never charged, so a writer with no byte budget left
    /// could still build the reference table.
    #[test]
    fn the_borrowed_field_table_is_admitted_not_merely_reserved() {
        // Exactly enough for one owned slot and one value byte, nothing more.
        struct Exact(u64);
        impl SharedLedger for Exact {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                if bytes > self.0 {
                    return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.0));
                }
                self.0 -= bytes;
                Ok(())
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let slot = core::mem::size_of::<(u16, Vec<u8>)>() as u64;
        let mut led = Exact(slot + 1);
        let mut set = FieldSet::with_capacity(1, &mut led).expect("one slot fits");
        set.push(&mut led, 1, 1u8, |w, v| w.u8(v))
            .expect("one value byte fits");
        assert_eq!(led.0, 0, "the fixture must exhaust the budget");

        // Output goes into caller-owned storage, so only the reference table can be
        // refused - which is exactly the allocation the mutation would hide.
        let mut out = Vec::with_capacity(64);
        let err = set
            .write_event(&mut Writer::new(&mut out, &mut led), 1)
            .expect_err("no byte budget remains for the reference table");
        assert!(
            matches!(err, WriteError::Budget(e) if e.unit() == BudgetUnit::Bytes),
            "got {err:?}"
        );
        assert_eq!(out.len(), 0, "a refused event writes nothing");
    }

    #[test]
    fn pushing_more_fields_than_admitted_is_a_caller_bug() {
        let mut led = Open;
        let mut set = FieldSet::with_capacity(1, &mut led).expect("capacity 1");
        set.push(&mut led, 1, 7u16, |w, v| w.u16(v))
            .expect("first fits");
        let err = set
            .push(&mut led, 2, 8u16, |w, v| w.u16(v))
            .expect_err("second exceeds the admitted capacity");
        assert!(matches!(err, WriteError::Internal { .. }));
    }

    #[test]
    fn a_foreign_recognized_field_is_refused() {
        let policy = FieldPolicy::new(&[1, 2, 3], &[1]);
        // Build an event carrying tag 64 (a real, globally-registered tag).
        let fields = [Field::new(1, &[9]), Field::new(64, &[0, 0])];
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| {
            write_event(w, 16, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("framing");
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("decode");
        assert_eq!(
            check_policy(&event, &policy),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                event: 16,
                field: 64
            })),
            "a field from another family must be refused, not dropped"
        );
    }

    #[test]
    fn a_missing_required_field_is_refused_by_policy() {
        let policy = FieldPolicy::new(&[1, 2], &[1, 2]);
        let fields = [Field::new(1, &[9])];
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| {
            write_event(w, 16, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("framing");
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("decode");
        assert_eq!(
            check_policy(&event, &policy),
            Err(ReadError::Data(DecodeError::MissingField {
                event: 16,
                field: 2
            }))
        );
    }

    #[test]
    fn changed_fields_decoding_charges_bytes_and_entries() {
        // One element, value bytes present, zero byte budget.
        let cases: [(&str, &[u8], u16); 1] = [("one tag", &[1, 0, 0, 0, 9, 0], 9)];
        for (name, raw, expected) in cases {
            let mut led = NoBytes;
            let event_bytes = {
                let mut led2 = Open;
                let fields = [Field::new(13, raw)];
                encode_with(&mut led2, |w| {
                    write_event(w, 2, EVENT_FLAG_REQUIRED, &fields)
                })
                .expect("framing")
            };
            let mut reader = Reader::new(&event_bytes);
            let event = read_event(&mut reader, &mut Open).expect("decode");
            let err = decode_changed_fields(&event, 13, &mut led)
                .expect_err("element bytes must be charged");
            assert!(matches!(err, ReadError::Budget(_)), "{name}: got {err:?}");
            let _ = expected;
        }
    }

    #[test]
    fn changed_fields_rejects_an_impossible_count() {
        // Count 4 with two bytes present.
        let raw = [4u8, 0, 0, 0, 9, 0];
        let mut led = Open;
        let event_bytes = encode_with(&mut led, |w| {
            let fields = [Field::new(13, &raw)];
            write_event(w, 2, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("framing");
        let mut reader = Reader::new(&event_bytes);
        let event = read_event(&mut reader, &mut Open).expect("decode");
        assert!(matches!(
            decode_changed_fields(&event, 13, &mut Open),
            Err(ReadError::Data(DecodeError::Truncated { .. }))
        ));
    }

    #[test]
    fn reason_is_required_exactly_on_terminal_and_administrative_events() {
        for tag in [3u16, 18, 34, 35, 51, 81, 97, 147, 160, 208] {
            assert!(reason_required(tag), "event {tag} must require a reason");
        }
        for tag in [
            1u16, 2, 16, 17, 32, 33, 48, 49, 50, 80, 96, 112, 128, 144, 145, 146, 176, 192, 224,
        ] {
            assert!(!reason_required(tag), "event {tag} must not require one");
        }
    }
}
