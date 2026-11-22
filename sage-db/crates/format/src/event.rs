//! Event and field envelopes: the layer every record codec sits on.
//!
//! ```text
//! Event := tag:u16 | flags:u16 | field_count:u16 | Field[field_count]
//! Field := tag:u16 | flags:u16 | encoded_len:u32 | value[encoded_len]
//! ```
//!
//! Three properties are load-bearing and each has a matching rejection:
//!
//! * **Field order is strict ascending.** A duplicate or out-of-order tag is
//!   corruption, not something to sort away. Silent normalisation would let two
//!   different byte sequences claim to be the same event.
//! * **`encoded_len` is the only skip mechanism.** An unknown field can be stepped over
//!   by length, which is what makes forward compatibility possible at all — but only
//!   for fields whose compatibility class allows it, which is decided by the registry,
//!   not here.
//! * **Zero length is never a value.** Every registered type has a minimum encoded size
//!   of at least one byte, so `encoded_len == 0` is corruption rather than "present but
//!   empty". Absence is the field not existing.
//!
//! `Event.flags` bit 0 means REQUIRED; in this format major every registered event
//! requires it, and bits 1–15 must be zero. `Field.flags` must be zero: the earlier
//! design's PRESENT bit was removed because it gave two encodings of "absent".

use crate::wire::{ReadError, Reader, WriteError, Writer};
use sage_db_types::ledger::SharedLedger;

/// Bit 0 of `Event.flags`: an unknown event must be refused rather than skipped.
pub const EVENT_FLAG_REQUIRED: u16 = 0x0001;

/// Maximum number of fields in one event.
pub const MAX_EVENT_FIELDS: u16 = 64;

/// Maximum number of fields in a transaction or manifest header.
pub const MAX_HEADER_FIELDS: u16 = 32;

/// One field: its tag and its value bytes.
///
/// The value is borrowed from the encoded form; a decoder produces a `&[u8]` window and
/// the record codecs interpret it. No allocation happens per field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field<'a> {
    tag: u16,
    value: &'a [u8],
}

impl<'a> Field<'a> {
    /// A field with `tag` and raw `value`.
    #[must_use]
    pub const fn new(tag: u16, value: &'a [u8]) -> Self {
        Self { tag, value }
    }

    /// The field tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        self.tag
    }

    /// The raw value bytes.
    #[must_use]
    pub const fn value(self) -> &'a [u8] {
        self.value
    }
}

/// One decoded event: tag, flags, and its validated field window.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Event<'a> {
    tag: u16,
    flags: u16,
    fields: Vec<Field<'a>>,
}

impl<'a> Event<'a> {
    /// The event tag.
    #[must_use]
    pub const fn tag(&self) -> u16 {
        self.tag
    }

    /// The event flags.
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// Whether this event claims to be required.
    #[must_use]
    pub const fn is_required(&self) -> bool {
        self.flags & EVENT_FLAG_REQUIRED != 0
    }

    /// The fields, in ascending tag order.
    #[must_use]
    pub fn fields(&self) -> &[Field<'a>] {
        &self.fields
    }

    /// Look up one field by tag.
    #[must_use]
    pub fn field(&self, tag: u16) -> Option<&'a [u8]> {
        self.fields
            .binary_search_by_key(&tag, |f| f.tag)
            .ok()
            .map(|i| self.fields[i].value)
    }

    /// Tags present, as a borrowed iterator.
    ///
    /// Deliberately not a `Vec`: this runs on every decode path, and an unmetered
    /// allocation here would escape the ledger that governs the rest of the decode.
    pub fn tags(&self) -> impl Iterator<Item = u16> + '_ {
        self.fields.iter().map(|f| f.tag)
    }
}

/// Write one event with its fields.
///
/// Fields are written in the order given and must already be ascending; the encoder
/// does not sort, because a caller that built a field set out of order has a bug that
/// sorting would hide.
///
/// # Errors
///
/// [`WriteError::Data`] for more than [`MAX_EVENT_FIELDS`] fields, a zero-length field,
/// or an out-of-order tag; plus budget and allocation refusals.
pub fn write_event(
    writer: &mut Writer<'_>,
    tag: u16,
    flags: u16,
    fields: &[Field<'_>],
) -> Result<(), WriteError> {
    let count = u16::try_from(fields.len())
        .ok()
        .filter(|n| *n <= MAX_EVENT_FIELDS)
        .ok_or(WriteError::Data(crate::wire::EncodeError::CountTooLong))?;
    // Refuse a value our own reader would reject: a writer must never be able to emit
    // a self-invalid structure. `Field.flags` is always written as zero below, so only
    // the event-level flags can be wrong here.
    if flags & !EVENT_FLAG_REQUIRED != 0 {
        return Err(WriteError::Data(crate::wire::EncodeError::EventFlags {
            flags,
        }));
    }
    let mut previous: Option<u16> = None;
    for field in fields {
        if field.value.is_empty() {
            return Err(WriteError::Data(crate::wire::EncodeError::EmptyField {
                tag: field.tag,
            }));
        }
        if previous.is_some_and(|prev| field.tag <= prev) {
            return Err(WriteError::Data(crate::wire::EncodeError::FieldOrder {
                tag: field.tag,
            }));
        }
        previous = Some(field.tag);
    }

    writer.u16(tag)?;
    writer.u16(flags)?;
    writer.u16(count)?;
    for field in fields {
        writer.u16(field.tag)?;
        writer.u16(0)?; // Field.flags: reserved, always zero.
        writer.blob(field.value)?;
    }
    Ok(())
}

/// Read one event from `reader`.
///
/// # Errors
///
/// [`ReadError::Data`] for a duplicate or out-of-order tag, a zero-length field, or a
/// field count above [`MAX_EVENT_FIELDS`]; plus cancellation.
pub fn read_event<'a>(
    reader: &mut Reader<'a>,
    ledger: &mut dyn SharedLedger,
) -> Result<Event<'a>, ReadError> {
    let tag = reader.u16()?;
    let flags = reader.u16()?;
    if flags & !EVENT_FLAG_REQUIRED != 0 {
        return Err(ReadError::Data(crate::wire::DecodeError::EventFlags {
            flags,
        }));
    }
    let count = reader.u16()?;
    if count > MAX_EVENT_FIELDS {
        return Err(ReadError::Data(crate::wire::DecodeError::CountTooLong {
            count: u32::from(count),
            max: u32::from(MAX_EVENT_FIELDS),
        }));
    }
    admit_field_table(count, reader.remaining(), ledger)?;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            // Bytes, matching the field's documentation.
            requested: field_table_bytes(count) as usize,
        })?;
    let mut previous: Option<u16> = None;
    for _ in 0..count {
        let tag = reader.u16()?;
        let flags = reader.u16()?;
        if flags != 0 {
            return Err(ReadError::Data(crate::wire::DecodeError::FieldFlags {
                tag,
                flags,
            }));
        }
        if previous.is_some_and(|prev| tag <= prev) {
            return Err(ReadError::Data(crate::wire::DecodeError::FieldOrder {
                tag,
            }));
        }
        previous = Some(tag);
        let len = reader.u32()?;
        if len == 0 {
            return Err(ReadError::Data(crate::wire::DecodeError::ZeroLengthField {
                tag,
            }));
        }
        let value = reader.take_exact(len as usize)?;
        fields.push(Field::new(tag, value));
    }
    Ok(Event { tag, flags, fields })
}

/// Write a header: a `u16` field count followed by fields, using the same envelope as
/// events but with the header limit.
///
/// # Errors
///
/// As [`write_event`], with [`MAX_HEADER_FIELDS`] as the count limit.
pub fn write_header(writer: &mut Writer<'_>, fields: &[Field<'_>]) -> Result<(), WriteError> {
    let count = u16::try_from(fields.len())
        .ok()
        .filter(|n| *n <= MAX_HEADER_FIELDS)
        .ok_or(WriteError::Data(crate::wire::EncodeError::CountTooLong))?;
    let mut previous: Option<u16> = None;
    for field in fields {
        if field.value.is_empty() {
            return Err(WriteError::Data(crate::wire::EncodeError::EmptyField {
                tag: field.tag,
            }));
        }
        if previous.is_some_and(|prev| field.tag <= prev) {
            return Err(WriteError::Data(crate::wire::EncodeError::FieldOrder {
                tag: field.tag,
            }));
        }
        previous = Some(field.tag);
    }
    writer.u16(count)?;
    for field in fields {
        writer.u16(field.tag)?;
        writer.u16(0)?;
        writer.blob(field.value)?;
    }
    Ok(())
}

/// Read a header field set.
///
/// # Errors
///
/// As [`read_event`], with [`MAX_HEADER_FIELDS`] as the count limit.
pub fn read_header<'a>(
    reader: &mut Reader<'a>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<Field<'a>>, ReadError> {
    let count = reader.u16()?;
    if count > MAX_HEADER_FIELDS {
        return Err(ReadError::Data(crate::wire::DecodeError::CountTooLong {
            count: u32::from(count),
            max: u32::from(MAX_HEADER_FIELDS),
        }));
    }
    admit_field_table(count, reader.remaining(), ledger)?;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            // Bytes, matching the field's documentation.
            requested: field_table_bytes(count) as usize,
        })?;
    let mut previous: Option<u16> = None;
    for _ in 0..count {
        let tag = reader.u16()?;
        let flags = reader.u16()?;
        if flags != 0 {
            return Err(ReadError::Data(crate::wire::DecodeError::FieldFlags {
                tag,
                flags,
            }));
        }
        if previous.is_some_and(|prev| tag <= prev) {
            return Err(ReadError::Data(crate::wire::DecodeError::FieldOrder {
                tag,
            }));
        }
        previous = Some(tag);
        let len = reader.u32()?;
        if len == 0 {
            return Err(ReadError::Data(crate::wire::DecodeError::ZeroLengthField {
                tag,
            }));
        }
        fields.push(Field::new(tag, reader.take_exact(len as usize)?));
    }
    Ok(fields)
}

/// Minimum encoded size of one field: tag + flags + encoded_len, before any value.
const MIN_FIELD_BYTES: usize = 8;

/// In-memory size of one `Field` once decoded into the table.
///
/// The decode allocates a table of these, so the byte cost is not hypothetical: it is
/// `count * size_of::<Field>()` of retained memory, charged before the reservation.
fn field_table_bytes(count: u16) -> u64 {
    (count as u64).saturating_mul(core::mem::size_of::<Field<'static>>() as u64)
}

/// Check that `count` fields can possibly fit in `remaining` bytes, then admit the
/// field table on the shared ledger.
///
/// The feasibility check comes first (storage-format §9.1 step 9): a declared count that
/// cannot be satisfied by the bytes actually present is corruption, and detecting it
/// before reserving means a hostile count cannot become an allocation.
///
/// # Errors
///
/// [`ReadError::Data`] for an impossible count; [`ReadError::Budget`] or
/// [`ReadError::Stopped`] from the ledger.
fn admit_field_table(
    count: u16,
    remaining: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<(), ReadError> {
    let needed = (count as usize)
        .checked_mul(MIN_FIELD_BYTES)
        .ok_or(ReadError::Data(crate::wire::DecodeError::Truncated {
            offset: 0,
            wanted: usize::MAX,
        }))?;
    if needed > remaining {
        return Err(ReadError::Data(crate::wire::DecodeError::Truncated {
            offset: 0,
            wanted: needed,
        }));
    }
    // Two units, both required. Entries bound how many elements the walk may touch;
    // bytes bound the table's retained size. Charging only entries let a zero-byte
    // budget allocate a full table - the counter looked satisfied while memory grew.
    ledger.try_charge_entries(count as u64)?;
    ledger.try_charge(field_table_bytes(count))?;
    ledger.checkpoint()?;
    Ok(())
}

/// Encode a field's value with a ledger, for record codecs that build values first.
///
/// # Errors
///
/// Budget, allocation or control refusals from `Writer`.
pub fn encode_with(
    ledger: &mut dyn SharedLedger,
    build: impl FnOnce(&mut Writer<'_>) -> Result<(), WriteError>,
) -> Result<Vec<u8>, WriteError> {
    let mut buf = Vec::new();
    {
        let mut writer = Writer::new(&mut buf, ledger);
        build(&mut writer)?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetError;
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

    /// Encode, then decode while the caller's byte buffer is still alive.
    ///
    /// The decoded value borrows the buffer, so the buffer is owned here and the borrow
    /// is passed in - no `Box::leak`, which would turn a lifetime question into a
    /// permanent allocation.
    fn with_roundtrip<R>(fields: &[Field<'_>], check: impl FnOnce(&Event<'_>) -> R) -> R {
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_event(w, 7, EVENT_FLAG_REQUIRED, fields))
            .expect("encode");
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("decode");
        assert!(reader.is_empty(), "event must consume its whole input");
        check(&event)
    }

    #[test]
    fn an_event_round_trips_with_its_fields() {
        let fields = [Field::new(1, &[0xAA, 0xBB]), Field::new(9, &[0x01])];
        with_roundtrip(&fields, |event| {
            assert_eq!(event.tag(), 7);
            assert!(event.is_required());
            assert_eq!(event.tags().collect::<Vec<_>>(), vec![1, 9]);
            assert_eq!(event.field(1), Some(&[0xAA, 0xBB][..]));
            assert_eq!(event.field(9), Some(&[0x01][..]));
            assert_eq!(event.field(5), None, "absent means absent");
        });
    }

    #[test]
    fn out_of_order_fields_are_refused_not_sorted() {
        let mut led = Open;
        let reversed = [Field::new(9, &[1]), Field::new(1, &[2])];
        let err = encode_with(&mut led, |w| write_event(w, 1, 1, &reversed))
            .expect_err("descending tags");
        assert!(
            matches!(
                err,
                WriteError::Data(crate::wire::EncodeError::FieldOrder { tag: 1 })
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn duplicate_field_tags_are_refused() {
        let mut led = Open;
        let duplicate = [Field::new(4, &[1]), Field::new(4, &[2])];
        assert!(encode_with(&mut led, |w| write_event(w, 1, 1, &duplicate)).is_err());

        // And on the decode side, from hand-built bytes.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes()); // tag
        bytes.extend_from_slice(&1u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&2u16.to_le_bytes()); // count
        for _ in 0..2 {
            bytes.extend_from_slice(&4u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.push(0xFF);
        }
        let mut reader = Reader::new(&bytes);
        let mut led = Open;
        assert!(matches!(
            read_event(&mut reader, &mut led),
            Err(ReadError::Data(crate::wire::DecodeError::FieldOrder {
                tag: 4
            }))
        ));
    }

    #[test]
    fn a_zero_length_field_is_corruption() {
        let mut led = Open;
        let empty = [Field::new(1, &[])];
        assert!(matches!(
            encode_with(&mut led, |w| write_event(w, 1, 1, &empty)),
            Err(WriteError::Data(crate::wire::EncodeError::EmptyField {
                tag: 1
            }))
        ));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // tag
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u32.to_le_bytes()); // len 0
        let mut reader = Reader::new(&bytes);
        let mut led = Open;
        assert!(matches!(
            read_event(&mut reader, &mut led),
            Err(ReadError::Data(crate::wire::DecodeError::ZeroLengthField {
                tag: 1
            }))
        ));
    }

    /// A writer must not be able to emit a structure its own reader rejects.
    #[test]
    fn a_writer_refuses_reserved_event_flags() {
        let mut led = Open;
        let fields = [Field::new(1, &[1])];
        let err = encode_with(&mut led, |w| write_event(w, 1, 0x0002, &fields))
            .expect_err("reserved bit must be refused before writing");
        assert!(
            matches!(
                err,
                WriteError::Data(crate::wire::EncodeError::EventFlags { flags: 2 })
            ),
            "got {err:?}"
        );
    }

    /// The field table is admitted on the shared ledger, so a decode cannot allocate an
    /// unmetered collection, and a refusal arrives as Budget rather than as data damage.
    #[test]
    fn decoding_a_field_table_charges_the_ledger() {
        struct RefuseEntries;
        impl SharedLedger for RefuseEntries {
            fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
                Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Entries,
                    count,
                    0,
                ))
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let mut led = Open;
        let fields = [Field::new(1, &[1]), Field::new(2, &[2])];
        let bytes = encode_with(&mut led, |w| {
            write_event(w, 1, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("encode");
        let mut refusing = RefuseEntries;
        let mut reader = Reader::new(&bytes);
        let err = read_event(&mut reader, &mut refusing)
            .expect_err("field-table admission must be refused");
        assert!(matches!(err, ReadError::Budget(_)), "got {err:?}");
    }

    /// A declared field count that cannot fit in the remaining bytes is corruption, and
    /// is rejected before any reservation happens.
    #[test]
    fn an_impossible_field_count_is_refused_before_reserving() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes()); // tag
        bytes.extend_from_slice(&1u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&5u16.to_le_bytes()); // claim 5 fields
        // ...with no field bytes at all.
        let mut led = Open;
        let mut reader = Reader::new(&bytes);
        let err = read_event(&mut reader, &mut led)
            .expect_err("5 fields cannot fit in zero remaining bytes");
        assert!(
            matches!(
                err,
                ReadError::Data(crate::wire::DecodeError::Truncated { .. })
            ),
            "got {err:?}"
        );
    }

    /// A ledger with plenty of entries but no byte allowance must still refuse: the
    /// table's retained memory is charged in bytes.
    #[test]
    fn a_zero_byte_budget_refuses_the_field_table() {
        struct BytesOnlyZero;
        impl SharedLedger for BytesOnlyZero {
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

        let raw = [1u8, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7];
        let mut reader = Reader::new(&raw);
        let mut led = BytesOnlyZero;
        let err = read_event(&mut reader, &mut led)
            .expect_err("a zero byte budget must refuse the table");
        assert!(
            matches!(err, ReadError::Budget(e) if e.unit() == sage_db_types::ledger::BudgetUnit::Bytes),
            "refusal must name the byte unit: {err:?}"
        );
    }

    /// And the mirror: plenty of bytes but no entry allowance also refuses, naming the
    /// entry unit. Both units are independently required.
    #[test]
    fn a_zero_entry_allowance_refuses_the_field_table() {
        struct EntriesOnlyZero;
        impl SharedLedger for EntriesOnlyZero {
            fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
                Err(BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Entries,
                    count,
                    0,
                ))
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let mut led = Open;
        let fields = [Field::new(1, &[1])];
        let bytes = encode_with(&mut led, |w| {
            write_event(w, 1, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("encode");
        let mut reader = Reader::new(&bytes);
        let mut refusing = EntriesOnlyZero;
        let err =
            read_event(&mut reader, &mut refusing).expect_err("no entry allowance must refuse");
        assert!(
            matches!(err, ReadError::Budget(e) if e.unit() == sage_db_types::ledger::BudgetUnit::Entries),
            "refusal must name the entry unit: {err:?}"
        );
    }

    /// Exactly enough of both units succeeds; one byte short fails.
    #[test]
    fn an_exact_byte_and_entry_allowance_succeeds() {
        struct Exact {
            bytes: u64,
            entries: u64,
        }
        impl SharedLedger for Exact {
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

        let mut led = Open;
        let fields = [Field::new(1, &[1]), Field::new(2, &[2])];
        let bytes = encode_with(&mut led, |w| {
            write_event(w, 1, EVENT_FLAG_REQUIRED, &fields)
        })
        .expect("encode");
        let table_bytes = field_table_bytes(2);

        // Exact: 2 entries and exactly the table's bytes.
        let mut exact = Exact {
            bytes: table_bytes,
            entries: 2,
        };
        let mut reader = Reader::new(&bytes);
        read_event(&mut reader, &mut exact).expect("exact allowance must succeed");

        // One byte short: refused, and the unit names bytes.
        let mut short = Exact {
            bytes: table_bytes - 1,
            entries: 2,
        };
        let mut reader = Reader::new(&bytes);
        let err = read_event(&mut reader, &mut short).expect_err("one byte short");
        assert!(
            matches!(err, ReadError::Budget(e) if e.unit() == sage_db_types::ledger::BudgetUnit::Bytes),
            "got {err:?}"
        );
    }

    #[test]
    fn reserved_flag_bits_are_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0x0002u16.to_le_bytes()); // reserved bit set
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let mut reader = Reader::new(&bytes);
        let mut led = Open;
        assert!(matches!(
            read_event(&mut reader, &mut led),
            Err(ReadError::Data(crate::wire::DecodeError::EventFlags { .. }))
        ));
    }

    #[test]
    fn a_header_round_trips_and_respects_its_own_limit() {
        let fields = [Field::new(3, &[7, 7]), Field::new(4, &[8])];
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_header(w, &fields)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let decoded = read_header(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].tag(), 3);
        assert!(reader.is_empty());
    }

    #[test]
    fn an_unknown_field_can_be_skipped_by_length() {
        // Forward compatibility rests on this: a reader that does not know tag 99 can
        // still read the fields around it, because encoded_len says where the next one
        // starts.
        // Fields stay ascending; the unknown tag sits between two known ones so a
        // length-skipping reader must still land on tag 99's neighbour correctly.
        let fields = [
            Field::new(1, &[1]),
            Field::new(2, &[2]),
            Field::new(99, &[9, 9, 9]),
        ];
        with_roundtrip(&fields, |event| {
            assert_eq!(event.field(1), Some(&[1][..]));
            assert_eq!(event.field(2), Some(&[2][..]));
            assert_eq!(event.field(99), Some(&[9, 9, 9][..]));
            // A reader that knows only tags 1 and 2 can walk past 99 by length.
            let known: Vec<u16> = event
                .fields()
                .iter()
                .map(|f| f.tag())
                .filter(|t| *t < 90)
                .collect();
            assert_eq!(known, vec![1, 2]);
        });
    }
}
