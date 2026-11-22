//! Evidence and evidence-link codecs.
//!
//! Two properties drive the whole design here:
//!
//! * **Evidence is a record, not a path.** The excerpt and asset bytes live in blobs, and
//!   the record holds their hashes. A locator (URL, repository path, page number) is a
//!   free-form string for a human; it is never resolved as a KB reference and never used
//!   to fetch content.
//! * **A link pins a revision.** `(evidence, evidence_revision, target, target_revision)`
//!   is the identity of the support. When the target gains a revision, the link does not
//!   change - only its *classification* as current-or-historical does, and that is derived
//!   at read time. Storing the classification instead would require rewriting every
//!   inbound link whenever a target was edited.
//!
//! ## Optional fields and losslessness
//!
//! `locator`, `excerpt_ref`, `asset_sha256` and `declared_captured_by` are all optional at
//! this major. That is a statement about the record, not a licence to drop data: an
//! imported evidence may genuinely have no locator, and a reader that cannot represent
//! "absent" would have to invent one. The HTTP layer's stricter admission rules (text
//! evidence needs an excerpt) belong to the service boundary, not to storage.
//!
//! Field tags: 1 key, 2 revision, 4 status, 5 recorded_by, 6 actor_origin, 7 reason,
//! 8 created_at, 9 updated_at, 13 changed_fields, 14 extensions, 21 confidence-free
//! evidence fields below, 64–70 evidence payload, 80–83 link payload.

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, EvidenceKind, EvidenceLinkRecord, EvidenceRecord, Extensions,
    LinkTargetKind, RecordKind,
};

use crate::event::read_event;
use crate::field::{
    MAX_DECLARED_CAPTURED_BY, MAX_IDENTITY_BYTES, MAX_LOCATOR_BYTES, MAX_TEXT_BYTES,
    MAX_TITLE_BYTES, read_actor_origin, read_extensions, read_opt_str, read_str,
    write_actor_origin, write_extensions, write_record_key, write_revision, write_str,
};
use crate::record::{
    FieldSet, check_policy, decode_changed_fields, decode_field, decode_opt_field, forbid,
    reason_required, write_changed_fields,
};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: first revision of an evidence record.
pub const EVIDENCE_CREATED: u16 = 48;
/// Event tag: a later evidence revision.
pub const EVIDENCE_REVISED: u16 = 49;
/// Event tag: first revision of an evidence link.
pub const LINK_CREATED: u16 = 50;
/// Event tag: link retraction.
pub const LINK_RETRACTED: u16 = 51;

mod t {
    pub(crate) const RECORD_KEY: u16 = 1;
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const STATUS: u16 = 4;
    pub(crate) const RECORDED_BY: u16 = 5;
    pub(crate) const ACTOR_ORIGIN: u16 = 6;
    pub(crate) const REASON: u16 = 7;
    pub(crate) const CREATED_AT: u16 = 8;
    pub(crate) const UPDATED_AT: u16 = 9;
    pub(crate) const CHANGED_FIELDS: u16 = 13;
    pub(crate) const EXTENSIONS: u16 = 14;
    pub(crate) const EVIDENCE_KIND: u16 = 64;
    pub(crate) const TITLE: u16 = 65;
    pub(crate) const LOCATOR: u16 = 66;
    pub(crate) const EXCERPT_REF: u16 = 67;
    pub(crate) const ASSET_SHA256: u16 = 68;
    pub(crate) const CAPTURED_AT: u16 = 69;
    pub(crate) const DECLARED_CAPTURED_BY: u16 = 70;
    pub(crate) const LINK_EVIDENCE: u16 = 80;
    pub(crate) const LINK_EVIDENCE_REVISION: u16 = 81;
    pub(crate) const LINK_TARGET: u16 = 82;
    pub(crate) const LINK_TARGET_REVISION: u16 = 83;
    pub(crate) const LINK_LEGACY_ID: u16 = 10;
}

const EVIDENCE_FIELDS: &[u16] = &[1, 2, 4, 5, 6, 7, 8, 9, 13, 14, 64, 65, 66, 67, 68, 69, 70];
const EVIDENCE_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 64, 65, 69];
// Tag 10 (legacy id) is part of the complete link after-image: the canonical record
// carries it, so dropping it on write would lose a record value.
const LINK_FIELDS: &[u16] = &[1, 2, 4, 5, 6, 7, 8, 9, 10, 13, 14, 80, 81, 82, 83];
const LINK_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 80, 81, 82, 83];

/// The permitted/required sets for `EvidenceCreated`/`EvidenceRevised`.
///
/// Public so a dispatcher applies this table rather than restating it.
pub fn evidence_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(EVIDENCE_FIELDS, EVIDENCE_REQUIRED)
}

/// The permitted/required sets for the evidence-link events.
///
/// Public so a dispatcher applies this table rather than restating it.
pub fn link_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(LINK_FIELDS, LINK_REQUIRED)
}

/// Which evidence event is being encoded or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvidenceEvent {
    /// First revision.
    Created,
    /// A later revision carrying the complete after-image.
    Revised,
}

impl EvidenceEvent {
    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Created => EVIDENCE_CREATED,
            Self::Revised => EVIDENCE_REVISED,
        }
    }

    /// Decode from a wire tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            EVIDENCE_CREATED => Some(Self::Created),
            EVIDENCE_REVISED => Some(Self::Revised),
            _ => None,
        }
    }

    const fn is_creation(self) -> bool {
        matches!(self, Self::Created)
    }
}

/// Which link event is being encoded or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkEvent {
    /// First revision.
    Created,
    /// Retraction with the complete after-image.
    Retracted,
}

impl LinkEvent {
    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Created => LINK_CREATED,
            Self::Retracted => LINK_RETRACTED,
        }
    }

    /// Decode from a wire tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            LINK_CREATED => Some(Self::Created),
            LINK_RETRACTED => Some(Self::Retracted),
            _ => None,
        }
    }
}

/// Write an evidence event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a missing asset on an image record,
/// revision rules, a missing reason where required, or an actor/origin mismatch; plus
/// budget, control and allocation refusals.
pub fn write_evidence(
    writer: &mut Writer<'_>,
    event: EvidenceEvent,
    record: &EvidenceRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if record.key.kind() != RecordKind::Evidence {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    // An image evidence without an asset would be a record pointing at nothing. This is a
    // schema rule, so it applies on every write path rather than only in the service.
    if matches!(record.kind, EvidenceKind::Image) && record.asset_sha256.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::ASSET_SHA256,
        }));
    }
    if event.is_creation() {
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event.tag(),
                revision: record.revision.get(),
            }));
        }
    } else if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if event.is_creation() && record.common.updated_at.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if event.is_creation() && record.changed_fields.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    if !event.is_creation() && record.common.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    match (
        record.common.actor_origin,
        record.common.recorded_by.is_some(),
    ) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(WriteError::Data(EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(EVIDENCE_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    // Evidence has no retired state in this major: tag 4 is always active.
    set.push(ledger, t::STATUS, 0u16, |w, v| w.u16(v))?;
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
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::EVIDENCE_KIND, record.kind.as_u16(), |w, v| {
        w.u16(v)
    })?;
    set.push(ledger, t::TITLE, record.title.as_str(), |w, v| {
        write_str(w, v, MAX_TITLE_BYTES)
    })?;
    set.push_opt(ledger, t::LOCATOR, record.locator.as_deref(), |w, v| {
        write_str(w, v, MAX_LOCATOR_BYTES)
    })?;
    set.push_opt(ledger, t::EXCERPT_REF, record.excerpt_ref, |w, v| {
        w.bytes(&v)
    })?;
    set.push_opt(ledger, t::ASSET_SHA256, record.asset_sha256, |w, v| {
        w.bytes(&v)
    })?;
    set.push(ledger, t::CAPTURED_AT, record.captured_at, |w, v| w.i64(v))?;
    set.push_opt(
        ledger,
        t::DECLARED_CAPTURED_BY,
        record.declared_captured_by.as_deref(),
        // Tag 70's own bound is 800 bytes (storage-format §6.2), not the generic
        // identity length: a declared capturer is a document attribute, not a principal.
        |w, v| write_str(w, v, MAX_DECLARED_CAPTURED_BY),
    )?;
    set.write_event(writer, event.tag())
}

/// Decode an evidence event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing, forbidden or inconsistent field; budget, control and
/// allocation refusals otherwise.
pub fn read_evidence(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(EvidenceEvent, EvidenceRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    check_policy(&event, &evidence_policy())?;
    let kind = EvidenceEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::Evidence {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
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
    let status = decode_field(&event, t::STATUS, |r| r.u16())?;
    if status != 0 {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status,
        }));
    }
    let evidence_kind_raw = decode_field(&event, t::EVIDENCE_KIND, |r| r.u16())?;
    let evidence_kind = EvidenceKind::from_u16(evidence_kind_raw).ok_or(ReadError::Data(
        DecodeError::UnknownEnum {
            tag: t::EVIDENCE_KIND,
            value: evidence_kind_raw,
        },
    ))?;

    let actor_origin = decode_field(&event, t::ACTOR_ORIGIN, read_actor_origin)?;
    let recorded_by = decode_opt_field(&event, t::RECORDED_BY, |r| {
        read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
    })?;
    match (actor_origin, recorded_by.is_some()) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(ReadError::Data(DecodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }
    if kind.is_creation() {
        forbid(&event, t::UPDATED_AT)?;
        forbid(&event, t::CHANGED_FIELDS)?;
    } else if event.field(t::UPDATED_AT).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }

    let asset_sha256 = decode_opt_field(&event, t::ASSET_SHA256, |r| r.digest32())?;
    if matches!(evidence_kind, EvidenceKind::Image) && asset_sha256.is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::ASSET_SHA256,
        }));
    }

    Ok((
        kind,
        EvidenceRecord {
            key,
            revision,
            kind: evidence_kind,
            title: decode_field(&event, t::TITLE, |r| read_str(r, MAX_TITLE_BYTES, ledger))?,
            locator: decode_opt_field(&event, t::LOCATOR, |r| {
                read_opt_str(r, MAX_LOCATOR_BYTES, ledger)
            })?,
            excerpt_ref: decode_opt_field(&event, t::EXCERPT_REF, |r| r.digest32())?,
            asset_sha256,
            captured_at: decode_field(&event, t::CAPTURED_AT, |r| r.i64())?,
            declared_captured_by: decode_opt_field(&event, t::DECLARED_CAPTURED_BY, |r| {
                read_opt_str(r, MAX_DECLARED_CAPTURED_BY, ledger)
            })?,
            common: CommonFields {
                recorded_by,
                actor_origin,
                reason: decode_opt_field(&event, t::REASON, |r| {
                    read_opt_str(r, MAX_TEXT_BYTES, ledger)
                })?,
                created_at: decode_field(&event, t::CREATED_AT, |r| r.i64())?,
                updated_at: decode_opt_field(&event, t::UPDATED_AT, |r| r.i64())?,
            },
            extensions: match event.field(t::EXTENSIONS) {
                Some(raw) => {
                    let mut r = Reader::new(raw);
                    let ext = read_extensions(&mut r, ledger)?;
                    r.finish()?;
                    ext
                }
                None => Extensions::new(),
            },
            changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
        },
    ))
}

/// Write an evidence-link event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a non-evidence source, a target kind that
/// does not match the target key, status/tag contradiction, a missing retraction reason,
/// or an actor/origin mismatch; plus budget, control and allocation refusals.
pub fn write_link(
    writer: &mut Writer<'_>,
    event: LinkEvent,
    record: &EvidenceLinkRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if record.key.kind() != RecordKind::EvidenceLink {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if record.evidence.kind() != RecordKind::Evidence {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.evidence.kind().as_u16(),
        }));
    }
    // The target kind is implied by the target key's kind; a mismatch would let one be
    // written while the other is read.
    if LinkTargetKind::from_u16(record.target.kind().as_u16()).is_none() {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.target.kind().as_u16(),
        }));
    }
    if event == LinkEvent::Created && record.revision != RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if event == LinkEvent::Retracted && record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if record.retracted != (event == LinkEvent::Retracted) {
        return Err(WriteError::Data(EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    if event == LinkEvent::Created && record.changed_fields.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    if event == LinkEvent::Created && record.common.updated_at.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if event == LinkEvent::Retracted && record.common.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if reason_required(event.tag()) && record.common.reason.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::REASON,
        }));
    }
    match (
        record.common.actor_origin,
        record.common.recorded_by.is_some(),
    ) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(WriteError::Data(EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(LINK_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(ledger, t::STATUS, u16::from(record.retracted), |w, v| {
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
    set.push_opt(
        ledger,
        t::LINK_LEGACY_ID,
        record.legacy_id.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::LINK_EVIDENCE, record.evidence, write_record_key)?;
    set.push(
        ledger,
        t::LINK_EVIDENCE_REVISION,
        record.evidence_revision,
        write_revision,
    )?;
    set.push(ledger, t::LINK_TARGET, record.target, write_record_key)?;
    set.push(
        ledger,
        t::LINK_TARGET_REVISION,
        record.target_revision,
        write_revision,
    )?;
    set.write_event(writer, event.tag())
}

/// Decode an evidence-link event.
///
/// # Errors
///
/// As [`read_evidence`], plus a target kind that is not term/assertion/claim.
pub fn read_link(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(LinkEvent, EvidenceLinkRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    check_policy(&event, &link_policy())?;
    let kind = LinkEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::EvidenceLink {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let evidence = decode_field(&event, t::LINK_EVIDENCE, crate::field::read_record_key)?;
    if evidence.kind() != RecordKind::Evidence {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: evidence.kind().as_u16(),
        }));
    }
    let target = decode_field(&event, t::LINK_TARGET, crate::field::read_record_key)?;
    if LinkTargetKind::from_u16(target.kind().as_u16()).is_none() {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: target.kind().as_u16(),
        }));
    }

    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if kind == LinkEvent::Created && revision != RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    if kind == LinkEvent::Retracted && revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    let status = decode_field(&event, t::STATUS, |r| r.u16())?;
    let retracted = match status {
        0 => false,
        1 => true,
        other => {
            return Err(ReadError::Data(DecodeError::UnknownEnum {
                tag: t::STATUS,
                value: other,
            }));
        }
    };
    if retracted != (kind == LinkEvent::Retracted) {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }

    let actor_origin = decode_field(&event, t::ACTOR_ORIGIN, read_actor_origin)?;
    let recorded_by = decode_opt_field(&event, t::RECORDED_BY, |r| {
        read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
    })?;
    match (actor_origin, recorded_by.is_some()) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(ReadError::Data(DecodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }
    if kind == LinkEvent::Created {
        forbid(&event, t::UPDATED_AT)?;
        forbid(&event, t::CHANGED_FIELDS)?;
    } else if event.field(t::UPDATED_AT).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if reason_required(event.tag()) && event.field(t::REASON).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::REASON,
        }));
    }

    Ok((
        kind,
        EvidenceLinkRecord {
            key,
            revision,
            retracted,
            evidence,
            evidence_revision: decode_field(
                &event,
                t::LINK_EVIDENCE_REVISION,
                crate::field::read_revision,
            )?,
            target,
            target_revision: decode_field(
                &event,
                t::LINK_TARGET_REVISION,
                crate::field::read_revision,
            )?,
            common: CommonFields {
                recorded_by,
                actor_origin,
                reason: decode_opt_field(&event, t::REASON, |r| {
                    read_opt_str(r, MAX_TEXT_BYTES, ledger)
                })?,
                created_at: decode_field(&event, t::CREATED_AT, |r| r.i64())?,
                updated_at: decode_opt_field(&event, t::UPDATED_AT, |r| r.i64())?,
            },
            legacy_id: decode_opt_field(&event, t::LINK_LEGACY_ID, |r| {
                read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
            })?,
            extensions: match event.field(t::EXTENSIONS) {
                Some(raw) => {
                    let mut r = Reader::new(raw);
                    let ext = read_extensions(&mut r, ledger)?;
                    r.finish()?;
                    ext
                }
                None => Extensions::new(),
            },
            changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::encode_with;
    use sage_db_types::ledger::{BudgetError, ControlStop};
    use sage_db_types::persist::RecordKey;

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

    fn evidence(revision: u64) -> EvidenceRecord {
        EvidenceRecord {
            key: RecordKey::new(RecordKind::Evidence, 2),
            revision: RecordRevision::new(revision).unwrap(),
            kind: EvidenceKind::Text,
            title: "A source".to_owned(),
            locator: Some("https://example.invalid/doc".to_owned()),
            excerpt_ref: Some([0x55; 32]),
            asset_sha256: None,
            captured_at: 1_763_424_000_000_000,
            declared_captured_by: Some("the document's author".to_owned()),
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1_763_424_000_000_000,
                updated_at: if revision > 1 {
                    Some(1_763_424_000_001_000)
                } else {
                    None
                },
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn link(revision: u64) -> EvidenceLinkRecord {
        EvidenceLinkRecord {
            key: RecordKey::new(RecordKind::EvidenceLink, 4),
            revision: RecordRevision::new(revision).unwrap(),
            retracted: revision > 1,
            evidence: RecordKey::new(RecordKind::Evidence, 2),
            evidence_revision: RecordRevision::new(1).unwrap(),
            target: RecordKey::term(9),
            target_revision: RecordRevision::new(3).unwrap(),
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: if revision > 1 {
                    Some("target replaced".to_owned())
                } else {
                    None
                },
                created_at: 1_763_424_000_000_000,
                updated_at: if revision > 1 {
                    Some(1_763_424_000_001_000)
                } else {
                    None
                },
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    #[test]
    fn text_evidence_round_trips_with_optional_fields_present() {
        let record = evidence(1);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| {
            write_evidence(w, EvidenceEvent::Created, &record)
        })
        .expect("encode");
        let mut reader = Reader::new(&bytes);
        let (_, decoded) = read_evidence(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.declared_captured_by.as_deref(),
            Some("the document's author"),
            "the declared capturer is preserved as declared, not as the executor"
        );
    }

    /// An imported evidence may genuinely have no locator or declared capturer; absence
    /// must survive as absence rather than becoming an invented value.
    #[test]
    fn absent_optional_evidence_fields_stay_absent() {
        let mut record = evidence(1);
        record.locator = None;
        record.declared_captured_by = None;
        record.excerpt_ref = None;
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| {
            write_evidence(w, EvidenceEvent::Created, &record)
        })
        .expect("encode");
        let mut reader = Reader::new(&bytes);
        let (_, decoded) = read_evidence(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded.locator, None);
        assert_eq!(decoded.declared_captured_by, None);
        assert_eq!(decoded.excerpt_ref, None);
        assert_eq!(decoded, record);
    }

    #[test]
    fn image_evidence_requires_an_asset_on_both_sides() {
        let mut led = Open;
        let mut record = evidence(1);
        record.kind = EvidenceKind::Image;
        assert!(matches!(
            encode_with(&mut led, |w| write_evidence(
                w,
                EvidenceEvent::Created,
                &record
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::MissingRequiredField { field: 68, .. }
            ))
        ));

        record.asset_sha256 = Some([0x66; 32]);
        let bytes = encode_with(&mut led, |w| {
            write_evidence(w, EvidenceEvent::Created, &record)
        })
        .expect("encode");
        let mut reader = Reader::new(&bytes);
        let (_, decoded) = read_evidence(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded.asset_sha256, Some([0x66; 32]));
    }

    #[test]
    fn an_evidence_link_round_trips_and_pins_both_revisions() {
        let record = link(1);
        let mut led = Open;
        let bytes =
            encode_with(&mut led, |w| write_link(w, LinkEvent::Created, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (_, decoded) = read_link(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.target_revision,
            RecordRevision::new(3).unwrap(),
            "the link cites a specific target revision, not 'current'"
        );
    }

    /// Retracting a link changes the link only; the cited revisions do not move.
    #[test]
    fn retraction_keeps_the_cited_revisions() {
        let mut record = link(2);
        record.retracted = true;
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_link(w, LinkEvent::Retracted, &record))
            .expect("encode");
        let mut reader = Reader::new(&bytes);
        let (_, decoded) = read_link(&mut reader, &mut led).expect("decode");
        assert!(decoded.retracted);
        assert_eq!(decoded.evidence_revision, record.evidence_revision);
        assert_eq!(decoded.target_revision, record.target_revision);
    }

    #[test]
    fn a_link_to_a_non_supportable_kind_is_refused() {
        let mut led = Open;
        let mut record = link(1);
        record.target = RecordKey::new(RecordKind::Evidence, 1);
        assert!(matches!(
            encode_with(&mut led, |w| write_link(w, LinkEvent::Created, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_non_evidence_source_is_refused() {
        let mut led = Open;
        let mut record = link(1);
        record.evidence = RecordKey::term(1);
        assert!(matches!(
            encode_with(&mut led, |w| write_link(w, LinkEvent::Created, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_retraction_requires_a_reason() {
        let mut led = Open;
        let mut record = link(2);
        record.retracted = true;
        record.common.reason = None;
        assert!(matches!(
            encode_with(&mut led, |w| write_link(w, LinkEvent::Retracted, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::MissingRequiredField { field: 7, .. }
            ))
        ));
    }

    /// Tag 70's bound is 800 bytes, and 801 must be refused on BOTH sides - not merely
    /// round-tripped at one length.
    #[test]
    fn declared_capturer_bound_is_800_on_write_and_read() {
        let mut led = Open;

        // Exactly 800 succeeds.
        let mut record = evidence(1);
        record.declared_captured_by = Some("x".repeat(800));
        let bytes = encode_with(&mut led, |w| {
            write_evidence(w, EvidenceEvent::Created, &record)
        })
        .expect("800 bytes is the declared limit");

        // 801 is refused by the writer.
        record.declared_captured_by = Some("x".repeat(801));
        assert!(matches!(
            encode_with(&mut led, |w| write_evidence(
                w,
                EvidenceEvent::Created,
                &record
            )),
            Err(WriteError::Data(crate::wire::EncodeError::StringTooLong))
        ));

        // And 801 is refused by the reader, from hand-built bytes, naming the real bound.
        let mut reader = Reader::new(&bytes);
        let mut event = crate::event::read_event(&mut reader, &mut led).expect("framing");
        let _ = &mut event;
        let mut fields: Vec<(u16, Vec<u8>)> = event
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .filter(|(tag, _)| *tag != t::DECLARED_CAPTURED_BY)
            .collect();
        let mut too_long = 801u32.to_le_bytes().to_vec();
        too_long.extend_from_slice(&vec![b'x'; 801]);
        fields.push((t::DECLARED_CAPTURED_BY, too_long));
        fields.sort_by_key(|(tag, _)| *tag);

        let mut raw = Vec::new();
        raw.extend_from_slice(&48u16.to_le_bytes());
        raw.extend_from_slice(&1u16.to_le_bytes());
        raw.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (tag, value) in fields {
            raw.extend_from_slice(&tag.to_le_bytes());
            raw.extend_from_slice(&0u16.to_le_bytes());
            raw.extend_from_slice(&(value.len() as u32).to_le_bytes());
            raw.extend_from_slice(&value);
        }
        let mut reader = Reader::new(&raw);
        match read_evidence(&mut reader, &mut led) {
            Err(ReadError::Data(DecodeError::StringTooLong { len, max })) => {
                assert_eq!(len, 801);
                assert_eq!(max, 800, "the refusal must name tag 70's own bound");
            }
            other => panic!("801 bytes must be refused with the real bound, got {other:?}"),
        }
    }

    /// A link's legacy id is part of its complete after-image and must survive a
    /// retraction, not only a creation.
    #[test]
    fn link_legacy_id_survives_creation_and_retraction() {
        let mut led = Open;
        for (event, revision) in [(LinkEvent::Created, 1u64), (LinkEvent::Retracted, 2u64)] {
            let mut record = link(revision);
            record.legacy_id = Some("original-link".to_owned());
            let bytes = encode_with(&mut led, |w| write_link(w, event, &record)).expect("encode");
            let mut reader = Reader::new(&bytes);
            let (_, decoded) = read_link(&mut reader, &mut led).expect("decode");
            assert_eq!(
                decoded.legacy_id.as_deref(),
                Some("original-link"),
                "{event:?} must preserve the legacy id"
            );
        }
    }

    #[test]
    fn the_link_excerpt_is_a_reference_not_content() {
        let record = evidence(1);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| {
            write_evidence(w, EvidenceEvent::Created, &record)
        })
        .expect("encode");
        assert!(
            bytes.len() < 500,
            "evidence carries hashes, not the excerpt bytes"
        );
    }
}
