//! Canonical state-body projection: one decoded event to 0, 1 or 2 [`StateEntry`] values.
//!
//! [`crate::state`] consumes a `RecordSet` of [`StateEntry`] but nothing produced one from an
//! event; this module is that step. Format owns the **bytes** of a body; the store owns the
//! keyed fold and the ordering, so this appends in the caller's order and never sorts.
//!
//! ## What each event contributes (`storage-digests` §4.4.3 / §4.4.6)
//!
//! | event | entries |
//! |---|---|
//! | `192 RequestOutcomeRecorded`, `208 ImportBookkeeping`, `176 IdAllocation` | **0** — they are `bookkeeping_digest` inputs, not knowledge records |
//! | an ordinary identified record | **1** |
//! | an ordinary natural-key record | **1**, key from the natural form |
//! | `224 Genesis` | **2** — KbMetadata (kind 12) and RuleConfig (kind 13) |
//!
//! The body is the event's fields **minus exactly tags 1 and 13**, ascending: tag 1 is already
//! `Entry.kind` + `Entry.key`, and tag 13 (`changed_fields`) is reconstructible from adjacent
//! after-images. Everything else is retained — including `4 status`, `10/11/12 legacy_*` and
//! `14 extensions` — because those cannot be reconstructed.
//!
//! Genesis is the one event that yields two records, so its fields are *partitioned*, and
//! §4.4.6 deliberately shares a few: `2 revision`, `8 created_at`, `7 reason` and `14
//! extensions` enter both bodies with byte-identical values. `163 rules_digest` enters **only**
//! RuleConfig: putting it in KbMetadata too would make one fact appear in the digest twice and
//! couple "metadata changed" to "rules changed". [`project_event`] asserts the shared-tag
//! equality rather than assuming it, because that is the invariant easiest to break silently.
//!
//! ## Validation is not re-implemented here
//!
//! [`project_event`] calls `encode::validate_event_through_dispatch` **first** and
//! refuses on anything that is not `Ok`. That helper routes through the real family readers
//! inside `read_dispatched`, so this module never re-states a permitted/required/forbidden
//! field policy and never trusts its caller's claim that an event was validated.
//!
//! ## Resources
//!
//! Admitted, each before the work it covers:
//!
//! * one entry per appended [`StateEntry`] (`try_charge_entries`);
//! * the exact key bytes and the exact body bytes, as a single sum per entry, before any
//!   fallible copy.
//!
//! Nothing here can fail for a reason that is not already typed: the copies use `try_reserve`
//! and `extend_from_slice` on buffers whose extent was admitted first, so an allocator refusal
//! surfaces as [`Refusal::Allocation`] rather than a panic.
//!
//! There is no `Box`, no `BTreeMap`, no infallible clone, and no reservation after a charge
//! that could be mistaken for admission.

use sage_db_types::framing::{BlobRefSink, FormatError, FormatOutcome, Refusal};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::StateEntry;
use sage_db_types::query::ControlStop;

use crate::event::{EVENT_FLAG_REQUIRED, Field, write_event};
use crate::wire::Writer;

/// `record_key`; already `Entry.kind` + `Entry.key`.
const TAG_RECORD_KEY: u16 = 1;
/// `revision`, carried into `Entry.revision` and retained in the body.
const TAG_REVISION: u16 = 2;
/// `changed_fields`, reconstructible from adjacent after-images.
const TAG_CHANGED_FIELDS: u16 = 13;
/// `extensions`, shared by both Genesis bodies when present.
const TAG_EXTENSIONS: u16 = 14;
/// `created_at`, shared by both Genesis bodies.
const TAG_CREATED_AT: u16 = 8;
/// `reason`, shared by both Genesis bodies when present.
const TAG_REASON: u16 = 7;

/// State kinds this projection can emit (§4.4.1). `10..=13` have no numeric record identity.
const KIND_ASSET: u16 = 10;
const KIND_MODEL: u16 = 11;
const KIND_KB_METADATA: u16 = 12;
const KIND_RULE_CONFIG: u16 = 13;

/// `storage-digests` §4.4.6: the tags that enter the **KbMetadata** body of a Genesis event.
const GENESIS_KB_TAGS: &[u16] = &[2, 7, 8, 14, 144, 145, 146, 147, 148];
/// §4.4.6: the tags that enter the **RuleConfig** body. `163` is here and only here.
const GENESIS_RULES_TAGS: &[u16] = &[2, 7, 8, 14, 160, 161, 162, 163, 166, 196];
/// §4.4.6: tags deliberately present in **both** Genesis bodies, byte-identical.
const GENESIS_SHARED_TAGS: &[u16] = &[TAG_REVISION, TAG_REASON, TAG_CREATED_AT, TAG_EXTENSIONS];

/// Why an event could not be projected.
///
/// The refusal arms are not verdicts about the bytes: a budget refusal means "retry with more
/// allowance", a stop means "the caller said stop". They are never reported as corruption, which
/// is why they are a separate enum rather than variants of `FormatError`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProjectionError {
    /// This attempt's ledger refused an admission.
    Budget(sage_db_types::ledger::BudgetError),
    /// The caller's policy asked to stop.
    Stopped(ControlStop),
    /// The allocator refused a buffer whose extent was already admitted.
    Allocation {
        /// Bytes that were needed.
        requested: usize,
    },
    /// The event's field stream is not a well-formed field sequence (order, duplicates, empty
    /// value), or the body could not be re-encoded. A defect in the *input fields*, not a
    /// verdict about a file.
    MalformedFields {
        /// The event tag.
        tag: u16,
    },
    /// The record has no canonical state kind, so it cannot be folded.
    UnprojectableKind {
        /// The event tag.
        tag: u16,
    },
}

impl From<sage_db_types::ledger::BudgetError> for ProjectionError {
    fn from(error: sage_db_types::ledger::BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ControlStop> for ProjectionError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// Which state kind an event's record folds into, and whether it emits one entry or two.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Emission {
    /// Bookkeeping only: contributes no state entry.
    None,
    /// One entry of this state kind.
    One(u16),
    /// The two Genesis records.
    Genesis,
}

/// Classify an event tag by what it contributes to the folded state set.
///
/// Kept as a table rather than inferred, because "which events are knowledge records" is a
/// decision of §4.4.8 and not something the field stream can tell us.
const fn emission_of(tag: u16) -> Emission {
    match tag {
        // Bookkeeping families: their records live in `bookkeeping_digest`, not `state_digest`.
        176 | 192 | 208 => Emission::None,
        // Genesis establishes both singletons.
        224 => Emission::Genesis,
        // Identified records: state kind == `RecordKind` numerically (§4.4.1 note).
        1..=3 => Emission::One(1),   // TermCreated / Revised / Retired
        16..=18 => Emission::One(2), // Assertion*
        32..=35 => Emission::One(3), // Claim*
        48..=49 => Emission::One(4), // Evidence*
        50..=51 => Emission::One(5), // EvidenceLink*
        80..=81 => Emission::One(6), // Vector*
        96..=97 => Emission::One(7), // Proposal*
        112 => Emission::One(8),     // FrontierEntry
        128 => Emission::One(9),     // SessionCheckpoint
        // Natural-key records.
        64 => Emission::One(KIND_ASSET),
        145..=147 => Emission::One(KIND_MODEL),
        144 => Emission::One(KIND_KB_METADATA),
        160 => Emission::One(KIND_RULE_CONFIG),
        // Unregistered in this major; the validator refuses it before we classify.
        _ => Emission::None,
    }
}

/// Project one validated event into `out`, appending 0, 1 or 2 entries.
///
/// `fields` must be the event's validated field stream in ascending tag order — the shape
/// [`crate::event::Event::fields`] returns. This function re-validates through
/// the crate-private `encode::validate_event_through_dispatch` before reading anything, so a caller
/// cannot smuggle in an unvalidated event.
///
/// Returns the number of entries appended (0 for bookkeeping events, 2 for Genesis).
///
/// # Errors
///
/// [`ProjectionError`]: a refusal (budget, stop, allocation), or a malformed field stream.
/// The event's own validity is reported as the `FormatOutcome::Format` the validator produced,
/// unchanged, so an unsupported representation stays `Unsupported` and damage stays `Corrupt`.
pub fn project_event(
    tag: u16,
    fields: &[Field<'_>],
    out: &mut Vec<StateEntry>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<usize> {
    // Validation first, through the REAL family readers. Anything else would be a second
    // opinion by omission, which is exactly what the seam exists to prevent. All three
    // outcomes are matched: letting a refusal fall through would project an event the real
    // validator declined to accept.
    match crate::encode::validate_event_through_dispatch(tag, fields, ledger) {
        FormatOutcome::Ok(()) => {}
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    }

    let emission = emission_of(tag);
    match emission {
        Emission::None => FormatOutcome::Ok(0),
        Emission::One(kind) => match project_one(kind, tag, fields, out, ledger) {
            Ok(()) => FormatOutcome::Ok(1),
            Err(error) => projection_outcome(error),
        },
        Emission::Genesis => match project_genesis(tag, fields, out, ledger) {
            Ok(()) => FormatOutcome::Ok(2),
            Err(error) => projection_outcome(error),
        },
    }
}

/// Map a projection failure onto the boundary: refusals stay refusals.
fn projection_outcome<T>(error: ProjectionError) -> FormatOutcome<T> {
    match error {
        ProjectionError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        ProjectionError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
        ProjectionError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
        // The remaining arms describe the caller's input, not the bytes on disk. Reported as
        // corruption would tell an operator to restore a backup over a caller bug, so they are
        // internals: this build could not do what it promised with correct input.
        ProjectionError::MalformedFields { .. } | ProjectionError::UnprojectableKind { .. } => {
            FormatOutcome::Format(FormatError::Internal(
                sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
            ))
        }
    }
}

/// Project one ordinary record event into a single entry.
fn project_one(
    kind: u16,
    tag: u16,
    fields: &[Field<'_>],
    out: &mut Vec<StateEntry>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), ProjectionError> {
    let revision = entry_revision(kind, tag, fields)?;
    let key = natural_key(kind, tag, fields)?;
    let body = canonical_body(tag, fields, None, ledger)?;
    push_entry(kind, key, revision, body, out, ledger)
}

/// Project a Genesis event into its two singleton records (§4.4.6).
fn project_genesis(
    tag: u16,
    fields: &[Field<'_>],
    out: &mut Vec<StateEntry>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), ProjectionError> {
    let revision = match find(fields, TAG_REVISION) {
        Some(raw) if raw.len() == 8 => u64::from_le_bytes(
            raw.try_into()
                .map_err(|_| ProjectionError::MalformedFields { tag })?,
        ),
        _ => return Err(ProjectionError::MalformedFields { tag }),
    };

    let kb_body = canonical_body(tag, fields, Some(GENESIS_KB_TAGS), ledger)?;
    let rules_body = canonical_body(tag, fields, Some(GENESIS_RULES_TAGS), ledger)?;

    // §4.4.6's shared tags must be byte-identical in the two bodies that were actually
    // PRODUCED. Comparing the input fields to themselves would prove nothing — only the
    // projection can break this invariant, so the assertion must read its output.
    for shared in GENESIS_SHARED_TAGS {
        if let (Some(kb), Some(rules)) = (
            body_field(&kb_body, *shared),
            body_field(&rules_body, *shared),
        ) {
            debug_assert_eq!(
                kb, rules,
                "shared Genesis tag must be byte-identical in both produced bodies"
            );
        }
    }
    push_entry(KIND_KB_METADATA, &[], revision, kb_body, out, ledger)?;
    push_entry(KIND_RULE_CONFIG, &[], revision, rules_body, out, ledger)
}

/// The `Entry.revision` for a record (§4.4.4).
///
/// Every kind but Asset takes the event's `2 revision`. Asset is content-addressed and
/// immutable, so its revision is the explicit constant `1` — not a fabricated number, but a
/// statement that it exists and cannot change.
fn entry_revision(kind: u16, tag: u16, fields: &[Field<'_>]) -> Result<u64, ProjectionError> {
    if kind == KIND_ASSET {
        return Ok(1);
    }
    match find(fields, TAG_REVISION) {
        Some(raw) if raw.len() == 8 => Ok(u64::from_le_bytes(
            raw.try_into()
                .map_err(|_| ProjectionError::MalformedFields { tag })?,
        )),
        _ => Err(ProjectionError::MalformedFields { tag }),
    }
}

/// The `Entry.key` for a record (§4.4.2).
///
/// Numeric kinds carry their id little-endian; Term is `u32` and the rest `u128`, which is why
/// the width is read from the kind rather than assumed. Asset's key is its raw 32-byte content
/// address, Model's is the UTF-8 `model_key`, and the singletons are empty.
/// The `Entry.key` bytes for a record (§4.4.2), **borrowed** from the caller's field stream.
///
/// Returning an owned `Vec` here would allocate before any admission, which is the ordering
/// defect this module is explicitly required to avoid. [`push_entry`] admits the extent and
/// reserves exactly, then copies.
///
/// Numeric kinds carry their id little-endian; Term is `u32` and the rest `u128`, so the width
/// comes from the kind rather than from an assumption. Asset's key is its raw 32-byte content
/// address, Model's is the UTF-8 `model_key`, and the singletons have an empty key.
fn natural_key<'a>(
    kind: u16,
    tag: u16,
    fields: &'a [Field<'a>],
) -> Result<&'a [u8], ProjectionError> {
    match kind {
        KIND_KB_METADATA | KIND_RULE_CONFIG => Ok(&[]),
        KIND_ASSET => {
            // `84 sha256` IS the key; it is not a blob reference, which is why
            // `is_blob_ref_tag` excludes it.
            match find(fields, 84) {
                Some(raw) if raw.len() == 32 => Ok(raw),
                _ => Err(ProjectionError::MalformedFields { tag }),
            }
        }
        KIND_MODEL => match find(fields, 149) {
            Some(raw) if raw.len() >= 4 => {
                let len = u32::from_le_bytes(
                    raw[..4]
                        .try_into()
                        .map_err(|_| ProjectionError::MalformedFields { tag })?,
                );
                let end = 4usize
                    .checked_add(len as usize)
                    .ok_or(ProjectionError::MalformedFields { tag })?;
                raw.get(4..end)
                    .ok_or(ProjectionError::MalformedFields { tag })
            }
            _ => Err(ProjectionError::MalformedFields { tag }),
        },
        // Identified records: `record_key` is `kind:u16 | id:u128`, and the entry key is the id
        // alone at the width that kind uses.
        1..=9 => {
            let raw =
                find(fields, TAG_RECORD_KEY).ok_or(ProjectionError::MalformedFields { tag })?;
            if raw.len() != 18 {
                return Err(ProjectionError::MalformedFields { tag });
            }
            let id = &raw[2..18];
            if kind == 1 {
                // Term ids are `u32`; the `u128` LE encoding carries the same value in its
                // first word.
                Ok(&id[..4])
            } else {
                Ok(id)
            }
        }
        _ => Err(ProjectionError::MalformedFields { tag }),
    }
}

/// Re-encode the canonical body: the event's fields minus tags 1 and 13 (or, for Genesis,
/// restricted to the section's tag set), ascending, using the **real** event writer.
///
/// No envelope is hand-rolled: [`write_event`] is the only writer, so the body bytes are
/// produced by the same code that produces every other event.
fn canonical_body(
    tag: u16,
    fields: &[Field<'_>],
    allowed: Option<&[u16]>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ProjectionError> {
    let keep = |field_tag: u16| -> bool {
        match allowed {
            Some(allow) => allow.contains(&field_tag),
            None => field_tag != TAG_RECORD_KEY && field_tag != TAG_CHANGED_FIELDS,
        }
    };

    let mut kept: Vec<Field<'_>> = Vec::new();
    let count = fields.iter().filter(|f| keep(f.tag())).count();
    ledger.try_charge_entries(count as u64)?;
    let slot_bytes = (count as u64).saturating_mul(core::mem::size_of::<Field<'static>>() as u64);
    ledger.try_charge(slot_bytes)?;
    ledger.checkpoint()?;
    kept.try_reserve_exact(count)
        .map_err(|_| ProjectionError::Allocation {
            requested: slot_bytes as usize,
        })?;
    for field in fields {
        if keep(field.tag()) {
            kept.push(*field);
        }
    }

    // The body is the field sequence without the event header, so it is written with the same
    // envelope writer and then the 6-byte event header is skipped. Measuring first would mean
    // a second pass over the fields for no benefit; the buffer is admitted by the writer.
    let mut scratch: Vec<u8> = Vec::new();
    let mut writer = Writer::new(&mut scratch, ledger);
    if let Err(error) = write_event(&mut writer, tag, EVENT_FLAG_REQUIRED, &kept) {
        return Err(write_failure(error));
    }
    // `write_event` emits `tag:u16 | flags:u16 | count:u16` before the fields.
    const EVENT_HEADER: usize = 6;
    if scratch.len() < EVENT_HEADER {
        return Err(ProjectionError::MalformedFields { tag });
    }
    // Remove the prefix IN PLACE: `drain` shifts the tail down and truncates the same buffer,
    // so no second allocation happens and nothing needs a second admission. `split_off` would
    // allocate a new `Vec` of the body and leave the header bytes behind, which is the
    // allocation the resource rule forbids.
    scratch.drain(..EVENT_HEADER);
    Ok(scratch)
}

/// Map a writer failure: refusals keep their identity, data faults are the caller's input.
fn write_failure(error: crate::wire::WriteError) -> ProjectionError {
    match error {
        crate::wire::WriteError::Budget(budget) => ProjectionError::Budget(budget),
        crate::wire::WriteError::Stopped(stop) => ProjectionError::Stopped(stop),
        crate::wire::WriteError::Allocation { requested } => {
            ProjectionError::Allocation { requested }
        }
        crate::wire::WriteError::Data(_)
        | crate::wire::WriteError::MissingRequiredField { .. }
        | crate::wire::WriteError::Internal { .. } => ProjectionError::MalformedFields { tag: 0 },
    }
}

/// Append one entry, admitting each growth before the reservation that performs it.
///
/// Three distinct extents, and conflating them is what produced the first version's false claim:
///
/// * the **entry slot** in `out` — admitted for the capacity the reservation will actually
///   require, not a single hard-coded slot. A vector that must grow allocates several slots at
///   once, and charging one would understate the retained table. Capacity the caller's buffer
///   already has is treated as already-admitted credit: only the delta is charged, so a reused
///   buffer is not billed twice for the same slots.
/// * the **key** bytes — the key arrives as a borrowed slice, so its extent is admitted and then
///   reserved exactly, and only then copied.
/// * the **body** is NOT charged here: [`Writer`] already admitted every capacity growth of that
///   buffer on this same ledger while building it, and the `Vec` is moved into the entry rather
///   than copied. Charging body bytes again would count one allocation twice.
fn push_entry(
    kind: u16,
    key: &[u8],
    revision: u64,
    body: Vec<u8>,
    out: &mut Vec<StateEntry>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), ProjectionError> {
    let slot_bytes = core::mem::size_of::<StateEntry>() as u64;

    // The capacity this reservation will require, and only the part not already held.
    // ENTRIES and BYTES are different units and must not be substituted for each other:
    //
    // * the appended RECORD is one entry of work, always — a caller whose buffer already has
    //   spare capacity still pays for the record being appended. Prior capacity is a BYTE
    //   credit; it does not prepay the work of a new entry.
    // * the SLOT BYTES are only the capacity this reservation actually adds. Charging the full
    //   slot again when the buffer already holds it would bill memory that is not being grown.
    ledger.try_charge_entries(1)?;
    let needed = out
        .len()
        .checked_add(1)
        .ok_or(ProjectionError::MalformedFields { tag: 0 })?;
    let added_slots = (needed.saturating_sub(out.capacity())) as u64;
    if added_slots > 0 {
        ledger.try_charge(added_slots.saturating_mul(slot_bytes))?;
    }
    ledger.try_charge(key.len() as u64)?;
    ledger.checkpoint()?;

    // `try_reserve_exact` reserves the minimum rather than a geometric overshoot, so the
    // admitted delta is the delta that actually happens.
    out.try_reserve_exact(1)
        .map_err(|_| ProjectionError::Allocation {
            requested: (added_slots as usize).saturating_mul(core::mem::size_of::<StateEntry>()),
        })?;
    let mut owned_key: Vec<u8> = Vec::new();
    owned_key
        .try_reserve_exact(key.len())
        .map_err(|_| ProjectionError::Allocation {
            requested: key.len(),
        })?;
    owned_key.extend_from_slice(key);

    out.push(StateEntry {
        kind,
        key: owned_key,
        revision,
        body,
    });
    Ok(())
}

/// Read one field's value out of an encoded body, for the shared-tag comparison.
fn body_field(body: &[u8], want: u16) -> Option<&[u8]> {
    let mut at = 0usize;
    while at + 8 <= body.len() {
        let tag = u16::from_le_bytes([body[at], body[at + 1]]);
        let len = u32::from_le_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]);
        let start = at + 8;
        let end = start.checked_add(len as usize)?;
        if end > body.len() {
            return None;
        }
        if tag == want {
            return Some(&body[start..end]);
        }
        at = end;
    }
    None
}

/// The raw value of `tag`, when present.
fn find<'a>(fields: &'a [Field<'a>], tag: u16) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|field| field.tag() == tag)
        .map(|field| field.value())
}

/// Enumerate an event's blob references from a **raw field slice**, reusing
/// [`crate::dispatch::is_blob_ref_tag`].
///
/// [`crate::dispatch::for_each_blob_ref`] is the authority on *which* tags name a content
/// address, but it takes `&Event`, and an `Event` can only come from
/// [`crate::event::read_event`] — there is no constructor from a field slice. A consumer holding
/// fields (this module's own input shape) therefore could not reach that enumeration without
/// re-parsing bytes or hand-writing the tag list, and both are the duplication the seam exists to
/// prevent. This forwards the same predicate over the borrowed slice instead.
///
/// The tag set is **not** duplicated: this calls [`crate::dispatch::is_blob_ref_tag`], so a tag
/// added there is picked up here with no second table to keep in step. In particular `84 sha256`
/// stays excluded (it is the Asset's natural key) and `184 source_digest` stays excluded (it is a
/// digest of source material, not an address to fetch).
///
/// ## Borrowed input, nothing to admit
///
/// `fields` is borrowed for the whole call. Each reference handed to the sink is a 32-byte copy of
/// a value already inside that slice; no intermediate table is built, so there is no allocation
/// here to admit. The sink receives the caller's ledger and decides whether *its* storage may
/// grow — moving the retaining allocation to the sink does not remove that obligation from it.
///
/// ## Already-validated input
///
/// This performs no validation and does not need to: it is documented to be called on fields the
/// real validator accepted — the state after [`project_event`] returned `Ok`. A ref tag whose
/// value is not 32 bytes is skipped rather than reported, matching the `Event`-taking entry point:
/// the family decoder is what reports a wrong-width field, and enumeration must not become a
/// second verdict.
///
/// It enumerates **canonical format references only**. It does not decide which blobs a commit
/// must publish, does not accumulate history, and does not attach lengths — those are the store's
/// declared-superset and fold-union roles.
///
/// # Errors
///
/// [`Refusal`] when the sink's ledger refuses an admission.
pub fn for_each_blob_ref_fields(
    fields: &[Field<'_>],
    sink: &mut dyn BlobRefSink,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Refusal> {
    // Delegates to the one canonical enumerator: the tag predicate and the cancellation
    // boundary live there, not in a second copy here.
    crate::dispatch::for_each_blob_ref_fields(fields, sink, ledger)
}
