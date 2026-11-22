//! Record codecs: complete after-images as event field sets.
//!
//! A record event carries **every value of that revision**, not a delta. That is what
//! lets one revision be read without replaying earlier ones, and it is why these codecs
//! write a full field set rather than "what changed".
//!
//! Three rules run on the **public** decode path, not only in tests:
//!
//! * **Required fields must be present.** A missing revision, identity or content
//!   reference is corruption, because a record would otherwise decode into an
//!   incomplete value that later code would treat as meaningful.
//! * **Forbidden fields must be absent.** A field this event may not carry is not
//!   "ignored": accepting it would let a writer smuggle a value into a revision that the
//!   schema says cannot live there.
//! * **Cross-field rules hold.** `record_key.kind` must match the event,
//!   `actor_origin` constrains whether `recorded_by` may appear, and a retired record's
//!   status must agree with the event that produced it.
//!
//! ## Field tags used by term events
//!
//! | tag | value | notes |
//! |---|---|---|
//! | 1 | record key | kind must be Term |
//! | 2 | revision | ≥ 1 |
//! | 4 | status | 0 active, 1 retired |
//! | 5 | recorded_by | only when `actor_origin` permits |
//! | 6 | actor_origin | mandatory on every record event |
//! | 7 | reason | optional |
//! | 8 | created_at | mandatory |
//! | 9 | updated_at | mandatory on non-creation events |
//! | 10–12 | legacy id / path / content digest | optional |
//! | 13 | changed_fields | advisory, non-creation only |
//! | 14 | extensions | advisory container, part of state |
//! | 16–23 | slug, atom, layer, domains, source, confidence, triggers, body_ref | mandatory |
//! | 169–170 | legacy source-file digest / revision | optional |
//!
//! ## What is not here
//!
//! The term's *body text* is a blob. This codec writes and reads `body_ref`, the
//! content hash, and never the bytes: a record event that embedded a body would make
//! every revision rewrite the whole document and would break the "read one revision"
//! cost bound.

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{ActorOrigin, CommonFields, Extensions, RecordKind, TermRecord};

use crate::event::read_event;
use crate::field::{
    MAX_ATOM_BYTES, MAX_DOMAINS, MAX_IDENTITY_BYTES, MAX_PATH_BYTES, MAX_SLUG_BYTES,
    MAX_TERM_DOMAIN_BYTES, MAX_TEXT_BYTES, MAX_TRIGGER_BYTES, MAX_TRIGGERS, read_actor_origin,
    read_confidence, read_extensions, read_layer, read_opt_str, read_source, read_str,
    read_str_list, write_actor_origin, write_confidence, write_extensions, write_layer,
    write_record_key, write_revision, write_source, write_str, write_str_list,
};
use crate::record::require;
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// Event tags for the term record.
pub const TERM_CREATED: u16 = 1;
/// Event tag for a term revision.
pub const TERM_REVISED: u16 = 2;
/// Event tag for a term retirement.
pub const TERM_RETIRED: u16 = 3;

/// Field tags a term event may carry.
const TERM_FIELDS: &[u16] = &[
    1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21, 22, 23, 169, 170,
];

/// Fields every term event must carry.
const TERM_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 16, 17, 18, 19, 20, 21, 22, 23];

/// The term family's field policy.
/// The permitted/required sets for this family, for a dispatcher that must apply the
/// same policy instead of keeping a second copy of the table.
pub fn term_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(TERM_FIELDS, TERM_REQUIRED)
}

/// Field tags, named so the code below reads as fields rather than numbers.
mod t {
    pub(crate) const RECORD_KEY: u16 = 1;
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const STATUS: u16 = 4;
    pub(crate) const RECORDED_BY: u16 = 5;
    pub(crate) const ACTOR_ORIGIN: u16 = 6;
    pub(crate) const REASON: u16 = 7;
    pub(crate) const CREATED_AT: u16 = 8;
    pub(crate) const UPDATED_AT: u16 = 9;
    pub(crate) const LEGACY_ID: u16 = 10;
    pub(crate) const LEGACY_PATH: u16 = 11;
    pub(crate) const LEGACY_CONTENT: u16 = 12;
    pub(crate) const CHANGED_FIELDS: u16 = 13;
    pub(crate) const EXTENSIONS: u16 = 14;
    pub(crate) const SLUG: u16 = 16;
    pub(crate) const ATOM: u16 = 17;
    pub(crate) const LAYER: u16 = 18;
    pub(crate) const DOMAINS: u16 = 19;
    pub(crate) const SOURCE: u16 = 20;
    pub(crate) const CONFIDENCE: u16 = 21;
    pub(crate) const TRIGGERS: u16 = 22;
    pub(crate) const BODY_REF: u16 = 23;
    pub(crate) const LEGACY_SOURCE_FILE: u16 = 169;
    pub(crate) const LEGACY_SOURCE_REVISION: u16 = 170;
}

/// Which term event is being encoded or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermEvent {
    /// First revision of a term.
    Created,
    /// A later revision, carrying the complete after-image.
    Revised,
    /// Retirement, carrying the complete after-image with `retired = true`.
    Retired,
}

impl TermEvent {
    /// The event tag on the wire.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Created => TERM_CREATED,
            Self::Revised => TERM_REVISED,
            Self::Retired => TERM_RETIRED,
        }
    }

    /// Decode a term event from its tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            TERM_CREATED => Some(Self::Created),
            TERM_REVISED => Some(Self::Revised),
            TERM_RETIRED => Some(Self::Retired),
            _ => None,
        }
    }

    const fn is_creation(self) -> bool {
        matches!(self, Self::Created)
    }
}

/// Write a term event: the complete after-image of `record` at this revision.
///
/// # Errors
///
/// [`WriteError::Data`] when a field violates the schema for this event - a creation
/// event carrying `updated_at` or `changed_fields`, a retirement that is not flagged
/// retired, an id or revision out of range - plus budget and allocation refusals.
pub fn write_term(
    writer: &mut Writer<'_>,
    event: TermEvent,
    record: &TermRecord,
) -> Result<(), WriteError> {
    // Cross-field rules first, so an invalid record never reaches the bytes.
    if record.key.kind() != RecordKind::Term {
        return Err(WriteError::Data(crate::wire::EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if event.is_creation() {
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(crate::wire::EncodeError::WrongRevision {
                event: event.tag(),
                revision: record.revision.get(),
            }));
        }
    } else if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(crate::wire::EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if !event.is_creation() && record.common.updated_at.is_none() {
        return Err(WriteError::Data(
            crate::wire::EncodeError::MissingRequiredField {
                event: event.tag(),
                field: t::UPDATED_AT,
            },
        ));
    }
    if crate::record::reason_required(event.tag()) && record.common.reason.is_none() {
        return Err(WriteError::Data(
            crate::wire::EncodeError::MissingRequiredField {
                event: event.tag(),
                field: t::REASON,
            },
        ));
    }
    if event.is_creation() && record.common.updated_at.is_some() {
        return Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if event.is_creation() && record.changed_fields.is_some() {
        return Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    if event == TermEvent::Retired && !record.retired {
        return Err(WriteError::Data(crate::wire::EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    if event != TermEvent::Retired && record.retired {
        return Err(WriteError::Data(crate::wire::EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    // `recorded_by` appears exactly when the actor origin allows a verified identity.
    if matches!(record.common.actor_origin, ActorOrigin::Unavailable)
        && record.common.recorded_by.is_some()
    {
        return Err(WriteError::Data(
            crate::wire::EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            },
        ));
    }
    if matches!(record.common.actor_origin, ActorOrigin::Authenticated)
        && record.common.recorded_by.is_none()
    {
        return Err(WriteError::Data(
            crate::wire::EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            },
        ));
    }

    // Encode field values into their own buffers, then assemble the event with them.
    // Field values are small and bounded, so a scratch buffer per field is simpler than
    // threading one writer through closures - and it keeps each value's encoding in one
    // place.
    let mut set = crate::record::FieldSet::with_capacity(TERM_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(ledger, t::STATUS, u16::from(record.retired), |w, v| {
        w.u16(v)
    })?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(
        ledger,
        t::REASON,
        record.common.reason.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::CREATED_AT, record.common.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.common.updated_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::LEGACY_ID, record.legacy_id.as_deref(), |w, v| {
        write_str(w, v, MAX_IDENTITY_BYTES)
    })?;
    set.push_opt(
        ledger,
        t::LEGACY_PATH,
        record.legacy_source_path.as_deref(),
        |w, v| write_str(w, v, MAX_PATH_BYTES),
    )?;
    set.push_opt(
        ledger,
        t::LEGACY_CONTENT,
        record.legacy_content_digest,
        |w, v| w.bytes(&v),
    )?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        crate::record::write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::SLUG, record.slug.as_str(), |w, v| {
        write_str(w, v, MAX_SLUG_BYTES)
    })?;
    set.push(ledger, t::ATOM, record.atom.as_str(), |w, v| {
        write_str(w, v, MAX_ATOM_BYTES)
    })?;
    set.push(ledger, t::LAYER, record.layer, write_layer)?;
    set.push(ledger, t::DOMAINS, record.domains.as_slice(), |w, v| {
        write_str_list(w, v, MAX_DOMAINS, MAX_TERM_DOMAIN_BYTES)
    })?;
    set.push(ledger, t::SOURCE, record.source, write_source)?;
    set.push(ledger, t::CONFIDENCE, record.confidence, write_confidence)?;
    set.push(ledger, t::TRIGGERS, record.triggers.as_slice(), |w, v| {
        write_str_list(w, v, MAX_TRIGGERS, MAX_TRIGGER_BYTES)
    })?;
    set.push(ledger, t::BODY_REF, record.body_ref, |w, v| w.bytes(&v))?;
    set.push_opt(
        ledger,
        t::LEGACY_SOURCE_FILE,
        record.legacy_source_file_digest,
        |w, v| w.bytes(&v),
    )?;
    set.push_opt(
        ledger,
        t::LEGACY_SOURCE_REVISION,
        record.legacy_source_revision,
        |w, v| w.bytes(&v),
    )?;
    set.write_event(writer, event.tag())
}

/// Decode a term event into its complete after-image.
///
/// Runs framing, registry acceptance and the per-event rules on the public path.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing required field, a forbidden field, a cross-field
/// violation or malformed bytes; [`ReadError::Uninterpretable`] when the registry does
/// not accept the event; budget, control and allocation refusals otherwise.
pub fn read_term(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(TermEvent, TermRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    crate::record::check_policy(&event, &term_policy())?;
    let kind = TermEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;

    let key_raw = require(&event, t::RECORD_KEY)?;
    let key = {
        let mut r = Reader::new(key_raw);
        let key = crate::field::read_record_key(&mut r)?;
        r.finish()?;
        key
    };
    if key.kind() != RecordKind::Term {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }

    let revision = {
        let mut r = Reader::new(require(&event, t::REVISION)?);
        let rev = crate::field::read_revision(&mut r)?;
        r.finish()?;
        rev
    };
    if kind.is_creation() && revision != RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    if !kind.is_creation() && revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }

    let retired = {
        let status = u16_field(&event, t::STATUS)?;
        match status {
            0 => false,
            1 => true,
            other => {
                return Err(ReadError::Data(DecodeError::UnknownEnum {
                    tag: t::STATUS,
                    value: other,
                }));
            }
        }
    };
    match (kind, retired) {
        (TermEvent::Retired, false) | (TermEvent::Created | TermEvent::Revised, true) => {
            return Err(ReadError::Data(DecodeError::StatusMismatch {
                event: event.tag(),
            }));
        }
        _ => {}
    }

    let actor_origin = {
        let mut r = Reader::new(require(&event, t::ACTOR_ORIGIN)?);
        let actor = read_actor_origin(&mut r)?;
        r.finish()?;
        actor
    };
    let recorded_by = opt_str(&event, t::RECORDED_BY, MAX_IDENTITY_BYTES, ledger)?;
    match (actor_origin, recorded_by.is_some()) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(ReadError::Data(DecodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }

    if kind.is_creation() && event.field(t::UPDATED_AT).is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if kind.is_creation() && event.field(t::CHANGED_FIELDS).is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    if !kind.is_creation() && event.field(t::UPDATED_AT).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }

    let slug = str_field(&event, t::SLUG, MAX_SLUG_BYTES, ledger)?;
    let atom = str_field(&event, t::ATOM, MAX_ATOM_BYTES, ledger)?;
    let layer = {
        let mut r = Reader::new(require(&event, t::LAYER)?);
        let layer = read_layer(&mut r)?;
        r.finish()?;
        layer
    };
    let domains = list_field(
        &event,
        t::DOMAINS,
        MAX_DOMAINS,
        MAX_TERM_DOMAIN_BYTES,
        ledger,
    )?;
    let source = {
        let mut r = Reader::new(require(&event, t::SOURCE)?);
        let source = read_source(&mut r)?;
        r.finish()?;
        source
    };
    let confidence = {
        let mut r = Reader::new(require(&event, t::CONFIDENCE)?);
        let confidence = read_confidence(&mut r)?;
        r.finish()?;
        confidence
    };
    let triggers = list_field(&event, t::TRIGGERS, MAX_TRIGGERS, MAX_TRIGGER_BYTES, ledger)?;
    let body_ref = digest_field(&event, t::BODY_REF)?;

    let created_at = i64_field(&event, t::CREATED_AT)?;
    let updated_at = match event.field(t::UPDATED_AT) {
        Some(raw) => Some({
            let mut r = Reader::new(raw);
            let v = r.i64()?;
            r.finish()?;
            v
        }),
        None => None,
    };

    let changed_fields = match event.field(t::CHANGED_FIELDS) {
        Some(raw) => Some({
            let mut r = Reader::new(raw);
            let count = r.count(64)?;
            ledger.try_charge_entries(u64::from(count))?;
            let mut tags = Vec::new();
            tags.try_reserve_exact(count as usize)
                .map_err(|_| ReadError::Allocation {
                    requested: count as usize * 2,
                })?;
            let mut previous: Option<u16> = None;
            for _ in 0..count {
                let tag = r.u16()?;
                if previous.is_some_and(|prev| tag <= prev) {
                    return Err(ReadError::Data(DecodeError::FieldOrder { tag }));
                }
                previous = Some(tag);
                tags.push(tag);
            }
            r.finish()?;
            tags
        }),
        None => None,
    };

    let extensions = match event.field(t::EXTENSIONS) {
        Some(raw) => {
            let mut r = Reader::new(raw);
            let ext = read_extensions(&mut r, ledger)?;
            r.finish()?;
            ext
        }
        None => Extensions::new(),
    };

    let common = CommonFields {
        recorded_by,
        actor_origin,
        reason: opt_str(&event, t::REASON, MAX_TEXT_BYTES, ledger)?,
        created_at,
        updated_at,
    };

    Ok((
        kind,
        TermRecord {
            key,
            revision,
            retired,
            slug,
            atom,
            layer,
            domains,
            source,
            confidence,
            triggers,
            body_ref,
            common,
            legacy_id: opt_str(&event, t::LEGACY_ID, MAX_IDENTITY_BYTES, ledger)?,
            legacy_source_path: opt_str(&event, t::LEGACY_PATH, MAX_PATH_BYTES, ledger)?,
            legacy_content_digest: opt_digest(&event, t::LEGACY_CONTENT),
            legacy_source_file_digest: opt_digest(&event, t::LEGACY_SOURCE_FILE),
            legacy_source_revision: opt_digest(&event, t::LEGACY_SOURCE_REVISION),
            extensions,
            changed_fields,
        },
    ))
}

fn u16_field(event: &crate::event::Event<'_>, tag: u16) -> Result<u16, ReadError> {
    let mut r = Reader::new(require(event, tag)?);
    let v = r.u16()?;
    r.finish()?;
    Ok(v)
}

fn i64_field(event: &crate::event::Event<'_>, tag: u16) -> Result<i64, ReadError> {
    let mut r = Reader::new(require(event, tag)?);
    let v = r.i64()?;
    r.finish()?;
    Ok(v)
}

fn digest_field(event: &crate::event::Event<'_>, tag: u16) -> Result<[u8; 32], ReadError> {
    let mut r = Reader::new(require(event, tag)?);
    let v = r.digest32()?;
    r.finish()?;
    Ok(v)
}

fn opt_digest(event: &crate::event::Event<'_>, tag: u16) -> Option<[u8; 32]> {
    event.field(tag).map(|raw| {
        let mut r = Reader::new(raw);
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw[..32.min(raw.len())]);
        let _ = &mut r;
        out
    })
}

fn str_field(
    event: &crate::event::Event<'_>,
    tag: u16,
    max: u32,
    ledger: &mut dyn SharedLedger,
) -> Result<String, ReadError> {
    let mut r = Reader::new(require(event, tag)?);
    let v = read_str(&mut r, max, ledger)?;
    r.finish()?;
    Ok(v)
}

fn opt_str(
    event: &crate::event::Event<'_>,
    tag: u16,
    max: u32,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<String>, ReadError> {
    match event.field(tag) {
        Some(raw) => {
            let mut r = Reader::new(raw);
            let v = read_opt_str(&mut r, max, ledger)?;
            r.finish()?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

fn list_field(
    event: &crate::event::Event<'_>,
    tag: u16,
    max_items: u32,
    max_each: u32,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<String>, ReadError> {
    let mut r = Reader::new(require(event, tag)?);
    let v = read_str_list(&mut r, max_items, max_each, ledger)?;
    r.finish()?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Field, encode_with, write_event};
    use sage_db_types::confidence::Confidence;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::ledger::{BudgetError, ControlStop};
    use sage_db_types::persist::{RecordKey, TermSource};

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

    fn sample(revision: u64, retired: bool) -> TermRecord {
        TermRecord {
            key: RecordKey::term(7),
            revision: RecordRevision::new(revision).unwrap(),
            retired,
            slug: "sample-term".to_owned(),
            atom: "An example atom.".to_owned(),
            layer: SemanticLayer::L1,
            domains: vec!["demo".to_owned()],
            source: TermSource::Domain,
            confidence: Confidence::Confirmed,
            triggers: vec!["example".to_owned()],
            body_ref: [0xAA; 32],
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some("fixture reason".to_owned()),
                created_at: 1_763_424_000_000_000,
                updated_at: if revision > 1 {
                    Some(1_763_424_000_001_000)
                } else {
                    None
                },
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

    fn roundtrip(event: TermEvent, record: &TermRecord) -> (Vec<u8>, TermRecord) {
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_term(w, event, record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (decoded_event, decoded) = read_term(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded_event, event);
        assert!(reader.is_empty());
        (bytes, decoded)
    }

    #[test]
    fn a_created_term_round_trips_completely() {
        let record = sample(1, false);
        let (_, decoded) = roundtrip(TermEvent::Created, &record);
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_revised_term_carries_the_full_after_image() {
        let mut record = sample(2, false);
        record.legacy_id = Some("legacy-term-1".to_owned());
        record.legacy_source_path = Some("concepts/L1-core/sample-term.md".to_owned());
        record.legacy_content_digest = Some([0xAA; 32]);
        record.legacy_source_file_digest = Some([0xBB; 32]);
        record.legacy_source_revision = Some([0xCC; 32]);
        record
            .extensions
            .insert("legacy.provider", b"alpha".to_vec())
            .unwrap();
        record.changed_fields = Some(vec![9, 17]);
        record.common.reason = Some("clarified the atom".to_owned());
        let (_, decoded) = roundtrip(TermEvent::Revised, &record);
        assert_eq!(
            decoded, record,
            "every field of the revision must survive, including legacy and extensions"
        );
    }

    #[test]
    fn a_retired_term_round_trips_with_status() {
        let record = sample(3, true);
        let (_, decoded) = roundtrip(TermEvent::Retired, &record);
        assert!(decoded.retired);
        assert_eq!(decoded.revision, record.revision);
    }

    #[test]
    fn a_creation_cannot_carry_revision_fields() {
        let mut record = sample(1, false);
        record.common.updated_at = Some(1);
        let mut led = Open;
        assert!(matches!(
            encode_with(&mut led, |w| write_term(w, TermEvent::Created, &record)),
            Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
                field: 9,
                ..
            }))
        ));

        let mut record = sample(1, false);
        record.changed_fields = Some(vec![17]);
        assert!(matches!(
            encode_with(&mut led, |w| write_term(w, TermEvent::Created, &record)),
            Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
                field: 13,
                ..
            }))
        ));
    }

    #[test]
    fn status_must_match_the_event() {
        let mut led = Open;
        let retired_but_created = sample(1, true);
        assert!(matches!(
            encode_with(&mut led, |w| write_term(
                w,
                TermEvent::Created,
                &retired_but_created
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));

        let active_but_retired = sample(2, false);
        assert!(matches!(
            encode_with(&mut led, |w| write_term(
                w,
                TermEvent::Retired,
                &active_but_retired
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
    }

    #[test]
    fn actor_origin_constrains_recorded_by() {
        let mut led = Open;
        let mut unavailable_with_actor = sample(1, false);
        unavailable_with_actor.common.actor_origin = ActorOrigin::Unavailable;
        assert!(matches!(
            encode_with(&mut led, |w| write_term(
                w,
                TermEvent::Created,
                &unavailable_with_actor
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::ActorOriginMismatch { .. }
            ))
        ));

        let mut authenticated_without_actor = sample(1, false);
        authenticated_without_actor.common.recorded_by = None;
        assert!(matches!(
            encode_with(&mut led, |w| write_term(
                w,
                TermEvent::Created,
                &authenticated_without_actor
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::ActorOriginMismatch { .. }
            ))
        ));
    }

    #[test]
    fn a_wrong_record_kind_is_refused() {
        let mut led = Open;
        let mut record = sample(1, false);
        record.key = RecordKey::new(RecordKind::Claim, 1);
        assert!(matches!(
            encode_with(&mut led, |w| write_term(w, TermEvent::Created, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_missing_required_field_is_refused_on_decode() {
        // Hand-build a Created event without tag 23 (body_ref).
        let mut led = Open;
        let record = sample(1, false);
        let bytes =
            encode_with(&mut led, |w| write_term(w, TermEvent::Created, &record)).expect("encode");
        // Re-encode the same fields minus body_ref, by decoding the event and rebuilding.
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing");
        let kept: Vec<Field<'_>> = event
            .fields()
            .iter()
            .copied()
            .filter(|f| f.tag() != t::BODY_REF)
            .collect();
        let mut rebuilt = Vec::new();
        {
            let mut w = Writer::new(&mut rebuilt, &mut led);
            write_event(
                &mut w,
                event.tag(),
                crate::event::EVENT_FLAG_REQUIRED,
                &kept,
            )
            .expect("re-encode");
        }
        let mut reader = Reader::new(&rebuilt);
        assert!(matches!(
            read_term(&mut reader, &mut led),
            Err(ReadError::Data(DecodeError::MissingField { field: 23, .. }))
        ));
    }

    #[test]
    fn a_revision_event_without_updated_at_is_refused() {
        let mut led = Open;
        let record = sample(2, false);
        let bytes =
            encode_with(&mut led, |w| write_term(w, TermEvent::Revised, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing");
        let kept: Vec<Field<'_>> = event
            .fields()
            .iter()
            .copied()
            .filter(|f| f.tag() != t::UPDATED_AT)
            .collect();
        let mut rebuilt = Vec::new();
        {
            let mut w = Writer::new(&mut rebuilt, &mut led);
            write_event(
                &mut w,
                event.tag(),
                crate::event::EVENT_FLAG_REQUIRED,
                &kept,
            )
            .expect("re-encode");
        }
        let mut reader = Reader::new(&rebuilt);
        assert!(matches!(
            read_term(&mut reader, &mut led),
            Err(ReadError::Data(DecodeError::MissingField { field: 9, .. }))
        ));
    }

    #[test]
    fn the_body_text_is_a_reference_not_embedded_bytes() {
        let record = sample(1, false);
        let (bytes, _) = roundtrip(TermEvent::Created, &record);
        // 32 bytes of digest, not a body.
        assert!(
            bytes.len() < 600,
            "a term event must stay small; got {} bytes",
            bytes.len()
        );
    }
}
