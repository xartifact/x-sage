//! Natural-key record codecs: assets, KB metadata, models, rule configuration,
//! import bookkeeping and committed request outcomes.
//!
//! These six families carry **no** [`RecordKind`] key. Their identity is content
//! (`sha256`), a configured name (`model_key`, `import_session_id`), a pair of strings
//! (`principal_id` + `request_id`), or the fact that exactly one exists (`KbMetadata`,
//! `RuleConfig`). A numeric key would invent an identity the record does not have, so the
//! identified-record codecs cannot be reused here.
//!
//! ## The three axes, applied per family
//!
//! Every field is one of: *must* appear, *may* appear (absence = the whole field is
//! omitted, never a zero-length value, §8.1), or *foreign* to this family and refused even
//! though the registry knows the tag. The distinction is load-bearing: registry acceptance
//! only says "this major has a meaning for tag N", not "this record may carry it".
//!
//! `5 recorded_by` is never in a required list. Its presence is a function of
//! `6 actor_origin` (§6.1's joint table): `Authenticated` requires it and `Unavailable`
//! forbids it. Listing it as unconditionally required would make every
//! `actor_origin = 2` record undecodable - the exact case where the specification
//! *forbids* the field.
//!
//! ## What is deliberately not here
//!
//! * **No revision on immutable records.** `AssetDeclared`, `RequestOutcomeRecorded` and
//!   `ImportBookkeeping` forbid tag `2`: a content address cannot be revised, a first
//!   outcome cannot be rewritten, and an import session is a stage *sequence* ordered by
//!   `created_at`, not a revision chain.
//! * **No status on Asset, KbMetadata or RuleConfig.** §7.3 defines no `4` domain for
//!   them, so writing one would be a value no reader could check.
//! * **No product admission policy.** Whether an image evidence has an asset, whether a
//!   stage transition is legal, whether a model key is already registered - all P2. This
//!   layer records what happened; refusing a legal historical record because today's
//!   product rules dislike it would make the log unfaithful.

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, AssetRecord, CommonFields, CountPair, CreationAudit, ImportSessionRecord,
    ImportStage, KbMetadataRecord, MediaType, ModelRecord, ModelStatus, OptionalActorAudit,
    RecordKind, RequestOutcomeEvent, RuleConfigRecord, StateKind,
};
use sage_db_types::relation::RuleProfile;

use crate::event::read_event;
use crate::field::{
    MAX_DECLARED_CAPTURED_BY, MAX_IDENTITY_BYTES, MAX_MODEL_ID_BYTES, MAX_NAME_BYTES,
    MAX_PRINCIPAL_ID_BYTES, MAX_REQUEST_ID_BYTES, MAX_SESSION_ID_BYTES, MAX_SLUG_BYTES,
    MAX_TEXT_BYTES, read_actor_origin, read_extensions, read_str, read_str_list,
    write_actor_origin, write_extensions, write_str, write_str_list,
};
use crate::record::{
    FieldSet, check_policy, decode_changed_fields, decode_field, decode_opt_field, reason_required,
    write_changed_fields,
};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, EncodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: a content-addressed asset was declared.
pub const ASSET_DECLARED: u16 = 64;
/// Event tag: the singleton KB metadata gained a revision.
pub const KB_METADATA_UPDATED: u16 = 144;
/// Event tag: a model's first revision.
pub const MODEL_REGISTERED: u16 = 145;
/// Event tag: a later model revision.
pub const MODEL_UPDATED: u16 = 146;
/// Event tag: a model was retired.
pub const MODEL_RETIRED: u16 = 147;
/// Event tag: the singleton rule configuration migrated.
pub const RULE_PROFILE_CHANGED: u16 = 160;
/// Event tag: a committed request's durable outcome.
pub const REQUEST_OUTCOME_RECORDED: u16 = 192;
/// Event tag: one import-session stage.
pub const IMPORT_BOOKKEEPING: u16 = 208;

/// Per-element width of `kb_domains` (tag 148) and `domains` (tag 19).
const MAX_DOMAIN_BYTES: u32 = 128;
/// Maximum `kb_domains` items (tag 148).
const MAX_DOMAINS: u32 = 64;
/// Maximum `expected_counts` pairs (tag 186).
const MAX_EXPECTED_COUNTS: u32 = 32;
/// Inclusive embedding-width bound for tags 99 and 152.
const DIMENSIONS: core::ops::RangeInclusive<u32> = 1..=4096;

mod t {
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const STATUS: u16 = 4;
    pub(crate) const RECORDED_BY: u16 = 5;
    pub(crate) const ACTOR_ORIGIN: u16 = 6;
    pub(crate) const REASON: u16 = 7;
    pub(crate) const CREATED_AT: u16 = 8;
    pub(crate) const UPDATED_AT: u16 = 9;
    pub(crate) const CHANGED_FIELDS: u16 = 13;
    pub(crate) const EXTENSIONS: u16 = 14;
    // Asset.
    pub(crate) const SHA256: u16 = 84;
    pub(crate) const MEDIA_TYPE: u16 = 85;
    pub(crate) const BYTE_SIZE: u16 = 86;
    // KB metadata.
    pub(crate) const KB_UUID: u16 = 144;
    pub(crate) const KB_SLUG: u16 = 145;
    pub(crate) const KB_NAME: u16 = 146;
    pub(crate) const KB_DESCRIPTION: u16 = 147;
    pub(crate) const KB_DOMAINS: u16 = 148;
    pub(crate) const LEGACY_FRONTIER_REF: u16 = 153;
    // Model.
    pub(crate) const MODEL_KEY: u16 = 149;
    pub(crate) const MODEL_IDENTIFIER: u16 = 150;
    pub(crate) const MODEL_PROVIDER: u16 = 151;
    pub(crate) const MODEL_DIMENSIONS: u16 = 152;
    // Rule configuration.
    pub(crate) const FROM_PROFILE: u16 = 160;
    pub(crate) const TO_PROFILE: u16 = 161;
    pub(crate) const DRY_RUN_REPORT_REF: u16 = 162;
    pub(crate) const RULES_DIGEST: u16 = 163;
    pub(crate) const RULES_REF: u16 = 166;
    // Asset declaration provenance.
    pub(crate) const DECLARED_BY: u16 = 172;
    pub(crate) const DECLARED_AT: u16 = 173;
    // Request outcome.
    pub(crate) const PRINCIPAL_ID: u16 = 176;
    pub(crate) const REQUEST_ID: u16 = 177;
    pub(crate) const PAYLOAD_DIGEST: u16 = 178;
    pub(crate) const RESULT_KIND: u16 = 179;
    pub(crate) const RESULT_REF: u16 = 180;
    pub(crate) const COMMIT_VERSION: u16 = 181;
    // Import bookkeeping.
    pub(crate) const IMPORT_SESSION_ID: u16 = 182;
    pub(crate) const IMPORT_STAGE: u16 = 183;
    pub(crate) const SOURCE_DIGEST: u16 = 184;
    pub(crate) const PLAN_REF: u16 = 185;
    pub(crate) const EXPECTED_COUNTS: u16 = 186;
    pub(crate) const IMPORT_COMPLETE: u16 = 187;
}

/// The only `result_kind` format major 0 can persist.
///
/// A business rejection is a committed transaction whose *event stream* records the
/// decision; a transport-level unknown is not a durable result and must resolve on retry.
/// Persisting either as a distinct `result_kind` would freeze a recoverable uncertainty
/// into a terminal state (§7.3).
const RESULT_KIND_COMMITTED: u16 = 0;

// ---------------------------------------------------------------------------
// Field policies
// ---------------------------------------------------------------------------

const ASSET_FIELDS: &[u16] = &[
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::EXTENSIONS,
    t::SHA256,
    t::MEDIA_TYPE,
    t::BYTE_SIZE,
    t::DECLARED_BY,
    t::DECLARED_AT,
];
const ASSET_REQUIRED: &[u16] = &[
    t::ACTOR_ORIGIN,
    t::CREATED_AT,
    t::SHA256,
    t::MEDIA_TYPE,
    t::BYTE_SIZE,
];

const KB_METADATA_FIELDS: &[u16] = &[
    t::REVISION,
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::CHANGED_FIELDS,
    t::EXTENSIONS,
    t::KB_UUID,
    t::KB_SLUG,
    t::KB_NAME,
    t::KB_DESCRIPTION,
    t::KB_DOMAINS,
    t::LEGACY_FRONTIER_REF,
];
const KB_METADATA_REQUIRED: &[u16] = &[
    t::REVISION,
    t::ACTOR_ORIGIN,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::KB_UUID,
    t::KB_SLUG,
    t::KB_NAME,
    t::KB_DESCRIPTION,
    t::KB_DOMAINS,
];

const MODEL_FIELDS: &[u16] = &[
    t::REVISION,
    t::STATUS,
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::CHANGED_FIELDS,
    t::EXTENSIONS,
    t::MODEL_KEY,
    t::MODEL_IDENTIFIER,
    t::MODEL_PROVIDER,
    t::MODEL_DIMENSIONS,
];

const RULE_CONFIG_FIELDS: &[u16] = &[
    t::REVISION,
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::CHANGED_FIELDS,
    t::EXTENSIONS,
    t::FROM_PROFILE,
    t::TO_PROFILE,
    t::DRY_RUN_REPORT_REF,
    t::RULES_DIGEST,
    t::RULES_REF,
];
const RULE_CONFIG_REQUIRED: &[u16] = &[
    t::REVISION,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::FROM_PROFILE,
    t::TO_PROFILE,
    t::RULES_DIGEST,
    t::RULES_REF,
];

const OUTCOME_FIELDS: &[u16] = &[
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::EXTENSIONS,
    t::PRINCIPAL_ID,
    t::REQUEST_ID,
    t::PAYLOAD_DIGEST,
    t::RESULT_KIND,
    t::RESULT_REF,
    t::COMMIT_VERSION,
];

/// Fields every `192 RequestOutcomeRecorded` event must carry.
///
/// `5 recorded_by` **is** unconditional here, unlike every other natural-key event. That
/// is a deliberate event-specific narrowing of §6.1's class-level "conditional（依
/// `actor_origin`）": row 192 lists `5` in 专属必现 while its 专属条件 column spells out
/// only `7` (every revise row, by contrast, restates `14（有值必现）`), and 192 is the sole
/// event of the 30 whose 专属必现 includes tag 5.
///
/// The governing reason is not the list itself but what `176 principal_id` means: this
/// event names the executor of a transaction the store just committed. `actor_origin = 2`
/// says "the history exists but its actor was never archived", which cannot describe a
/// principal this same event is required to carry. So both arms of origin 2 fail: with
/// tag 5 it fabricates an actor (§6.1), and without it the row's own list is violated.
/// Requiring tag 5 makes `origin = 2` inadmissible for this event without inventing any
/// alternative global rule - the class conditional stands unchanged for all 29 others.
const OUTCOME_REQUIRED: &[u16] = &[
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::CREATED_AT,
    t::PRINCIPAL_ID,
    t::REQUEST_ID,
    t::PAYLOAD_DIGEST,
    t::RESULT_KIND,
    t::RESULT_REF,
    t::COMMIT_VERSION,
];

const IMPORT_FIELDS: &[u16] = &[
    t::RECORDED_BY,
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::UPDATED_AT,
    t::EXTENSIONS,
    t::IMPORT_SESSION_ID,
    t::IMPORT_STAGE,
    t::SOURCE_DIGEST,
    t::PLAN_REF,
    t::EXPECTED_COUNTS,
    t::IMPORT_COMPLETE,
];
const IMPORT_REQUIRED: &[u16] = &[
    t::ACTOR_ORIGIN,
    t::REASON,
    t::CREATED_AT,
    t::IMPORT_SESSION_ID,
    t::IMPORT_STAGE,
    t::SOURCE_DIGEST,
    t::PLAN_REF,
    t::EXPECTED_COUNTS,
    t::IMPORT_COMPLETE,
];

const fn policy(fields: &'static [u16], required: &'static [u16]) -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(fields, required)
}

// ---------------------------------------------------------------------------
// Which event a family is encoding
// ---------------------------------------------------------------------------

/// A model event, which shares one record type across three tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelEvent {
    /// First revision; status must be active.
    Registered,
    /// A later revision; status may be either, because reviving is an explicit act.
    Updated,
    /// Retirement; status must be retired and `reason` is mandatory.
    Retired,
}

impl ModelEvent {
    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Registered => MODEL_REGISTERED,
            Self::Updated => MODEL_UPDATED,
            Self::Retired => MODEL_RETIRED,
        }
    }

    /// Decode from a wire tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            MODEL_REGISTERED => Some(Self::Registered),
            MODEL_UPDATED => Some(Self::Updated),
            MODEL_RETIRED => Some(Self::Retired),
            _ => None,
        }
    }

    const fn is_creation(self) -> bool {
        matches!(self, Self::Registered)
    }

    /// The fields this specific event must carry.
    ///
    /// `145` has no update time because the revision is the record's first; `146` and
    /// `147` do, and `147` additionally requires the reason. Expressing that per event
    /// rather than with one union keeps `145` from accepting a time it cannot mean.
    const fn required(self) -> &'static [u16] {
        match self {
            Self::Registered => &[
                t::REVISION,
                t::STATUS,
                t::ACTOR_ORIGIN,
                t::CREATED_AT,
                t::MODEL_KEY,
                t::MODEL_IDENTIFIER,
                t::MODEL_DIMENSIONS,
            ],
            Self::Updated => &[
                t::REVISION,
                t::STATUS,
                t::ACTOR_ORIGIN,
                t::CREATED_AT,
                t::UPDATED_AT,
                t::MODEL_KEY,
                t::MODEL_IDENTIFIER,
                t::MODEL_DIMENSIONS,
            ],
            Self::Retired => &[
                t::REVISION,
                t::STATUS,
                t::ACTOR_ORIGIN,
                t::REASON,
                t::CREATED_AT,
                t::UPDATED_AT,
                t::MODEL_KEY,
                t::MODEL_IDENTIFIER,
                t::MODEL_DIMENSIONS,
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Shared audit checks
// ---------------------------------------------------------------------------

/// The actor/origin rule, in one place so no family can omit it.
///
/// `Authenticated` requires an identity; `Unavailable` forbids one. `LegacyDeclared`
/// permits either, because an import may or may not have known who ran it. Stated over
/// `Option<ActorOrigin>` so a Genesis projection - where the field is absent entirely -
/// has no actor to police and none is fabricated.
fn check_actor(actor: Option<ActorOrigin>, recorded_by: Option<&str>) -> Result<(), WriteError> {
    match (actor, recorded_by.is_some()) {
        (Some(ActorOrigin::Authenticated), false) | (Some(ActorOrigin::Unavailable), true) => {
            Err(WriteError::Data(EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }))
        }
        _ => Ok(()),
    }
}

/// The read-side mirror of [`check_actor`].
fn read_actor(actor: Option<ActorOrigin>, recorded_by: Option<&str>) -> Result<(), ReadError> {
    match (actor, recorded_by) {
        (Some(ActorOrigin::Authenticated), None) | (Some(ActorOrigin::Unavailable), Some(_)) => {
            Err(ReadError::Data(DecodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }))
        }
        _ => Ok(()),
    }
}

/// `reason` is mandatory for the events §7.2 names as terminal or administrative.
fn check_reason(reason: Option<&str>, event_tag: u16) -> Result<(), WriteError> {
    if reason_required(event_tag) && reason.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event_tag,
            field: t::REASON,
        }));
    }
    Ok(())
}

/// The read-side mirror of [`check_reason`].
fn read_reason(event: &crate::event::Event<'_>) -> Result<(), ReadError> {
    if reason_required(event.tag()) && event.field(t::REASON).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::REASON,
        }));
    }
    Ok(())
}

/// Every natural-key record event's shared inline audit fields, decoded once.
type AuditTail = (
    Option<ActorOrigin>,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
);

fn read_audit(
    event: &crate::event::Event<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<AuditTail, ReadError> {
    let actor = decode_field(event, t::ACTOR_ORIGIN, read_actor_origin)?;
    let recorded_by = decode_opt_field(event, t::RECORDED_BY, |r| {
        read_str(r, MAX_IDENTITY_BYTES, ledger)
    })?;
    read_actor(Some(actor), recorded_by.as_deref())?;
    read_reason(event)?;
    let reason = decode_opt_field(event, t::REASON, |r| read_str(r, MAX_TEXT_BYTES, ledger))?;
    let created_at = decode_field(event, t::CREATED_AT, |r| r.i64())?;
    let updated_at = decode_opt_field(event, t::UPDATED_AT, |r| r.i64())?;
    Ok((Some(actor), recorded_by, reason, created_at, updated_at))
}

/// The read-side UUID: a 16-byte fixed field, never truncated to fit.
fn read_uuid(
    event: &crate::event::Event<'_>,
) -> Result<sage_db_types::identity::KbUuid, ReadError> {
    let raw = crate::record::require(event, t::KB_UUID)?;
    let bytes: [u8; 16] = raw.try_into().map_err(|_| {
        ReadError::Data(DecodeError::Truncated {
            offset: 0,
            wanted: 16,
        })
    })?;
    Ok(sage_db_types::identity::KbUuid::from_bytes(bytes))
}

// ---------------------------------------------------------------------------
// 64 AssetDeclared
// ---------------------------------------------------------------------------

/// Write an `AssetDeclared` event.
///
/// # Errors
///
/// [`WriteError::Data`] for `byte_size == 0` (no asset has zero bytes), an actor/origin
/// mismatch, or an over-length declaration string; plus budget and allocation refusals.
pub fn write_asset(writer: &mut Writer<'_>, record: &AssetRecord) -> Result<(), WriteError> {
    if record.byte_size == 0 {
        return Err(WriteError::Data(EncodeError::UnknownEnumValue {
            tag: t::BYTE_SIZE,
            value: 0,
        }));
    }
    check_actor(
        Some(record.audit.actor_origin),
        record.audit.recorded_by.as_deref(),
    )?;
    check_reason(record.audit.reason.as_deref(), ASSET_DECLARED)?;

    let mut set = FieldSet::with_capacity(ASSET_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 5, 6, 7, 8, 14, 84, 85, 86, 172, 173.
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.audit.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.audit.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(ledger, t::REASON, record.audit.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.audit.created_at, |w, v| {
        w.i64(v)
    })?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::SHA256, record.sha256, |w, v| w.bytes(&v))?;
    set.push(
        ledger,
        t::MEDIA_TYPE,
        record.media_type,
        |w, v: MediaType| w.u16(v.as_u16()),
    )?;
    set.push(ledger, t::BYTE_SIZE, record.byte_size, |w, v| w.u64(v))?;
    set.push_opt(
        ledger,
        t::DECLARED_BY,
        record.declared_by.as_deref(),
        |w, v| write_str(w, v, MAX_DECLARED_CAPTURED_BY),
    )?;
    set.push_opt(ledger, t::DECLARED_AT, record.declared_at, |w, v| w.i64(v))?;
    set.write_event(writer, ASSET_DECLARED)
}

/// Decode an `AssetDeclared` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, an unregistered media type,
/// `byte_size == 0`, an actor/origin mismatch or a malformed string;
/// [`ReadError::Uninterpretable`] for a foreign tag or a registry refusal.
pub fn read_asset(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<AssetRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != ASSET_DECLARED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &policy(ASSET_FIELDS, ASSET_REQUIRED))?;

    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    // An asset is immutable, so "when did it change" has no answer rather than a zero.
    if updated_at.is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: ASSET_DECLARED,
            field: t::UPDATED_AT,
        }));
    }
    let sha256 = decode_field(&event, t::SHA256, |r| r.digest32())?;
    let media_raw = decode_field(&event, t::MEDIA_TYPE, |r| r.u16())?;
    let media_type =
        MediaType::from_u16(media_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::MEDIA_TYPE,
            value: media_raw,
        }))?;
    let byte_size = decode_field(&event, t::BYTE_SIZE, |r| r.u64())?;
    if byte_size == 0 {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::BYTE_SIZE,
            value: 0,
        }));
    }
    let declared_by = decode_opt_field(&event, t::DECLARED_BY, |r| {
        read_str(r, MAX_DECLARED_CAPTURED_BY, ledger)
    })?;
    let declared_at = decode_opt_field(&event, t::DECLARED_AT, |r| r.i64())?;
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    // Exit checkpoint: a fully-read event must still observe a cancelled attempt.
    ledger.checkpoint()?;

    Ok(AssetRecord {
        sha256,
        media_type,
        byte_size,
        declared_by,
        declared_at,
        audit: CreationAudit {
            recorded_by,
            actor_origin: actor.unwrap_or(ActorOrigin::Unavailable),
            reason,
            created_at,
        },
        extensions,
    })
}

// ---------------------------------------------------------------------------
// 144 KbMetadataUpdated
// ---------------------------------------------------------------------------

/// Write a `KbMetadataUpdated` event.
///
/// # Errors
///
/// [`WriteError::Data`] for revision 1 (genesis owns that revision), a missing update
/// time, a Genesis-shaped record whose `actor` is absent, or an over-length string; plus
/// budget and allocation refusals.
pub fn write_kb_metadata(
    writer: &mut Writer<'_>,
    record: &KbMetadataRecord,
) -> Result<(), WriteError> {
    if record.revision == RecordRevision::FIRST {
        // Revision 1 of the singleton is genesis's, so an update cannot claim it: two
        // events would otherwise both claim to be the first revision.
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: KB_METADATA_UPDATED,
            revision: record.revision.get(),
        }));
    }
    if record.audit.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: KB_METADATA_UPDATED,
            field: t::UPDATED_AT,
        }));
    }
    // Tag 6 is required by this event's class, so `None` (the genesis-only shape) is not
    // encodable here.
    let actor = record
        .audit
        .actor
        .ok_or(WriteError::Data(EncodeError::MissingRequiredField {
            event: KB_METADATA_UPDATED,
            field: t::ACTOR_ORIGIN,
        }))?;
    check_actor(Some(actor), record.audit.recorded_by.as_deref())?;
    check_reason(record.audit.reason.as_deref(), KB_METADATA_UPDATED)?;

    let mut set = FieldSet::with_capacity(KB_METADATA_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 2, 5, 6, 7, 8, 9, 13, 14, 144..148, 153.
    set.push(
        ledger,
        t::REVISION,
        record.revision,
        crate::field::write_revision,
    )?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.audit.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(ledger, t::ACTOR_ORIGIN, actor, write_actor_origin)?;
    set.push_opt(ledger, t::REASON, record.audit.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.audit.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.audit.updated_at, |w, v| {
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
    set.push(
        ledger,
        t::KB_UUID,
        record.kb_uuid,
        |w, v: sage_db_types::identity::KbUuid| w.bytes(v.as_bytes()),
    )?;
    set.push(ledger, t::KB_SLUG, record.slug.as_str(), |w, v| {
        write_str(w, v, MAX_SLUG_BYTES)
    })?;
    set.push(ledger, t::KB_NAME, record.name.as_str(), |w, v| {
        write_str(w, v, MAX_NAME_BYTES)
    })?;
    set.push(
        ledger,
        t::KB_DESCRIPTION,
        record.description.as_str(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::KB_DOMAINS, record.domains.as_slice(), |w, v| {
        write_str_list(w, v, MAX_DOMAINS, MAX_DOMAIN_BYTES)
    })?;
    set.push_opt(
        ledger,
        t::LEGACY_FRONTIER_REF,
        record.legacy_frontier_ref,
        |w, v| w.bytes(&v),
    )?;
    set.write_event(writer, KB_METADATA_UPDATED)
}

/// Decode a `KbMetadataUpdated` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, revision 1, a missing update
/// time, a `kb_uuid` that is not 16 bytes or a malformed string/list;
/// [`ReadError::Uninterpretable`] for a foreign tag or a registry refusal.
pub fn read_kb_metadata(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<KbMetadataRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != KB_METADATA_UPDATED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &policy(KB_METADATA_FIELDS, KB_METADATA_REQUIRED))?;

    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: KB_METADATA_UPDATED,
            revision: revision.get(),
        }));
    }
    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    if updated_at.is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: KB_METADATA_UPDATED,
            field: t::UPDATED_AT,
        }));
    }
    let changed_fields = decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?;
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    let kb_uuid = read_uuid(&event)?;
    let slug = decode_field(&event, t::KB_SLUG, |r| read_str(r, MAX_SLUG_BYTES, ledger))?;
    let name = decode_field(&event, t::KB_NAME, |r| read_str(r, MAX_NAME_BYTES, ledger))?;
    let description = decode_field(&event, t::KB_DESCRIPTION, |r| {
        read_str(r, MAX_TEXT_BYTES, ledger)
    })?;
    let domains = decode_field(&event, t::KB_DOMAINS, |r| {
        read_str_list(r, MAX_DOMAINS, MAX_DOMAIN_BYTES, ledger)
    })?;
    ledger.checkpoint()?;

    Ok(KbMetadataRecord {
        revision,
        kb_uuid,
        slug,
        name,
        description,
        domains,
        legacy_frontier_ref: decode_opt_field(&event, t::LEGACY_FRONTIER_REF, |r| r.digest32())?,
        audit: OptionalActorAudit {
            actor,
            recorded_by,
            reason,
            created_at,
            updated_at,
        },
        extensions,
        changed_fields,
    })
}

// ---------------------------------------------------------------------------
// 145/146/147 Model
// ---------------------------------------------------------------------------

/// Write a model event.
///
/// # Errors
///
/// [`WriteError::Data`] for a revision/event mismatch, a status that contradicts the
/// event, a missing retirement reason or update time, or an out-of-range dimension count;
/// plus budget and allocation refusals.
pub fn write_model(
    writer: &mut Writer<'_>,
    event: ModelEvent,
    record: &ModelRecord,
) -> Result<(), WriteError> {
    if event.is_creation() {
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event.tag(),
                revision: record.revision.get(),
            }));
        }
        if record.status != ModelStatus::Active {
            return Err(WriteError::Data(EncodeError::StatusMismatch {
                event: event.tag(),
            }));
        }
    } else if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if event == ModelEvent::Retired && record.status != ModelStatus::Retired {
        return Err(WriteError::Data(EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    if !DIMENSIONS.contains(&record.dimensions) {
        return Err(WriteError::Data(EncodeError::UnknownEnumValue {
            tag: t::MODEL_DIMENSIONS,
            value: record.dimensions,
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
    check_actor(
        Some(record.common.actor_origin),
        record.common.recorded_by.as_deref(),
    )?;
    check_reason(record.common.reason.as_deref(), event.tag())?;

    let mut set = FieldSet::with_capacity(MODEL_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 2, 4, 5, 6, 7, 8, 9, 13, 14, 149..152.
    set.push(
        ledger,
        t::REVISION,
        record.revision,
        crate::field::write_revision,
    )?;
    set.push(ledger, t::STATUS, record.status, |w, v: ModelStatus| {
        w.u16(v.as_u16())
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
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::MODEL_KEY, record.model_key.as_str(), |w, v| {
        write_str(w, v, MAX_MODEL_ID_BYTES)
    })?;
    set.push(
        ledger,
        t::MODEL_IDENTIFIER,
        record.identifier.as_str(),
        |w, v| write_str(w, v, MAX_MODEL_ID_BYTES),
    )?;
    set.push_opt(
        ledger,
        t::MODEL_PROVIDER,
        record.provider.as_deref(),
        |w, v| write_str(w, v, MAX_MODEL_ID_BYTES),
    )?;
    set.push(ledger, t::MODEL_DIMENSIONS, record.dimensions, |w, v| {
        w.u32(v)
    })?;
    set.write_event(writer, event.tag())
}

/// Decode a model event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, an unregistered status, a
/// revision/event mismatch, a missing retirement reason or update time, or an
/// out-of-range dimension count; [`ReadError::Uninterpretable`] for a foreign tag or a
/// registry refusal.
pub fn read_model(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(ModelEvent, ModelRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    let kind = ModelEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;
    accept_event(&event)?;
    check_policy(&event, &policy(MODEL_FIELDS, kind.required()))?;

    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if kind.is_creation() {
        if revision != RecordRevision::FIRST {
            return Err(ReadError::Data(DecodeError::WrongRevision {
                event: event.tag(),
                revision: revision.get(),
            }));
        }
    } else if revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    let status_raw = decode_field(&event, t::STATUS, |r| r.u16())?;
    let status =
        ModelStatus::from_u16(status_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status_raw,
        }))?;
    if kind == ModelEvent::Registered && status != ModelStatus::Active {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    if kind == ModelEvent::Retired && status != ModelStatus::Retired {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
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
    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    // A registration is revision 1, which cannot have an update time; already refused
    // above. Later revisions must have one.
    if !kind.is_creation() && updated_at.is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    let changed_fields = decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?;
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    let model_key = decode_field(&event, t::MODEL_KEY, |r| {
        read_str(r, MAX_MODEL_ID_BYTES, ledger)
    })?;
    let identifier = decode_field(&event, t::MODEL_IDENTIFIER, |r| {
        read_str(r, MAX_MODEL_ID_BYTES, ledger)
    })?;
    let provider = decode_opt_field(&event, t::MODEL_PROVIDER, |r| {
        read_str(r, MAX_MODEL_ID_BYTES, ledger)
    })?;
    let dimensions = decode_field(&event, t::MODEL_DIMENSIONS, |r| r.u32())?;
    if !DIMENSIONS.contains(&dimensions) {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::MODEL_DIMENSIONS,
            value: u16::try_from(dimensions).unwrap_or(u16::MAX),
        }));
    }
    ledger.checkpoint()?;

    Ok((
        kind,
        ModelRecord {
            revision,
            model_key,
            identifier,
            provider,
            dimensions,
            status,
            common: CommonFields {
                recorded_by,
                actor_origin: actor.unwrap_or(ActorOrigin::Unavailable),
                reason,
                created_at,
                updated_at,
            },
            extensions,
            changed_fields,
        },
    ))
}

// ---------------------------------------------------------------------------
// 160 RuleProfileChanged
// ---------------------------------------------------------------------------

/// Write a `RuleProfileChanged` event.
///
/// # Errors
///
/// [`WriteError::Data`] for revision 1 (genesis owns that revision), a missing
/// `from_profile` or reason or update time, a Genesis-shaped record whose `actor` is
/// absent, or an over-length string; plus budget and allocation refusals.
pub fn write_rule_profile_changed(
    writer: &mut Writer<'_>,
    record: &RuleConfigRecord,
) -> Result<(), WriteError> {
    // Tag 160 is required by this event. A record without it would be re-encoded as a
    // different event, which is the failure "recognized data silently dropped" names.
    let from = record
        .from_profile
        .ok_or(WriteError::Data(EncodeError::MissingRequiredField {
            event: RULE_PROFILE_CHANGED,
            field: t::FROM_PROFILE,
        }))?;
    if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: RULE_PROFILE_CHANGED,
            revision: record.revision.get(),
        }));
    }
    if record.audit.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: RULE_PROFILE_CHANGED,
            field: t::UPDATED_AT,
        }));
    }
    let actor = record
        .audit
        .actor
        .ok_or(WriteError::Data(EncodeError::MissingRequiredField {
            event: RULE_PROFILE_CHANGED,
            field: t::ACTOR_ORIGIN,
        }))?;
    check_actor(Some(actor), record.audit.recorded_by.as_deref())?;
    check_reason(record.audit.reason.as_deref(), RULE_PROFILE_CHANGED)?;

    let mut set = FieldSet::with_capacity(RULE_CONFIG_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 2, 5, 6, 7, 8, 9, 13, 14, 160, 161, 162, 163, 166.
    set.push(
        ledger,
        t::REVISION,
        record.revision,
        crate::field::write_revision,
    )?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.audit.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(ledger, t::ACTOR_ORIGIN, actor, write_actor_origin)?;
    set.push_opt(ledger, t::REASON, record.audit.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.audit.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.audit.updated_at, |w, v| {
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
    set.push(ledger, t::FROM_PROFILE, from, |w, v: RuleProfile| {
        w.u16(v.as_u16())
    })?;
    set.push(
        ledger,
        t::TO_PROFILE,
        record.profile,
        |w, v: RuleProfile| w.u16(v.as_u16()),
    )?;
    set.push_opt(
        ledger,
        t::DRY_RUN_REPORT_REF,
        record.dry_run_report_ref,
        |w, v| w.bytes(&v),
    )?;
    set.push(ledger, t::RULES_DIGEST, record.rules_digest, |w, v| {
        w.bytes(&v)
    })?;
    set.push(ledger, t::RULES_REF, record.rules_ref, |w, v| w.bytes(&v))?;
    set.write_event(writer, RULE_PROFILE_CHANGED)
}

/// Decode a `RuleProfileChanged` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, revision 1, an unregistered
/// profile, a missing reason or update time; [`ReadError::Uninterpretable`] for a foreign
/// tag or a registry refusal.
pub fn read_rule_profile_changed(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<RuleConfigRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != RULE_PROFILE_CHANGED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &policy(RULE_CONFIG_FIELDS, RULE_CONFIG_REQUIRED))?;

    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: RULE_PROFILE_CHANGED,
            revision: revision.get(),
        }));
    }
    let from_raw = decode_field(&event, t::FROM_PROFILE, |r| r.u16())?;
    let from_profile =
        RuleProfile::from_u16(from_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::FROM_PROFILE,
            value: from_raw,
        }))?;
    let to_raw = decode_field(&event, t::TO_PROFILE, |r| r.u16())?;
    let profile =
        RuleProfile::from_u16(to_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::TO_PROFILE,
            value: to_raw,
        }))?;
    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    if updated_at.is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: RULE_PROFILE_CHANGED,
            field: t::UPDATED_AT,
        }));
    }
    let changed_fields = decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?;
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    let dry_run_report_ref = decode_opt_field(&event, t::DRY_RUN_REPORT_REF, |r| r.digest32())?;
    let rules_digest = decode_field(&event, t::RULES_DIGEST, |r| r.digest32())?;
    let rules_ref = decode_field(&event, t::RULES_REF, |r| r.digest32())?;
    ledger.checkpoint()?;

    Ok(RuleConfigRecord {
        revision,
        profile,
        from_profile: Some(from_profile),
        rules_digest,
        rules_ref,
        dry_run_report_ref,
        audit: OptionalActorAudit {
            actor,
            recorded_by,
            reason,
            created_at,
            updated_at,
        },
        extensions,
        changed_fields,
    })
}

// ---------------------------------------------------------------------------
// 192 RequestOutcomeRecorded
// ---------------------------------------------------------------------------

/// Write a `RequestOutcomeRecorded` event.
///
/// # Errors
///
/// [`WriteError::Data`] when `5 recorded_by` is absent (unconditional for this event, so
/// `origin = 2` is inadmissible here), for a zero commit version, an actor/origin
/// mismatch, or an over-length string; plus budget and allocation refusals.
pub fn write_request_outcome(
    writer: &mut Writer<'_>,
    record: &RequestOutcomeEvent,
) -> Result<(), WriteError> {
    // Checked before the actor join so the two negatives stay distinguishable: absence is
    // MissingRequiredField{192,5}; an origin-2 event that *does* carry 5 is
    // ActorOriginMismatch{5}. Both fire before any buffer exists, so an illegal encode
    // publishes nothing.
    if record.audit.recorded_by.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: REQUEST_OUTCOME_RECORDED,
            field: t::RECORDED_BY,
        }));
    }
    if record.commit_version == 0 {
        return Err(WriteError::Data(EncodeError::UnknownEnumValue {
            tag: t::COMMIT_VERSION,
            value: 0,
        }));
    }
    check_actor(
        Some(record.audit.actor_origin),
        record.audit.recorded_by.as_deref(),
    )?;
    check_reason(record.audit.reason.as_deref(), REQUEST_OUTCOME_RECORDED)?;

    let mut set = FieldSet::with_capacity(OUTCOME_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 5, 6, 7, 8, 9, 14, 176..181.
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.audit.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.audit.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(ledger, t::REASON, record.audit.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.audit.created_at, |w, v| {
        w.i64(v)
    })?;
    // `9 updated_at` is permitted here (an outcome is written once, but the field table
    // does not forbid a stamp) and has no slot on the record, so it is refused on read
    // rather than accepted and dropped. An outcome is never revised, so a caller has no
    // reason to supply one.
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(
        ledger,
        t::PRINCIPAL_ID,
        record.principal_id.as_str(),
        |w, v| write_str(w, v, MAX_PRINCIPAL_ID_BYTES),
    )?;
    set.push(ledger, t::REQUEST_ID, record.request_id.as_str(), |w, v| {
        write_str(w, v, MAX_REQUEST_ID_BYTES)
    })?;
    set.push(ledger, t::PAYLOAD_DIGEST, record.payload_digest, |w, v| {
        w.bytes(&v)
    })?;
    set.push(ledger, t::RESULT_KIND, RESULT_KIND_COMMITTED, |w, v| {
        w.u16(v)
    })?;
    set.push(ledger, t::RESULT_REF, record.result_ref, |w, v| w.bytes(&v))?;
    set.push(ledger, t::COMMIT_VERSION, record.commit_version, |w, v| {
        w.u64(v)
    })?;
    set.write_event(writer, REQUEST_OUTCOME_RECORDED)
}

/// Decode a `RequestOutcomeRecorded` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, `result_kind != 0`, a zero commit
/// version, an over-length string or an actor/origin mismatch;
/// [`ReadError::Uninterpretable`] for a foreign tag or a registry refusal.
pub fn read_request_outcome(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<RequestOutcomeEvent, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != REQUEST_OUTCOME_RECORDED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &policy(OUTCOME_FIELDS, OUTCOME_REQUIRED))?;

    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    // 192 forbids tag 2 and has no update time on its record; accepting a stamp with
    // nowhere to keep it would be accept-and-drop.
    if updated_at.is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: REQUEST_OUTCOME_RECORDED,
            field: t::UPDATED_AT,
        }));
    }
    let principal_id = decode_field(&event, t::PRINCIPAL_ID, |r| {
        read_str(r, MAX_PRINCIPAL_ID_BYTES, ledger)
    })?;
    let request_id = decode_field(&event, t::REQUEST_ID, |r| {
        read_str(r, MAX_REQUEST_ID_BYTES, ledger)
    })?;
    let payload_digest = decode_field(&event, t::PAYLOAD_DIGEST, |r| r.digest32())?;
    let result_kind = decode_field(&event, t::RESULT_KIND, |r| r.u16())?;
    if result_kind != RESULT_KIND_COMMITTED {
        // §9.2: a `179` other than 0 is UNSUPPORTED_FORMAT, because it is a value the
        // registry does not define rather than bytes that contradict themselves.
        return Err(ReadError::Uninterpretable(SchemaError::UnknownField {
            event: REQUEST_OUTCOME_RECORDED,
            field: t::RESULT_KIND,
        }));
    }
    let result_ref = decode_field(&event, t::RESULT_REF, |r| r.digest32())?;
    let commit_version = decode_field(&event, t::COMMIT_VERSION, |r| r.u64())?;
    if commit_version == 0 {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::COMMIT_VERSION,
            value: 0,
        }));
    }
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    ledger.checkpoint()?;

    Ok(RequestOutcomeEvent {
        principal_id,
        request_id,
        payload_digest,
        result_ref,
        commit_version,
        audit: CreationAudit {
            recorded_by,
            actor_origin: actor.unwrap_or(ActorOrigin::Unavailable),
            reason,
            created_at,
        },
        extensions,
    })
}

// ---------------------------------------------------------------------------
// 208 ImportBookkeeping
// ---------------------------------------------------------------------------

/// Write an `ImportBookkeeping` event.
///
/// # Errors
///
/// [`WriteError::Data`] for a missing reason, a zero-length session id, an over-length
/// string, or unsorted/duplicate `expected_counts`; plus budget and allocation refusals.
pub fn write_import_bookkeeping(
    writer: &mut Writer<'_>,
    record: &ImportSessionRecord,
) -> Result<(), WriteError> {
    if record.import_session_id.is_empty() {
        return Err(WriteError::Data(EncodeError::StringTooLong));
    }
    // Ascending by state kind with no duplicate kind: the collection is a set keyed by
    // kind, so two counts for one kind cannot be folded deterministically.
    if record
        .expected_counts
        .windows(2)
        .any(|pair| pair[0].kind.as_u16() >= pair[1].kind.as_u16())
    {
        return Err(WriteError::Data(EncodeError::FieldOrder {
            tag: t::EXPECTED_COUNTS,
        }));
    }
    if record.expected_counts.len() > MAX_EXPECTED_COUNTS as usize {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    check_actor(
        Some(record.audit.actor_origin),
        record.audit.recorded_by.as_deref(),
    )?;
    check_reason(record.audit.reason.as_deref(), IMPORT_BOOKKEEPING)?;

    let mut set = FieldSet::with_capacity(IMPORT_FIELDS.len(), writer.ledger())?;
    let ledger = writer.ledger();
    // Ascending: 5, 6, 7, 8, 9, 14, 182..187.
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.audit.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.audit.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(ledger, t::REASON, record.audit.reason.as_deref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, record.audit.created_at, |w, v| {
        w.i64(v)
    })?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(
        ledger,
        t::IMPORT_SESSION_ID,
        record.import_session_id.as_str(),
        |w, v| write_str(w, v, MAX_SESSION_ID_BYTES),
    )?;
    set.push(
        ledger,
        t::IMPORT_STAGE,
        record.stage,
        |w, v: ImportStage| w.u16(v.as_u16()),
    )?;
    set.push(ledger, t::SOURCE_DIGEST, record.source_digest, |w, v| {
        w.bytes(&v)
    })?;
    set.push(ledger, t::PLAN_REF, record.plan_ref, |w, v| w.bytes(&v))?;
    set.push(
        ledger,
        t::EXPECTED_COUNTS,
        record.expected_counts.as_slice(),
        write_count_pairs,
    )?;
    set.push(
        ledger,
        t::IMPORT_COMPLETE,
        record.import_complete,
        |w, v: bool| w.u8(u8::from(v)),
    )?;
    set.write_event(writer, IMPORT_BOOKKEEPING)
}

/// Decode an `ImportBookkeeping` event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing or foreign field, an unregistered stage, a
/// non-boolean `import_complete`, unsorted/duplicate counts or a malformed string;
/// [`ReadError::Uninterpretable`] for a foreign tag or a registry refusal.
pub fn read_import_bookkeeping(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<ImportSessionRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    if event.tag() != IMPORT_BOOKKEEPING {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    accept_event(&event)?;
    check_policy(&event, &policy(IMPORT_FIELDS, IMPORT_REQUIRED))?;

    let (actor, recorded_by, reason, created_at, updated_at) = read_audit(&event, ledger)?;
    // A stage is written once; later stages are new events under the same session id.
    if updated_at.is_some() {
        return Err(ReadError::Data(DecodeError::ForbiddenField {
            event: IMPORT_BOOKKEEPING,
            field: t::UPDATED_AT,
        }));
    }
    let import_session_id = decode_field(&event, t::IMPORT_SESSION_ID, |r| {
        read_str(r, MAX_SESSION_ID_BYTES, ledger)
    })?;
    let stage_raw = decode_field(&event, t::IMPORT_STAGE, |r| r.u16())?;
    let stage =
        ImportStage::from_u16(stage_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::IMPORT_STAGE,
            value: stage_raw,
        }))?;
    let source_digest = decode_field(&event, t::SOURCE_DIGEST, |r| r.digest32())?;
    let plan_ref = decode_field(&event, t::PLAN_REF, |r| r.digest32())?;
    let expected_counts =
        decode_field(&event, t::EXPECTED_COUNTS, |r| read_count_pairs(r, ledger))?;
    let complete_raw = decode_field(&event, t::IMPORT_COMPLETE, |r| r.u8())?;
    let import_complete = match complete_raw {
        0 => false,
        1 => true,
        other => {
            return Err(ReadError::Data(DecodeError::UnknownEnum {
                tag: t::IMPORT_COMPLETE,
                value: u16::from(other),
            }));
        }
    };
    let extensions = decode_opt_field(&event, t::EXTENSIONS, |r| read_extensions(r, ledger))?
        .unwrap_or_default();
    ledger.checkpoint()?;

    Ok(ImportSessionRecord {
        import_session_id,
        stage,
        source_digest,
        plan_ref,
        expected_counts,
        import_complete,
        audit: CreationAudit {
            recorded_by,
            actor_origin: actor.unwrap_or(ActorOrigin::Unavailable),
            reason,
            created_at,
        },
        extensions,
    })
}

// ---------------------------------------------------------------------------
// Collection helpers
// ---------------------------------------------------------------------------

/// Write `expected_counts`: a `u16` state kind and a `u64` row count.
///
/// Deliberately not `write_watermark`: a count is not an id, and a 64-bit count field
/// next to a 128-bit watermark field is exactly how a three-row import would come to
/// claim three allocated identifiers.
fn write_count_pairs(writer: &mut Writer<'_>, pairs: &[CountPair]) -> Result<(), WriteError> {
    if u32::try_from(pairs.len()).map_or(true, |n| n > MAX_EXPECTED_COUNTS) {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    writer.count(pairs.len())?;
    for pair in pairs {
        writer.u16(pair.kind.as_u16())?;
        writer.u64(pair.count)?;
    }
    Ok(())
}

/// Read `expected_counts`, charging the retained slot array before reserving it.
///
/// # Errors
///
/// [`ReadError::Data`] for an impossible count, an out-of-order or duplicate kind, or a
/// count the wire cannot supply; [`ReadError::Uninterpretable`] for a state kind outside
/// the registered universe; plus budget and control refusals.
fn read_count_pairs(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<CountPair>, ReadError> {
    let count = reader.count(MAX_EXPECTED_COUNTS)?;
    // Feasibility first: each pair is a u16 kind and a u64 count.
    let minimum = (count as usize).saturating_mul(10);
    if minimum > reader.remaining() {
        return Err(ReadError::Data(DecodeError::Truncated {
            offset: reader.offset(),
            wanted: minimum,
        }));
    }
    ledger.try_charge_entries(u64::from(count))?;
    // Retained memory is the Rust slot size, not the 10-byte wire width.
    let slot_bytes = (count as u64).saturating_mul(core::mem::size_of::<CountPair>() as u64);
    ledger.try_charge(slot_bytes)?;
    ledger.checkpoint()?;
    let mut pairs = Vec::new();
    pairs
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            requested: slot_bytes as usize,
        })?;
    let mut previous: Option<u16> = None;
    for _ in 0..count {
        let kind_raw = reader.u16()?;
        // A state kind outside the closed registered universe is UNSUPPORTED, not
        // corrupt: the row space is a registry value, so an unknown kind is a newer
        // build's mapping rather than bytes that contradict themselves.
        let kind = StateKind::from_u16(kind_raw).ok_or(ReadError::Uninterpretable(
            SchemaError::UnknownField {
                event: IMPORT_BOOKKEEPING,
                field: t::EXPECTED_COUNTS,
            },
        ))?;
        if previous.is_some_and(|prev| kind_raw <= prev) {
            return Err(ReadError::Data(DecodeError::FieldOrder {
                tag: t::EXPECTED_COUNTS,
            }));
        }
        previous = Some(kind_raw);
        let count = reader.u64()?;
        pairs.push(CountPair { kind, count });
    }
    if !reader.is_empty() {
        return Err(ReadError::Data(DecodeError::Trailing {
            remaining: reader.remaining(),
        }));
    }
    ledger.checkpoint()?;
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Fold helpers
// ---------------------------------------------------------------------------

/// The state kind a natural-key family folds into, for callers assembling a record set.
///
/// Kept here rather than inferred at each call site: the mapping is part of what these
/// events *mean*, and `storage-digests.md` §4.4.8 is its only authority.
#[must_use]
pub fn state_kind(event_tag: u16) -> Option<StateKind> {
    match event_tag {
        ASSET_DECLARED => Some(StateKind::Asset),
        KB_METADATA_UPDATED => Some(StateKind::KbMetadata),
        MODEL_REGISTERED | MODEL_UPDATED | MODEL_RETIRED => Some(StateKind::Model),
        RULE_PROFILE_CHANGED => Some(StateKind::RuleConfig),
        // Neither an outcome nor an import session is a knowledge record: §4.4.8 notes 5
        // and 6 put them in `bookkeeping_digest` only, so they have no state kind.
        REQUEST_OUTCOME_RECORDED | IMPORT_BOOKKEEPING => None,
        _ => None,
    }
}

/// Whether a natural-key record's revision is fixed at 1.
///
/// §4.4.4: Asset is the one natural-key record whose revision is constant, because a
/// content address cannot be revised. Callers use this instead of hard-coding `1` next to
/// a `RecordSet` entry.
#[must_use]
pub fn is_immutable(kind: StateKind) -> bool {
    matches!(kind, StateKind::Asset)
}

/// The identified-record kind a counted row refers to, when it has one.
///
/// `expected_counts` covers state kinds 1..=13, which is wider than [`RecordKind`]'s
/// 1..=9; this function is the single place that difference is expressed.
#[must_use]
pub fn counted_record_kind(kind: StateKind) -> Option<RecordKind> {
    kind.record_kind()
}
