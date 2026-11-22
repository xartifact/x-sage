//! Schema acceptance: which events and fields this major understands.
//!
//! The [`crate::event`] layer is a **transport**: it can frame any well-formed event and
//! field envelope. It deliberately cannot decide whether a tag means anything, and it
//! must not be read as schema acceptance. That decision lives here.
//!
//! In format major 0 the policy is strict:
//!
//! * an unregistered **field** tag is **rejected**, not skipped. `storage-format` §8.3
//!   states this explicitly; there is no forward-compatibility skipping in this major.
//!   Skipping would let a writer add an authoritative field that older readers silently
//!   drop, which is the failure the advisory registry exists to prevent;
//! * an unregistered **event** tag is rejected regardless of its REQUIRED bit;
//! * every registered event must carry the REQUIRED bit, because a reader that does not
//!   know the event must refuse rather than skip it.
//!
//! Keeping the three rules here means the envelope code has no policy and the policy has
//! no framing, so "it decoded" can never be mistaken for "it is valid".

use crate::event::Event;
use crate::wire::{DecodeError, ReadError};

/// What kind of record an event belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordClass {
    /// A record identified by a [`sage_db_types::persist::RecordKey`].
    Identified,
    /// A record identified by a natural key (content, name, or singleton).
    Natural,
    /// A non-record event: it advances bookkeeping rather than describing a record.
    Structural,
}

/// How a field may be treated by a reader that predates it.
///
/// Only the two advisory tags in the accepted registry may be `Advisory`; everything else
/// is `Required`, so an unknown authoritative field cannot be discarded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compat {
    /// Must be understood; an unknown tag in this position is a refusal.
    Required,
    /// Must be accepted and preserved, never interpreted.
    Advisory,
}

/// One event's registration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventSpec {
    /// The event tag.
    pub tag: u16,
    /// Stable name, for diagnostics.
    pub name: &'static str,
    /// Record class this event describes.
    pub class: RecordClass,
}

/// The registered events of format major 0.
///
/// Order is by tag; the list is closed. A missing tag is rejected, never "unknown but
/// tolerated".
pub const EVENTS: &[EventSpec] = &[
    EventSpec {
        tag: 1,
        name: "TermCreated",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 2,
        name: "TermRevised",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 3,
        name: "TermRetired",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 16,
        name: "AssertionCreated",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 17,
        name: "AssertionRevised",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 18,
        name: "AssertionRetracted",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 32,
        name: "ClaimCreated",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 33,
        name: "ClaimRevised",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 34,
        name: "ClaimSuperseded",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 35,
        name: "ClaimRejected",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 48,
        name: "EvidenceCreated",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 49,
        name: "EvidenceRevised",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 50,
        name: "EvidenceLinkCreated",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 51,
        name: "EvidenceLinkRetracted",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 64,
        name: "AssetDeclared",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 80,
        name: "VectorUpserted",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 81,
        name: "VectorRetired",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 96,
        name: "ProposalSubmitted",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 97,
        name: "ProposalDecided",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 112,
        name: "FrontierEntryChanged",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 128,
        name: "SessionCheckpointSaved",
        class: RecordClass::Identified,
    },
    EventSpec {
        tag: 144,
        name: "KbMetadataUpdated",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 145,
        name: "ModelRegistered",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 146,
        name: "ModelUpdated",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 147,
        name: "ModelRetired",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 160,
        name: "RuleProfileChanged",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 176,
        name: "IdAllocation",
        class: RecordClass::Structural,
    },
    EventSpec {
        tag: 192,
        name: "RequestOutcomeRecorded",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 208,
        name: "ImportBookkeeping",
        class: RecordClass::Natural,
    },
    EventSpec {
        tag: 224,
        name: "Genesis",
        class: RecordClass::Structural,
    },
];

/// Registered advisory field tags in format major 0.
///
/// Only these two: `changed_fields` is reconstructible from adjacent after-images and
/// `extensions` is an opaque container that still participates in the state digest.
pub const ADVISORY_FIELDS: [u16; 2] = [13, 14];

/// Exact field tags registered in format major 0.
///
/// A table, not a range: `storage-format` §6 assigns 107 specific tags with deliberate
/// holes (24, 36–47, 52–63, 71–79, 87–95, 102–111, 117–127, 131, 134–143, 155–159,
/// 167, 174–175, 188–191, 207+). A dense `1..=N` test accepts unassigned tags, which is
/// worse than useless: it turns "this major has no meaning for tag 24" into "tag 24 is
/// fine", and then a record codec silently ignores authoritative bytes.
///
/// The list is sorted; `is_registered_field` binary-searches it.
pub const REGISTERED_FIELDS: &[u16] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21, 22, 23, 32, 33, 34, 48,
    49, 50, 64, 65, 66, 67, 68, 69, 70, 80, 81, 82, 83, 84, 85, 86, 96, 97, 98, 99, 100, 101, 112,
    113, 114, 115, 116, 128, 129, 130, 132, 133, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153,
    154, 160, 161, 162, 163, 164, 165, 166, 168, 169, 170, 171, 172, 173, 176, 177, 178, 179, 180,
    181, 182, 183, 184, 185, 186, 187, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203,
    204, 205, 206,
];

/// Whether `tag` is a field this major actually assigns a meaning to.
#[must_use]
pub fn is_registered_field(tag: u16) -> bool {
    REGISTERED_FIELDS.binary_search(&tag).is_ok()
}

/// Compatibility class of a registered field tag.
///
/// # Panics
///
/// Cannot panic in practice: the advisory list is a compile-time constant and the
/// non-advisory branch is the fallback.
#[must_use]
pub const fn field_compat(tag: u16) -> Compat {
    let mut i = 0;
    while i < ADVISORY_FIELDS.len() {
        if ADVISORY_FIELDS[i] == tag {
            return Compat::Advisory;
        }
        i += 1;
    }
    Compat::Required
}

/// Why an event is not acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchemaError {
    /// The event tag is not registered in this major.
    UnknownEvent {
        /// The offending tag.
        tag: u16,
    },
    /// The event tag is registered but lacks the REQUIRED bit.
    MissingRequiredFlag {
        /// The offending tag.
        tag: u16,
    },
    /// A field tag is not registered in this major.
    UnknownField {
        /// The offending event tag.
        event: u16,
        /// The offending field tag.
        field: u16,
    },
}

impl From<SchemaError> for ReadError {
    fn from(error: SchemaError) -> Self {
        Self::Uninterpretable(error)
    }
}

/// Accept or reject one decoded event against the major-0 registry.
///
/// This is the step that turns a transport-valid envelope into a schema-accepted event.
/// Both rejections are deliberate: an unknown event could allocate ids or write blobs, so
/// skipping it would let a replay produce a different state from the one committed; an
/// unknown field could be authoritative, so skipping it would silently lose data.
///
/// # Errors
///
/// [`SchemaError`] wrapped in [`ReadError::Uninterpretable`].
pub fn accept_event(event: &Event<'_>) -> Result<(), SchemaError> {
    let spec = EVENTS
        .iter()
        .find(|spec| spec.tag == event.tag())
        .ok_or(SchemaError::UnknownEvent { tag: event.tag() })?;
    let _ = spec;
    if !event.is_required() {
        return Err(SchemaError::MissingRequiredFlag { tag: event.tag() });
    }
    for field in event.fields() {
        if !is_registered_field(field.tag()) {
            return Err(SchemaError::UnknownField {
                event: event.tag(),
                field: field.tag(),
            });
        }
    }
    Ok(())
}

/// Decode and accept one event, charging the shared ledger.
///
/// The convenience the record codecs use: framing plus policy in one call, with the
/// ledger threaded through the allocating path.
///
/// # Errors
///
/// [`ReadError`] for framing, budget, control or allocation; [`SchemaError`] wrapped in
/// [`ReadError::Uninterpretable`] for policy.
pub fn read_accepted_event<'a>(
    reader: &mut crate::wire::Reader<'a>,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<Event<'a>, ReadError> {
    let event = crate::event::read_event(reader, ledger)?;
    accept_event(&event)?;
    Ok(event)
}

/// The unregistered-tag error the envelope layer can produce on its own.
///
/// Provided so a caller that wants to test the raw transport still has a named outcome
/// distinct from a framing error.
#[must_use]
pub fn unregistered_field_error(tag: u16) -> DecodeError {
    DecodeError::UnregisteredField { tag }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EVENT_FLAG_REQUIRED, Field, encode_with, read_event, write_event};
    use crate::wire::Reader;
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

    fn encode_event(tag: u16, flags: u16, fields: &[Field<'_>]) -> Vec<u8> {
        let mut led = Open;
        encode_with(&mut led, |w| write_event(w, tag, flags, fields)).expect("encode")
    }

    #[test]
    fn every_registered_event_is_accepted() {
        for spec in EVENTS {
            let fields = [Field::new(1, &[0u8; 18])];
            let bytes = encode_event(spec.tag, EVENT_FLAG_REQUIRED, &fields);
            let mut led = Open;
            let mut reader = Reader::new(&bytes);
            let event = read_event(&mut reader, &mut led).expect("framing");
            assert_eq!(
                accept_event(&event),
                Ok(()),
                "registered event {} ({}) must be accepted",
                spec.tag,
                spec.name
            );
        }
    }

    #[test]
    fn an_unregistered_event_tag_is_rejected_even_when_required() {
        // 65535 is not in the registry: a REQUIRED bit does not make it acceptable, and
        // it must not be skipped either.
        let fields = [Field::new(1, &[0u8; 18])];
        let bytes = encode_event(65535, EVENT_FLAG_REQUIRED, &fields);
        let mut led = Open;
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing still works");
        assert_eq!(
            accept_event(&event),
            Err(SchemaError::UnknownEvent { tag: 65535 })
        );
    }

    #[test]
    fn a_truly_unregistered_field_tag_is_rejected_not_skipped() {
        // 65535 is above MAX_REGISTERED_FIELD, so this exercises real policy rather than
        // a tag that merely looks unfamiliar.
        assert!(!is_registered_field(65535));
        assert!(is_registered_field(99), "99 is registered (dimensions)");
        // Unassigned tags inside the numeric span must also be refused.
        for hole in [
            24u16, 36, 47, 52, 71, 87, 102, 117, 131, 134, 155, 167, 174, 188, 207,
        ] {
            assert!(!is_registered_field(hole), "unassigned tag {hole} accepted");
        }

        let fields = [Field::new(1, &[0u8; 18]), Field::new(65535, &[7])];
        let bytes = encode_event(1, EVENT_FLAG_REQUIRED, &fields);
        let mut led = Open;
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing still works");
        assert_eq!(
            accept_event(&event),
            Err(SchemaError::UnknownField {
                event: 1,
                field: 65535
            }),
            "an unregistered field must not be silently dropped"
        );
        // And the bytes are still there - rejection is not deletion.
        assert_eq!(event.field(65535), Some(&[7][..]));
    }

    #[test]
    fn a_registered_event_without_the_required_bit_is_rejected() {
        let fields = [Field::new(1, &[0u8; 18])];
        let bytes = encode_event(1, 0, &fields);
        let mut led = Open;
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing");
        assert_eq!(
            accept_event(&event),
            Err(SchemaError::MissingRequiredFlag { tag: 1 })
        );
    }

    #[test]
    fn only_the_two_advisory_tags_are_advisory() {
        assert_eq!(field_compat(13), Compat::Advisory);
        assert_eq!(field_compat(14), Compat::Advisory);
        for tag in [1u16, 5, 6, 23, 66, 70, 144, 179, 207] {
            assert_eq!(
                field_compat(tag),
                Compat::Required,
                "tag {tag} must not be skippable"
            );
        }
    }

    #[test]
    fn the_registry_is_sorted_and_unique() {
        let mut previous = 0u16;
        for spec in EVENTS {
            assert!(spec.tag > previous, "registry must be ascending and unique");
            previous = spec.tag;
        }
    }
}
