//! Typed field values: how a record field's bytes are read and written.
//!
//! Every registered field has exactly one encoding, and this module is the only place
//! that knows it. Each function pairs a writer and a reader so an encoder/decoder pair
//! cannot drift apart: adding a field means adding both halves in one place.
//!
//! Two rules that keep the canonical form unique:
//!
//! * **Absence is the field not existing.** There is no zero value standing in for
//!   "not set", and no empty-string sentinel. A reader that sees a field treats its
//!   value as real.
//! * **Enums are decoded strictly.** An unknown discriminant is a refusal, never a
//!   fallback to the first variant, because a fallback would silently reinterpret
//!   authored data.
//!
//! Timestamps are `i64` microseconds supplied by the caller: a replay must never
//! re-invent them, so no clock is read anywhere in this crate.

use sage_db_types::confidence::Confidence;
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::persist::{
    ActorOrigin, Extensions, IdSpace, RecordKey, RecordKind, TermSource, Watermark,
};
use sage_db_types::relation::RelationId;

use crate::wire::{DecodeError, EncodeError, ReadError, Reader, WriteError, Writer};

/// Largest accepted slug, in bytes.
pub const MAX_SLUG_BYTES: u32 = 64;
/// Largest accepted atom, in bytes.
pub const MAX_ATOM_BYTES: u32 = 800;
/// Largest accepted identity string (principal, request id, legacy id), in bytes.
pub const MAX_IDENTITY_BYTES: u32 = 200;
/// Largest accepted KB name, in bytes.
pub const MAX_NAME_BYTES: u32 = 600;
/// Largest accepted description or reason, in bytes.
pub const MAX_TEXT_BYTES: u32 = 8000;
/// Largest accepted evidence title, in bytes.
pub const MAX_TITLE_BYTES: u32 = 1200;
/// Largest accepted locator, in bytes.
pub const MAX_LOCATOR_BYTES: u32 = 8000;
/// Largest accepted model identifier, in bytes.
pub const MAX_MODEL_ID_BYTES: u32 = 120;
/// Largest accepted `declared_captured_by` (tag 70), in bytes.
///
/// A document's declared capturer is a free-form attribution, not a verified principal,
/// so it gets its own 800-byte bound rather than the 200-byte identity limit.
pub const MAX_DECLARED_CAPTURED_BY: u32 = 800;
/// Largest accepted `declared_author` (tag 168), in bytes.
///
/// A document's declared author is free-form attribution, not a verified principal, so it
/// gets its own 800-byte bound rather than the 200-byte identity limit.
pub const MAX_DECLARED_AUTHOR: u32 = 800;

/// Largest accepted `principal_id` (tag 176), in bytes.
pub const MAX_PRINCIPAL_ID_BYTES: u32 = 120;

/// Largest accepted `request_id` (tag 177), in bytes.
///
/// Wider than `principal_id`: the request id is the caller's idempotency key, and the
/// protocol lets an idempotency key be longer than a principal name. Sharing the
/// 120-byte identity bound would refuse a legal request.
pub const MAX_REQUEST_ID_BYTES: u32 = 128;

/// Largest accepted `import_session_id` (tag 182), in bytes.
pub const MAX_SESSION_ID_BYTES: u32 = 64;
/// Largest accepted path, in bytes.
pub const MAX_PATH_BYTES: u32 = 2000;
/// Largest accepted extension key, in bytes.
pub const MAX_EXT_KEY_BYTES: u32 = 128;
/// Largest accepted extension value, in bytes.
pub const MAX_EXT_VALUE_BYTES: u32 = 4096;
/// Maximum domains on one term.
pub const MAX_DOMAINS: u32 = 64;
/// Maximum triggers on one term.
pub const MAX_TRIGGERS: u32 = 128;
/// Maximum byte length of a Term domain label.
pub const MAX_TERM_DOMAIN_BYTES: u32 = 128;
/// Maximum byte length of a Term activation trigger.
pub const MAX_TRIGGER_BYTES: u32 = 200;
/// Maximum revision baselines on one proposal.
pub const MAX_BASE_REVISIONS: u32 = 256;
/// Maximum concept refs on one frontier entry.
pub const MAX_CONCEPT_REFS: u32 = 64;
/// Maximum engine version used to detect misuse of the registry.
pub const MAX_DIMENSIONS: u32 = 4096;

/// Discriminant of `source` (tag 20).
fn source_to_u16(value: TermSource) -> u16 {
    match value {
        TermSource::Personal => 0,
        TermSource::Domain => 1,
        TermSource::Synthesis => 2,
    }
}

fn source_from_u16(value: u16) -> Option<TermSource> {
    TermSource::from_u16(value)
}

/// Discriminant of `confidence` (tag 21).
///
/// Delegates to the single declaration in `sage_db_types`: a second `match` here is
/// how the wire number and the lattice order drift apart.
fn confidence_to_u16(value: Confidence) -> u16 {
    value.as_u16()
}

fn confidence_from_u16(value: u16) -> Option<Confidence> {
    Confidence::from_u16(value)
}

/// Discriminant of `actor_origin` (tag 6).
fn actor_to_u16(value: ActorOrigin) -> u16 {
    match value {
        ActorOrigin::Authenticated => 0,
        ActorOrigin::LegacyDeclared => 1,
        ActorOrigin::Unavailable => 2,
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Write a record key: `kind:u16 | id:u128`.
///
/// # Errors
///
/// [`WriteError::Data`] when a term id does not fit `u32`: truncating would alias a
/// different term.
pub fn write_record_key(writer: &mut Writer<'_>, key: RecordKey) -> Result<(), WriteError> {
    if !key.id_is_in_range() {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: key.kind().as_u16(),
        }));
    }
    writer.u16(key.kind().as_u16())?;
    writer.u128(key.id())?;
    Ok(())
}

/// Write a revision as `u64`, rejecting zero.
///
/// # Errors
///
/// Cannot fail for a valid [`RecordRevision`]; the zero case is unrepresentable.
pub fn write_revision(writer: &mut Writer<'_>, revision: RecordRevision) -> Result<(), WriteError> {
    writer.u64(revision.get())
}

/// Write an optional string field, or nothing at all when absent.
///
/// # Errors
///
/// [`WriteError::Data`] when the value exceeds `max` bytes in UTF-8.
pub fn write_opt_str(
    writer: &mut Writer<'_>,
    value: Option<&str>,
    max: u32,
) -> Result<(), WriteError> {
    match value {
        Some(text) => {
            if u32::try_from(text.len()).map_or(true, |n| n > max) {
                return Err(WriteError::Data(EncodeError::StringTooLong));
            }
            writer.str(text)
        }
        None => Ok(()),
    }
}

/// Write a required string field.
///
/// # Errors
///
/// [`WriteError::Data`] when the value exceeds `max` bytes.
pub fn write_str(writer: &mut Writer<'_>, value: &str, max: u32) -> Result<(), WriteError> {
    // Checked conversion: a `usize` above `u32::MAX` must not wrap into a passing value.
    if u32::try_from(value.len()).map_or(true, |n| n > max) {
        return Err(WriteError::Data(EncodeError::StringTooLong));
    }
    writer.str(value)
}

/// Write a `[str]` field: count then each element in the given order.
///
/// The order is the caller's; domains and triggers are author sequences, so sorting them
/// would change the meaning rather than canonicalise it.
///
/// # Errors
///
/// [`WriteError::Data`] for too many elements or an over-long element.
pub fn write_str_list(
    writer: &mut Writer<'_>,
    values: &[String],
    max_items: u32,
    max_each: u32,
) -> Result<(), WriteError> {
    if u32::try_from(values.len()).map_or(true, |n| n > max_items) {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    writer.count(values.len())?;
    for value in values {
        write_str(writer, value, max_each)?;
    }
    Ok(())
}

/// Write the extensions map: count then key/value pairs in canonical (UTF-8) order.
///
/// # Errors
///
/// [`WriteError::Data`] when the map exceeds the field limits or is not in order.
pub fn write_extensions(writer: &mut Writer<'_>, ext: &Extensions) -> Result<(), WriteError> {
    let entries = ext.entries();
    if u32::try_from(entries.len()).map_or(true, |n| n > 32) {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    writer.count(entries.len())?;
    let mut previous: Option<&str> = None;
    for (key, value) in entries {
        if previous.is_some_and(|prev| prev.as_bytes() >= key.as_bytes()) {
            return Err(WriteError::Data(EncodeError::FieldOrder { tag: 14 }));
        }
        previous = Some(key);
        write_str(writer, key, MAX_EXT_KEY_BYTES)?;
        if u32::try_from(value.len()).map_or(true, |n| n > MAX_EXT_VALUE_BYTES) {
            return Err(WriteError::Data(EncodeError::BlobTooLong));
        }
        writer.blob(value)?;
    }
    Ok(())
}

/// Write a watermark: `space:u16 | hwm:u128`.
///
/// # Errors
///
/// Nothing but budget/allocation; every watermark value is representable.
pub fn write_watermark(writer: &mut Writer<'_>, mark: Watermark) -> Result<(), WriteError> {
    // Each space declares its own bound: term ids are u32, the other eight are u128.
    // Accepting a term watermark above `u32::MAX` would let the log claim it allocated an
    // id that a term key can never carry back.
    if mark.hwm > mark.space.max_id() {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: mark.space.as_u16(),
        }));
    }
    writer.u16(mark.space.as_u16())?;
    writer.u128(mark.hwm)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// Read a record key, validating that the id fits its kind.
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown kind or an out-of-range id.
pub fn read_record_key(reader: &mut Reader<'_>) -> Result<RecordKey, ReadError> {
    let kind_raw = reader.u16()?;
    let id = reader.u128()?;
    let kind = RecordKind::from_u16(kind_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 1,
        value: kind_raw,
    }))?;
    let key = RecordKey::new(kind, id);
    if !key.id_is_in_range() {
        return Err(ReadError::Data(DecodeError::IdOutOfRange {
            kind: kind_raw,
        }));
    }
    Ok(key)
}

/// Read a revision, rejecting zero.
///
/// # Errors
///
/// [`ReadError::Data`] when the value is zero: no record has revision 0.
pub fn read_revision(reader: &mut Reader<'_>) -> Result<RecordRevision, ReadError> {
    let raw = reader.u64()?;
    RecordRevision::new(raw).ok_or(ReadError::Data(DecodeError::InvalidRevision { value: raw }))
}

/// Copy `text` into an owned `String`, charging and reserving first.
///
/// Every owned string in this module goes through this function. `to_owned()` cannot be
/// used because it aborts the process on allocation failure; the caller needs a typed
/// [`ReadError::Allocation`] instead, so the reservation is explicit and fallible.
///
/// # Errors
///
/// [`ReadError::Budget`] when the text was not admitted, [`ReadError::Allocation`] when
/// the allocator refused, [`ReadError::Stopped`] from the control checkpoint.
fn owned_str(
    text: &str,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<String, ReadError> {
    let len = text.len();
    ledger.try_charge(len as u64)?;
    ledger.checkpoint()?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(len)
        .map_err(|_| ReadError::Allocation { requested: len })?;
    owned.push_str(text);
    Ok(owned)
}

/// Copy `bytes` into an owned `Vec<u8>`, charging and reserving first.
///
/// The `Vec` counterpart of [`owned_str`]; `to_vec()` would abort on failure.
///
/// # Errors
///
/// As [`owned_str`].
fn owned_bytes(
    bytes: &[u8],
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<Vec<u8>, ReadError> {
    let len = bytes.len();
    ledger.try_charge(len as u64)?;
    ledger.checkpoint()?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(len)
        .map_err(|_| ReadError::Allocation { requested: len })?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

/// Read a required string as an owned value, charging the ledger.
///
/// The text is charged before the copy, because the copy is the allocation. For a
/// borrowed view that allocates nothing, use [`read_str_borrowed`].
///
/// # Errors
///
/// [`ReadError::Data`] for over-length, NUL-bearing or non-UTF-8 input;
/// [`ReadError::Budget`] or [`ReadError::Stopped`] from the ledger.
pub fn read_str(
    reader: &mut Reader<'_>,
    max: u32,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<String, ReadError> {
    let borrowed = reader.str(max)?;
    owned_str(borrowed, ledger)
}

/// Validate a string without copying or charging.
///
/// This is the ledger-free path: it borrows the input, retains nothing, and therefore
/// owes no admission. Named for that property so a caller cannot mistake it for the
/// owned read, which does charge.
///
/// # Errors
///
/// [`ReadError::Data`] for over-length, NUL-bearing or non-UTF-8 input.
pub fn validate_str<'a>(reader: &mut Reader<'a>, max: u32) -> Result<&'a str, ReadError> {
    reader.str(max)
}

/// Read a string without copying or charging.
///
/// Named to make the absence of accounting obvious at the call site: the returned slice
/// borrows the input, so no memory is retained and no admission is owed.
///
/// # Errors
///
/// [`ReadError::Data`] for over-length, NUL-bearing or non-UTF-8 input.
pub fn read_str_borrowed<'a>(reader: &mut Reader<'a>, max: u32) -> Result<&'a str, ReadError> {
    reader.str(max)
}

/// Read an optional string whose presence the caller already established.
///
/// # Errors
///
/// As [`read_str`].
pub fn read_opt_str(
    reader: &mut Reader<'_>,
    max: u32,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<String, ReadError> {
    read_str(reader, max, ledger)
}

/// Read a `[str]` field.
///
/// The count is checked against the collection limit **before** the vector is reserved,
/// so a hostile count cannot become an allocation.
///
/// # Errors
///
/// [`ReadError::Data`] for an impossible count or a bad element.
pub fn read_str_list(
    reader: &mut Reader<'_>,
    max_items: u32,
    max_each: u32,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<Vec<String>, ReadError> {
    let count = reader.count(max_items)?;
    // Feasibility first: each element needs at least its 4-byte length prefix.
    let minimum = (count as usize).saturating_mul(4);
    if minimum > reader.remaining() {
        return Err(ReadError::Data(DecodeError::Truncated {
            offset: reader.offset(),
            wanted: minimum,
        }));
    }
    ledger.try_charge_entries(count as u64)?;
    // The element table is retained memory: charge its capacity, not just its length.
    let table_bytes = (count as u64).saturating_mul(core::mem::size_of::<String>() as u64);
    ledger.try_charge(table_bytes)?;
    ledger.checkpoint()?;
    let mut out = Vec::new();
    out.try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            requested: table_bytes as usize,
        })?;
    for _ in 0..count {
        // Each element's text is charged by `read_str` before its own copy.
        out.push(read_str(reader, max_each, ledger)?);
    }
    // Exit checkpoint: an empty list must still observe a cancelled attempt.
    ledger.checkpoint()?;
    Ok(out)
}

/// Read the extensions map, validating order and uniqueness.
///
/// # Errors
///
/// [`ReadError::Data`] for out-of-order or duplicate keys, or a bad entry.
pub fn read_extensions(
    reader: &mut Reader<'_>,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<Extensions, ReadError> {
    let count = reader.count(32)?;
    // Feasibility: each entry needs at least a 4-byte key length and a 4-byte value
    // length before any bytes of content.
    let minimum = (count as usize).saturating_mul(8);
    if minimum > reader.remaining() {
        return Err(ReadError::Data(DecodeError::Truncated {
            offset: reader.offset(),
            wanted: minimum,
        }));
    }
    ledger.try_charge_entries(count as u64)?;
    // The retained slot array only. Key and value bytes are charged separately per
    // entry, so this is an exact slot count rather than a conservative key allowance
    // that would double-count `MAX_EXT_KEY_BYTES` on top of each key's real length.
    let table_bytes =
        (count as u64).saturating_mul(core::mem::size_of::<(String, Vec<u8>)>() as u64);
    ledger.try_charge(table_bytes)?;
    ledger.checkpoint()?;

    // Collect into an owned, already-ordered vector and hand it over once: no hidden
    // `insert` copy and no re-sorting inside the container.
    let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
    pairs
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            requested: table_bytes as usize,
        })?;
    for _ in 0..count {
        let key = read_str(reader, MAX_EXT_KEY_BYTES, ledger)?;
        // Compare against the previously stored key in place: cloning it into a
        // separate `previous` slot would allocate a second copy of every key.
        if pairs
            .last()
            .is_some_and(|(prev, _): &(String, Vec<u8>)| prev.as_bytes() >= key.as_bytes())
        {
            return Err(ReadError::Data(DecodeError::FieldOrder { tag: 14 }));
        }
        let value_len = reader.u32()?;
        if value_len > MAX_EXT_VALUE_BYTES {
            return Err(ReadError::Data(DecodeError::StringTooLong {
                len: value_len,
                max: MAX_EXT_VALUE_BYTES,
            }));
        }
        let raw = reader.take_exact(value_len as usize)?;
        let value = owned_bytes(raw, ledger)?;
        pairs.push((key, value));
    }
    // Exit checkpoint before handing ownership over: an empty or fully-read map must
    // still observe a cancelled attempt rather than returning a value.
    ledger.checkpoint()?;
    Extensions::from_canonical(pairs).map_err(|_| ReadError::Data(DecodeError::InvalidExtensionKey))
}

/// Read a watermark.
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown allocation space.
pub fn read_watermark(reader: &mut Reader<'_>) -> Result<Watermark, ReadError> {
    let space_raw = reader.u16()?;
    let space = IdSpace::from_u16(space_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 164,
        value: space_raw,
    }))?;
    let hwm = reader.u128()?;
    if hwm > space.max_id() {
        return Err(ReadError::Data(DecodeError::WatermarkOutOfRange {
            space: space.as_u16(),
            value: hwm,
        }));
    }
    Ok(Watermark { space, hwm })
}

/// Read `source` (tag 20).
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown discriminant.
pub fn read_source(reader: &mut Reader<'_>) -> Result<TermSource, ReadError> {
    let raw = reader.u16()?;
    source_from_u16(raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 20,
        value: raw,
    }))
}

/// Read `confidence` (tag 21).
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown discriminant.
pub fn read_confidence(reader: &mut Reader<'_>) -> Result<Confidence, ReadError> {
    let raw = reader.u16()?;
    confidence_from_u16(raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 21,
        value: raw,
    }))
}

/// Read `layer` (tag 18).
///
/// # Errors
///
/// [`ReadError::Data`] for a layer outside 0..=3.
pub fn read_layer(reader: &mut Reader<'_>) -> Result<SemanticLayer, ReadError> {
    let raw = reader.u8()?;
    SemanticLayer::from_u8(raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 18,
        value: u16::from(raw),
    }))
}

/// Read `relation` (tag 34), a `u16`.
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown relation.
pub fn read_relation(reader: &mut Reader<'_>) -> Result<RelationId, ReadError> {
    let raw = reader.u16()?;
    let narrow = u8::try_from(raw).map_err(|_| {
        ReadError::Data(DecodeError::UnknownEnum {
            tag: 34,
            value: raw,
        })
    })?;
    RelationId::from_u8(narrow).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 34,
        value: raw,
    }))
}

/// Read `actor_origin` (tag 6).
///
/// # Errors
///
/// [`ReadError::Data`] for an unknown discriminant.
pub fn read_actor_origin(reader: &mut Reader<'_>) -> Result<ActorOrigin, ReadError> {
    let raw = reader.u16()?;
    ActorOrigin::from_u16(raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: 6,
        value: raw,
    }))
}

/// Write `source` (tag 20).
///
/// # Errors
///
/// Budget or allocation refusals from the writer.
pub fn write_source(writer: &mut Writer<'_>, value: TermSource) -> Result<(), WriteError> {
    writer.u16(source_to_u16(value))
}

/// Write `confidence` (tag 21).
///
/// # Errors
///
/// Budget or allocation refusals from the writer.
pub fn write_confidence(writer: &mut Writer<'_>, value: Confidence) -> Result<(), WriteError> {
    writer.u16(confidence_to_u16(value))
}

/// Write `layer` (tag 18).
///
/// # Errors
///
/// Budget or allocation refusals from the writer.
pub fn write_layer(writer: &mut Writer<'_>, value: SemanticLayer) -> Result<(), WriteError> {
    writer.u8(value.get())
}

/// Write `relation` (tag 34).
///
/// `u16` on the wire: `storage-format` §6 assigns tag 34 a `u16` width. Only `layer`
/// (tag 18) is a `u8` in the enum-valued fields.
///
/// # Errors
///
/// Budget or allocation refusals from the writer.
pub fn write_relation(writer: &mut Writer<'_>, value: RelationId) -> Result<(), WriteError> {
    writer.u16(u16::from(value.get()))
}

/// Write `actor_origin` (tag 6).
///
/// # Errors
///
/// Budget or allocation refusals from the writer.
pub fn write_actor_origin(writer: &mut Writer<'_>, value: ActorOrigin) -> Result<(), WriteError> {
    writer.u16(actor_to_u16(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::{BudgetError, SharedLedger};
    use sage_db_types::query::ControlStop;

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

    fn roundtrip<T>(
        write: impl Fn(&mut Writer<'_>) -> Result<(), WriteError>,
        read: impl Fn(&mut Reader<'_>) -> Result<T, ReadError>,
    ) -> (Vec<u8>, Result<T, ReadError>) {
        let mut led = Open;
        let bytes = crate::event::encode_with(&mut led, write).expect("encode");
        let mut reader = Reader::new(&bytes);
        let value = read(&mut reader);
        assert!(reader.is_empty(), "value must consume its whole input");
        (bytes, value)
    }

    /// Explicit wire width per enum, asserted against literal bytes rather than against
    /// a round-trip. A self-consistent wrong width round-trips perfectly, which is
    /// exactly how the `u8` relation survived.
    #[test]
    fn enum_wire_widths_are_explicit() {
        let mut led = Open;
        for relation in RelationId::ALL {
            let bytes = crate::event::encode_with(&mut led, |w| write_relation(w, relation))
                .expect("encode");
            assert_eq!(
                bytes,
                u16::from(relation.get()).to_le_bytes(),
                "relation {:?} must be u16 on the wire",
                relation
            );
        }
        // layer is the documented u8 exception.
        for layer in SemanticLayer::ALL {
            let bytes =
                crate::event::encode_with(&mut led, |w| write_layer(w, layer)).expect("encode");
            assert_eq!(bytes, vec![layer.get()], "layer must be u8 on the wire");
        }
        for source in [
            TermSource::Personal,
            TermSource::Domain,
            TermSource::Synthesis,
        ] {
            let bytes =
                crate::event::encode_with(&mut led, |w| write_source(w, source)).expect("encode");
            assert_eq!(bytes.len(), 2, "source is u16");
        }
        for confidence in [
            Confidence::Contested,
            Confidence::Hypothetical,
            Confidence::Confirmed,
        ] {
            let bytes = crate::event::encode_with(&mut led, |w| write_confidence(w, confidence))
                .expect("encode");
            assert_eq!(bytes.len(), 2, "confidence is u16");
        }
        for actor in [
            ActorOrigin::Authenticated,
            ActorOrigin::LegacyDeclared,
            ActorOrigin::Unavailable,
        ] {
            let bytes = crate::event::encode_with(&mut led, |w| write_actor_origin(w, actor))
                .expect("encode");
            assert_eq!(bytes.len(), 2, "actor_origin is u16");
        }
    }

    #[test]
    fn nul_is_refused_on_both_sides_for_every_string_path() {
        // Write side: required, optional, list element, extension key.
        let mut led = Open;
        assert!(matches!(
            crate::event::encode_with(&mut led, |w| write_str(w, "a\0b", 64)),
            Err(WriteError::Data(EncodeError::NulInString))
        ));
        assert!(matches!(
            crate::event::encode_with(&mut led, |w| write_opt_str(w, Some("a\0b"), 64)),
            Err(WriteError::Data(EncodeError::NulInString))
        ));
        let bad_list = vec!["ok".to_owned(), "a\0b".to_owned()];
        assert!(
            crate::event::encode_with(&mut led, |w| write_str_list(w, &bad_list, MAX_DOMAINS, 128))
                .is_err()
        );

        // Read side, hand-built bytes containing a NUL.
        let raw = [3u8, 0, 0, 0, b'a', 0, b'b'];
        assert!(matches!(
            read_str_borrowed(&mut Reader::new(&raw), 64),
            Err(ReadError::Data(DecodeError::NulInString))
        ));
    }

    /// Every owned field collection charges before it allocates, in both units.
    #[test]
    fn owned_collections_are_metered() {
        struct NoBytes;
        impl SharedLedger for NoBytes {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                if bytes == 0 {
                    Ok(())
                } else {
                    Err(BudgetError::new(
                        sage_db_types::ledger::BudgetUnit::Bytes,
                        bytes,
                        0,
                    ))
                }
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        // list: one EMPTY element. Its text costs nothing to copy, so a refusal here
        // can only come from the retained element table - which is what the mutation
        // that dropped the table charge would otherwise hide.
        let raw = [1u8, 0, 0, 0, 0u8, 0, 0, 0];
        let err = read_str_list(&mut Reader::new(&raw), 64, 128, &mut NoBytes)
            .expect_err("the element table must be charged even for empty text");
        assert!(matches!(err, ReadError::Budget(_)), "got {err:?}");

        // single owned string, zero byte budget
        let raw = [1u8, 0, 0, 0, b'x'];
        let err = read_str(&mut Reader::new(&raw), 64, &mut NoBytes)
            .expect_err("the copy of the text must be charged");
        assert!(matches!(err, ReadError::Budget(_)), "got {err:?}");

        // extensions, zero byte budget
        let mut raw = Vec::new();
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&2u32.to_le_bytes());
        raw.extend_from_slice(b"aa");
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.push(b'v');
        let err = read_extensions(&mut Reader::new(&raw), &mut NoBytes)
            .expect_err("extension keys and values must be charged");
        assert!(matches!(err, ReadError::Budget(_)), "got {err:?}");
    }

    #[test]
    fn term_watermarks_are_bounded_but_other_spaces_are_not() {
        let mut led = Open;
        let too_big = Watermark {
            space: IdSpace::TermId,
            hwm: u128::from(u32::MAX) + 1,
        };
        assert!(matches!(
            crate::event::encode_with(&mut led, |w| write_watermark(w, too_big)),
            Err(WriteError::Data(EncodeError::IdOutOfRange { .. }))
        ));

        // The other eight spaces keep the full u128 range.
        for space in IdSpace::ALL {
            if space == IdSpace::TermId {
                continue;
            }
            let mark = Watermark {
                space,
                hwm: u128::MAX,
            };
            let bytes = crate::event::encode_with(&mut led, |w| write_watermark(w, mark))
                .expect("full range must be accepted");
            assert_eq!(read_watermark(&mut Reader::new(&bytes)).unwrap(), mark);
        }

        // And decode refuses an out-of-range term watermark from hand-built bytes.
        let mut raw = Vec::new();
        raw.extend_from_slice(&0u16.to_le_bytes()); // TermId
        raw.extend_from_slice(&(u128::from(u32::MAX) + 1).to_le_bytes());
        assert!(matches!(
            read_watermark(&mut Reader::new(&raw)),
            Err(ReadError::Data(DecodeError::WatermarkOutOfRange { .. }))
        ));
    }

    #[test]
    fn extension_errors_name_the_actual_problem() {
        // Key grammar, not "unknown enum".
        let mut raw = Vec::new();
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&4u32.to_le_bytes());
        raw.extend_from_slice(b"BAD!");
        raw.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            read_extensions(&mut Reader::new(&raw), &mut Open),
            Err(ReadError::Data(DecodeError::InvalidExtensionKey))
        ));

        // An impossible count is refused before any reservation.
        let mut raw = Vec::new();
        raw.extend_from_slice(&32u32.to_le_bytes());
        assert!(matches!(
            read_extensions(&mut Reader::new(&raw), &mut Open),
            Err(ReadError::Data(DecodeError::Truncated { .. }))
        ));
    }

    #[test]
    fn record_keys_round_trip_and_reject_out_of_range_terms() {
        let key = RecordKey::term(7);
        let (_, decoded) = roundtrip(|w| write_record_key(w, key), read_record_key);
        assert_eq!(decoded.unwrap(), key);

        // A term id above u32 is not representable and must be refused, not truncated.
        let too_big = RecordKey::new(RecordKind::Term, u128::from(u32::MAX) + 1);
        let mut led = Open;
        let err = crate::event::encode_with(&mut led, |w| write_record_key(w, too_big))
            .expect_err("out of range term id");
        assert!(matches!(
            err,
            WriteError::Data(EncodeError::IdOutOfRange { .. })
        ));
    }

    #[test]
    fn revisions_reject_zero() {
        let mut led = Open;
        let bytes =
            crate::event::encode_with(&mut led, |w| write_revision(w, RecordRevision::FIRST))
                .expect("encode");
        assert_eq!(
            read_revision(&mut Reader::new(&bytes)).unwrap(),
            RecordRevision::FIRST
        );

        let zero = [0u8; 8];
        assert!(matches!(
            read_revision(&mut Reader::new(&zero)),
            Err(ReadError::Data(DecodeError::InvalidRevision { value: 0 }))
        ));
    }

    #[test]
    fn absent_strings_write_nothing_at_all() {
        let mut led = Open;
        let present = crate::event::encode_with(&mut led, |w| {
            write_opt_str(w, Some("x"), MAX_IDENTITY_BYTES)
        })
        .expect("encode");
        let absent =
            crate::event::encode_with(&mut led, |w| write_opt_str(w, None, MAX_IDENTITY_BYTES))
                .expect("encode");
        assert_eq!(
            absent.len(),
            0,
            "absence must be zero bytes, not a sentinel"
        );
        assert!(present.len() > absent.len());
    }

    #[test]
    fn string_lists_keep_author_order() {
        let values = vec!["zeta".to_owned(), "alpha".to_owned()];
        let (_, decoded) = roundtrip(
            |w| write_str_list(w, &values, MAX_DOMAINS, 128),
            |r| read_str_list(r, MAX_DOMAINS, 128, &mut Open),
        );
        assert_eq!(decoded.unwrap(), values, "author order is not sorted");
    }

    #[test]
    fn a_hostile_list_count_is_refused_before_reserving() {
        // Count 64 with no element bytes at all.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&64u32.to_le_bytes());
        let mut reader = Reader::new(&bytes);
        assert!(matches!(
            read_str_list(&mut reader, MAX_DOMAINS, 128, &mut Open),
            Err(ReadError::Data(DecodeError::Truncated { .. }))
        ));
    }

    #[test]
    fn extensions_round_trip_in_canonical_order() {
        let mut ext = Extensions::new();
        ext.insert("legacy.provider", b"alpha".to_vec()).unwrap();
        ext.insert("legacy.dimensions", b"4".to_vec()).unwrap();
        let (_, decoded) = roundtrip(
            |w| write_extensions(w, &ext),
            |r| read_extensions(r, &mut Open),
        );
        let decoded = decoded.unwrap();
        let keys: Vec<&str> = decoded.entries().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["legacy.dimensions", "legacy.provider"]);
    }

    #[test]
    fn out_of_order_extensions_are_refused_on_both_sides() {
        // Decode side: hand-built keys in descending order.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        for key in ["zz", "aa"] {
            bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut reader = Reader::new(&bytes);
        assert!(matches!(
            read_extensions(&mut reader, &mut Open),
            Err(ReadError::Data(DecodeError::FieldOrder { tag: 14 }))
        ));
    }

    #[test]
    fn unknown_enum_values_are_refused_not_defaulted() {
        let mut reader = Reader::new(&[9u8, 0]);
        assert!(matches!(
            read_source(&mut reader),
            Err(ReadError::Data(DecodeError::UnknownEnum {
                tag: 20,
                value: 9
            }))
        ));
        let mut reader = Reader::new(&[9u8, 0]);
        assert!(matches!(
            read_confidence(&mut reader),
            Err(ReadError::Data(DecodeError::UnknownEnum { tag: 21, .. }))
        ));
        let mut reader = Reader::new(&[9u8]);
        assert!(matches!(
            read_layer(&mut reader),
            Err(ReadError::Data(DecodeError::UnknownEnum { tag: 18, .. }))
        ));
        let mut reader = Reader::new(&[9u8, 0]);
        assert!(matches!(
            read_relation(&mut reader),
            Err(ReadError::Data(DecodeError::UnknownEnum { tag: 34, .. }))
        ));
        let mut reader = Reader::new(&[9u8, 0]);
        assert!(matches!(
            read_actor_origin(&mut reader),
            Err(ReadError::Data(DecodeError::UnknownEnum { tag: 6, .. }))
        ));
    }

    #[test]
    fn watermarks_round_trip_with_the_full_range() {
        let mark = Watermark {
            space: IdSpace::AssertionId,
            hwm: u128::MAX - 5,
        };
        let (_, decoded) = roundtrip(|w| write_watermark(w, mark), read_watermark);
        assert_eq!(decoded.unwrap(), mark);
    }

    #[test]
    fn every_enum_value_round_trips() {
        for value in [
            TermSource::Personal,
            TermSource::Domain,
            TermSource::Synthesis,
        ] {
            let (_, decoded) = roundtrip(|w| write_source(w, value), read_source);
            assert_eq!(decoded.unwrap(), value);
        }
        for value in [
            Confidence::Contested,
            Confidence::Hypothetical,
            Confidence::Confirmed,
        ] {
            let (_, decoded) = roundtrip(|w| write_confidence(w, value), read_confidence);
            assert_eq!(decoded.unwrap(), value);
        }
        for value in SemanticLayer::ALL {
            let (_, decoded) = roundtrip(|w| write_layer(w, value), read_layer);
            assert_eq!(decoded.unwrap(), value);
        }
        for value in RelationId::ALL {
            let (_, decoded) = roundtrip(|w| write_relation(w, value), read_relation);
            assert_eq!(decoded.unwrap(), value);
        }
        for value in [
            ActorOrigin::Authenticated,
            ActorOrigin::LegacyDeclared,
            ActorOrigin::Unavailable,
        ] {
            let (_, decoded) = roundtrip(|w| write_actor_origin(w, value), read_actor_origin);
            assert_eq!(decoded.unwrap(), value);
        }
    }
}
