//! Structural event codecs: the two events that describe something other than a record.
//!
//! `IdAllocation` advances one allocation space's high-water mark. `Genesis` establishes
//! the KB's two singleton records at revision 1. Neither carries a
//! [`sage_db_types::persist::RecordKey`], so neither belongs with the record families,
//! and neither is a record after-image.
//!
//! ## Split of responsibility with replay
//!
//! A single-event codec can decide only what one event's bytes mean:
//!
//! * `IdAllocation` decides the space domain, the mark's width and its per-space bound.
//!   "One event per advanced space, ascending" is a property of a whole transaction, and
//!   "the mark only moves forward" is a property of the folded log. Both are replay's,
//!   not this file's. Checking them here would be a check the caller would still owe.
//! * `Genesis` decides that both singletons are at their first revision, that the
//!   profile is a registered value, and that the two field groups share one
//!   `extensions` value (§4.4.6). Whether `version == 1` and whether the nine
//!   allocations follow are transaction-shape facts.
//!
//! ## Why genesis has its own record type
//!
//! D10 row 224 forbids tags `5 recorded_by` and `6 actor_origin`. Encoding that as "a
//! full record whose actor field we refuse" invites a caller to build the illegal value
//! first. [`GenesisRecord`] has no actor field at all, so the illegal value cannot be
//! constructed. This module adds no actor and no workaround.
//!
//! ## Field tags
//!
//! | tag | value | note |
//! |---|---|---|
//! | 164 `counter` | `u16` 0..=8 | the allocation space |
//! | 165 `high_water_mark` | `u128` | largest allocated id; bounded per space |
//! | 2 `revision` | `u64` = 1 | both singleton records' first revision |
//! | 8 `created_at` | `i64` | commit time (global `must`) |
//! | 144–148 | `kb_uuid`, `kb_slug`, `kb_name`, `kb_description`, `kb_domains` | metadata half |
//! | 163 `rules_digest` | `bytes32` | rule-config half **only** (§4.4.6) |
//! | 166 `rules_ref` | `bytes32` | rule-config half |
//! | 196 `rules_profile` | `u16` 0/1 | rule-config half |
//! | 14 `extensions` | `ExtMap` | one value, referenced by both halves |
//!
//! `163` is deliberately absent from the metadata half: duplicating it would put one
//! fact in two bodies, so a pure description update could look like a rule change. The
//! normative discriminator is recorded in §4.4.6 (`b6422a4a…` vs `aace0923…`).

use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{Extensions, GenesisRecord, IdAllocationEvent, IdSpace};
use sage_db_types::relation::RuleProfile;

use crate::event::read_event;
use crate::field::{
    MAX_DOMAINS, MAX_NAME_BYTES, MAX_SLUG_BYTES, MAX_TEXT_BYTES, read_extensions, read_str,
    read_str_list, write_extensions, write_str, write_str_list,
};
use crate::record::{FieldSet, check_policy, decode_field, decode_opt_field, require};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, EncodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: one allocation space's high-water mark.
pub const ID_ALLOCATION: u16 = 176;
/// Event tag: the KB's first event, establishing both singleton records.
pub const GENESIS: u16 = 224;

/// Per-element domain bound (tag 148 element width).
const MAX_DOMAIN_BYTES: u32 = 128;

mod t {
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const REASON: u16 = 7;
    pub(crate) const CREATED_AT: u16 = 8;
    pub(crate) const EXTENSIONS: u16 = 14;
    pub(crate) const KB_UUID: u16 = 144;
    pub(crate) const KB_SLUG: u16 = 145;
    pub(crate) const KB_NAME: u16 = 146;
    pub(crate) const KB_DESCRIPTION: u16 = 147;
    pub(crate) const KB_DOMAINS: u16 = 148;
    pub(crate) const COUNTER: u16 = 164;
    pub(crate) const HIGH_WATER_MARK: u16 = 165;
    pub(crate) const RULES_DIGEST: u16 = 163;
    pub(crate) const RULES_REF: u16 = 166;
    pub(crate) const RULES_PROFILE: u16 = 196;
}

/// Fields an `IdAllocation` event may carry.
///
/// §7.2's shared rule for non-record events grants `7 reason` and `14 extensions`, and
/// both now have a home on [`IdAllocationEvent`]; refusing them would reject a legal
/// file, dropping them would violate §8.3. `8 created_at` stays refused: §6.1 marks it
/// `must` in general, but row 176's own column lists only `164,165`, the accepted golden
/// `IdAllocation` events carry exactly those two, and the accepted carrier has no
/// timestamp slot. Inventing one here would be a timestamp no digest covers.
const ALLOCATION_FIELDS: &[u16] = &[t::REASON, t::EXTENSIONS, t::COUNTER, t::HIGH_WATER_MARK];

/// Fields every `IdAllocation` event must carry: its numeric payload and nothing else.
const ALLOCATION_REQUIRED: &[u16] = &[t::COUNTER, t::HIGH_WATER_MARK];

/// Fields a `Genesis` event may carry.
const GENESIS_FIELDS: &[u16] = &[
    t::REVISION,
    t::REASON,
    t::CREATED_AT,
    t::EXTENSIONS,
    t::KB_UUID,
    t::KB_SLUG,
    t::KB_NAME,
    t::KB_DESCRIPTION,
    t::KB_DOMAINS,
    t::RULES_DIGEST,
    t::RULES_REF,
    t::RULES_PROFILE,
];

/// Fields every `Genesis` event must carry.
///
/// `8 created_at` is required: §6.1 marks it `must` globally and the accepted golden
/// genesis event carries it, even though D10's row-224 专属必现 column omits it. The
/// stricter of the two accepted sources wins; the discrepancy is reported rather than
/// silently resolved.
const GENESIS_REQUIRED: &[u16] = &[
    t::REVISION,
    t::CREATED_AT,
    t::KB_UUID,
    t::KB_SLUG,
    t::KB_NAME,
    t::KB_DESCRIPTION,
    t::KB_DOMAINS,
    t::RULES_DIGEST,
    t::RULES_REF,
    t::RULES_PROFILE,
];

fn allocation_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(ALLOCATION_FIELDS, ALLOCATION_REQUIRED)
}

fn genesis_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(GENESIS_FIELDS, GENESIS_REQUIRED)
}

/// Write an `IdAllocation` event.
///
/// # Errors
///
/// [`WriteError::Data`] when the mark exceeds the bound its space declares - a term
/// watermark above `u32::MAX` would let the log claim an id that no term key can carry
/// back - or when a string exceeds its declared width; plus budget and allocation
/// refusals.
pub fn write_id_allocation(
    writer: &mut Writer<'_>,
    record: &IdAllocationEvent,
) -> Result<(), WriteError> {
    let mark = record.allocation;
    if !mark.is_in_range() {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: mark.space.as_u16(),
        }));
    }
    let mut set = FieldSet::with_capacity(ALLOCATION_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 7, 14, 164, 165.
    set.push_opt(ledger, t::REASON, record.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::COUNTER, mark.space, |w, v: IdSpace| {
        w.u16(v.as_u16())
    })?;
    set.push(ledger, t::HIGH_WATER_MARK, mark.hwm, |w, v| w.u128(v))?;
    set.write_event(writer, ID_ALLOCATION)
}

/// Decode an `IdAllocation` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, an unregistered space, a mark
/// outside the space's bound, or a malformed string; [`ReadError::Uninterpretable`] when
/// the envelope is not this event or the registry refuses it; budget, control and
/// allocation refusals otherwise.
pub fn read_id_allocation(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<IdAllocationEvent, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != ID_ALLOCATION {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &allocation_policy())?;

    let space_raw = decode_field(&event, t::COUNTER, |r| r.u16())?;
    let space = IdSpace::from_u16(space_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
        tag: t::COUNTER,
        value: space_raw,
    }))?;
    let hwm = decode_field(&event, t::HIGH_WATER_MARK, |r| r.u128())?;
    if hwm > space.max_id() {
        return Err(ReadError::Data(DecodeError::WatermarkOutOfRange {
            space: space.as_u16(),
            value: hwm,
        }));
    }
    // `8 created_at` is refused rather than ignored: this event's own table row does not
    // grant it, and accepting a recognized field with nowhere to keep it would be
    // accept-and-drop.
    if event.field(t::CREATED_AT).is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: ID_ALLOCATION,
            field: t::CREATED_AT,
        }));
    }
    let reason = decode_opt_field(&event, t::REASON, |r| read_str(r, MAX_TEXT_BYTES, ledger))?;
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    // Exit checkpoint so a cancelled attempt cannot return a value.
    ledger.checkpoint()?;
    Ok(IdAllocationEvent {
        allocation: sage_db_types::persist::Watermark { space, hwm },
        reason,
        extensions,
    })
}

/// Write a `Genesis` event: both singleton records at revision 1.
///
/// The revision is written as `1` by construction: genesis *defines* the first revision
/// of both singletons, so there is no caller value to disagree with it.
///
/// # Errors
///
/// [`WriteError::Data`] for an over-length string or domain, or when the two field
/// groups carry different `extensions` maps - one event has one value, so two would be a
/// second source of truth and is not encodable; plus budget and allocation refusals.
pub fn write_genesis(writer: &mut Writer<'_>, record: &GenesisRecord) -> Result<(), WriteError> {
    // Both halves reference the same tag 14 bytes (§4.4.6). Two different maps in one
    // event has no encoding, which is what keeps "shared" from becoming "duplicated".
    if record.metadata.extensions != record.rules.extensions {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: GENESIS,
            field: t::EXTENSIONS,
        }));
    }

    let mut set = FieldSet::with_capacity(GENESIS_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 2, 7, 8, 14, 144, 145, 146, 147, 148, 163, 166, 196.
    set.push(
        ledger,
        t::REVISION,
        RecordRevision::FIRST,
        |w, v: RecordRevision| w.u64(v.get()),
    )?;
    set.push_opt(ledger, t::REASON, record.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.created_at, |w, v| w.i64(v))?;
    if !record.metadata.extensions.entries().is_empty() {
        set.push(
            ledger,
            t::EXTENSIONS,
            &record.metadata.extensions,
            write_extensions,
        )?;
    }
    set.push(
        ledger,
        t::KB_UUID,
        record.metadata.kb_uuid,
        |w, v: KbUuid| w.bytes(v.as_bytes()),
    )?;
    set.push(ledger, t::KB_SLUG, record.metadata.slug.as_str(), |w, v| {
        write_str(w, v, MAX_SLUG_BYTES)
    })?;
    set.push(ledger, t::KB_NAME, record.metadata.name.as_str(), |w, v| {
        write_str(w, v, MAX_NAME_BYTES)
    })?;
    set.push(
        ledger,
        t::KB_DESCRIPTION,
        record.metadata.description.as_str(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(
        ledger,
        t::KB_DOMAINS,
        record.metadata.domains.as_slice(),
        |w, v| write_str_list(w, v, MAX_DOMAINS, MAX_DOMAIN_BYTES),
    )?;
    set.push(
        ledger,
        t::RULES_DIGEST,
        record.rules.rules_digest,
        |w, v| w.bytes(&v),
    )?;
    set.push(ledger, t::RULES_REF, record.rules.rules_ref, |w, v| {
        w.bytes(&v)
    })?;
    set.push(
        ledger,
        t::RULES_PROFILE,
        record.rules.profile,
        |w, v: RuleProfile| w.u16(v.as_u16()),
    )?;
    set.write_event(writer, GENESIS)
}

/// Decode a `Genesis` event into its two singleton field groups.
///
/// The one shared `14 extensions` value is materialised into both halves, which is what
/// "one encoding, two references" means: a decoder that gave them different values would
/// be unable to re-encode the event. `7 reason` lives on the event (both bodies share
/// it), never duplicated into a half.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, a revision other than the first,
/// a `kb_uuid` that is not 16 bytes, an unregistered profile, or a malformed
/// string/list; [`ReadError::Uninterpretable`] for a foreign event tag or a registry
/// refusal; budget and control refusals otherwise.
pub fn read_genesis(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<GenesisRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != GENESIS {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &genesis_policy())?;

    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if revision != RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: GENESIS,
            revision: revision.get(),
        }));
    }

    let kb_uuid = {
        let raw = require(&event, t::KB_UUID)?;
        // A 16-byte fixed field: a shorter or longer value is not a UUID, and silently
        // taking the first 16 bytes would accept a value no writer could produce.
        let bytes: [u8; 16] = raw.try_into().map_err(|_| {
            ReadError::Data(DecodeError::Truncated {
                offset: 0,
                wanted: 16,
            })
        })?;
        KbUuid::from_bytes(bytes)
    };

    let slug = decode_field(&event, t::KB_SLUG, |r| read_str(r, MAX_SLUG_BYTES, ledger))?;
    let name = decode_field(&event, t::KB_NAME, |r| read_str(r, MAX_NAME_BYTES, ledger))?;
    let description = decode_field(&event, t::KB_DESCRIPTION, |r| {
        read_str(r, MAX_TEXT_BYTES, ledger)
    })?;
    let domains = decode_field(&event, t::KB_DOMAINS, |r| {
        read_str_list(r, MAX_DOMAINS, MAX_DOMAIN_BYTES, ledger)
    })?;

    let profile_raw = decode_field(&event, t::RULES_PROFILE, |r| r.u16())?;
    let profile =
        RuleProfile::from_u16(profile_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::RULES_PROFILE,
            value: profile_raw,
        }))?;
    let rules_digest = decode_field(&event, t::RULES_DIGEST, |r| r.digest32())?;
    let rules_ref = decode_field(&event, t::RULES_REF, |r| r.digest32())?;
    let created_at = decode_field(&event, t::CREATED_AT, |r| r.i64())?;
    let reason = decode_opt_field(&event, t::REASON, |r| read_str(r, MAX_TEXT_BYTES, ledger))?;

    let extensions: Extensions =
        decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
            .unwrap_or_default();

    // Exit checkpoint: a fully-read event must still observe a cancelled attempt.
    ledger.checkpoint()?;

    Ok(GenesisRecord {
        created_at,
        reason,
        metadata: sage_db_types::persist::GenesisMetadata {
            kb_uuid,
            slug,
            name,
            description,
            domains,
            extensions: extensions.clone(),
        },
        rules: sage_db_types::persist::GenesisRules {
            profile,
            rules_digest,
            rules_ref,
            extensions,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::{BudgetError, ControlStop};

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

    fn encode(f: impl FnOnce(&mut Writer<'_>) -> Result<(), WriteError>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut led = Open;
        f(&mut Writer::new(&mut out, &mut led)).expect("encode");
        out
    }

    fn allocation(space: IdSpace, hwm: u128) -> IdAllocationEvent {
        IdAllocationEvent {
            allocation: sage_db_types::persist::Watermark { space, hwm },
            reason: None,
            extensions: Extensions::new(),
        }
    }

    #[test]
    fn every_space_round_trips() {
        for space in IdSpace::ALL {
            let record = allocation(space, 7);
            let bytes = encode(|w| write_id_allocation(w, &record));
            assert_eq!(
                read_id_allocation(&mut Reader::new(&bytes), &mut Open).expect("decode"),
                record
            );
        }
    }

    #[test]
    fn a_term_mark_above_u32_is_refused() {
        let record = allocation(IdSpace::TermId, u32::MAX as u128 + 1);
        assert!(
            !record.allocation.is_in_range(),
            "the space bound is part of the numeric payload"
        );
        let mut out = Vec::new();
        let mut led = Open;
        assert!(write_id_allocation(&mut Writer::new(&mut out, &mut led), &record).is_err());
    }
}
