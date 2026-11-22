//! Transaction and manifest **encoders** (`encode_tx`, `encode_manifest`) plus the shared `SGLB`
//! envelope writer.
//!
//! Pure: no filesystem, clock, environment or `Store` access. Publishing a file — staging, `rename`,
//! directory sync, `CURRENT` update — is commit orchestration (P1-4) and is not here.
//!
//! # Resource discipline
//!
//! Every byte this module retains is admitted to the caller's ledger **before** it is allocated, and
//! the number of retained slots is admitted before the table holding them is allocated. The previous
//! revision allocated field buffers and field tables with `Vec::with_capacity` *before* the first
//! `Writer` admission, which made a zero-budget test pass while allocations had already happened; that
//! is a defect no amount of refusal testing at the `Writer` boundary could detect.
//!
//! Concretely:
//!
//! * variable-length header values are **not** copied into temporary buffers at all — they are written
//!   straight through [`Writer`], which admits every append;
//! * field tables are reserved fallibly **after** their slot count is admitted;
//! * nothing in this module calls `Vec::with_capacity` on a length derived from caller input.
//!
//! # Block ceilings
//!
//! The envelope writer enforces `max_block_raw_bytes` / `max_block_encoded_bytes` on the block it emits,
//! and splits the body into several legal codec-none blocks when one block would exceed the ceiling.
//! It never infers block-limit coverage from the file limit: a body that fits the file ceiling can still
//! be an illegal single block, and emitting it would produce a file this build's own reader rejects with
//! `Limit`.
//!
//! # What a sink does and does not guarantee
//!
//! A [`BytesSink`] receives the complete image in one `write` call. That is a *passing* guarantee, not a
//! durability one: it does not make the write atomic on disk, and a sink is free to write some bytes and
//! then fail. Durability and torn-write recovery belong to the store's commit protocol.

use sage_db_types::framing::{
    BlobRefSink, BytesSink, CorruptReason, FormatError, FormatOutcome, Limits, OperatorAction,
    Refusal, UnsupportedReason, classify,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::Extensions;

use crate::dispatch::from_write_error;
use crate::event::{Event, Field, write_event, write_header};
use crate::file::{
    BLOCK_FLAG_REQUIRED, BLOCK_HEADER_LEN, BlockHeader, CODEC_NONE, FILE_HEADER_LEN, FileHeader,
    HEADER_CRC_OFFSET, Kind, Layout, TRAILER_LEN,
};
use crate::wire::{WriteError, Writer};

// ---------------------------------------------------------------------------
// Writer input types (file-specific; identities and errors are canonical)
// ---------------------------------------------------------------------------

/// Request-canonicalisation version of a transaction header.
///
/// **Re-exported from `sage_db_types::prepared`**: there is exactly one declaration of this
/// closed enum, so a carrier and an encoder cannot hold two `Protocol` enums that must be
/// kept in agreement. `NonRequest` is only legal for genesis, so the pairing with the
/// payload-digest sentinel is a type-and-value rule rather than a convention.
pub use sage_db_types::prepared::RequestCanonVersion;

/// Everything `encode_tx` needs in its header.
///
/// `prev_version` is `Option` because a version has no legal zero: absent means genesis, and the wire
/// value 0 is derived from that rather than from a sentinel a caller could set inconsistently.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TxHeaderToEncode {
    /// KB identity; must match the chain.
    pub kb_uuid: KbUuid,
    /// This transaction's version; must be `prev_version + 1`.
    pub version: u64,
    /// 16-byte transaction id; must equal the `<tid>` in the file name.
    pub tx_id: [u8; 16],
    /// `None` exactly for genesis.
    pub prev_version: Option<u64>,
    /// Commit time written by the committer, UTC microseconds.
    pub created_at: i64,
    /// Executor; genesis uses `system`.
    pub principal_id: String,
    /// Caller request id; genesis uses `genesis`.
    pub request_id: String,
    /// Caller payload digest; the all-zero value is the genesis sentinel.
    pub payload_digest: [u8; 32],
    /// Rules profile this transaction was evaluated with.
    pub rules_profile: u16,
    /// Digest of the rules bytes.
    pub rules_digest: [u8; 32],
    /// Canonicalisation version; must agree with the digest sentinel.
    pub request_canon_version: RequestCanonVersion,
    /// When present, must equal `prev_version`.
    pub expect_version: Option<u64>,
    /// Advisory annotations (§8.3).
    pub extensions: Extensions,
}

/// One event handed to the encoder.
///
/// Construction is fallible and applies the same checks the decoder applies, so an encoder cannot emit
/// an event its own reader would call `Corrupt`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EventToEncode<'a> {
    /// Event tag, registered in this major.
    pub tag: u16,
    /// Fields in strictly ascending tag order.
    pub fields: Vec<Field<'a>>,
}

/// Everything `encode_tx` needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TxToEncode<'a> {
    /// Header fields.
    pub header: TxHeaderToEncode,
    /// Events in commit order; `event_count` is derived from this, never supplied separately.
    pub events: Vec<EventToEncode<'a>>,
}

/// Everything `encode_manifest` needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecodedManifest {
    /// KB identity; must match the referenced transaction.
    pub kb_uuid: KbUuid,
    /// Version; must match the referenced transaction.
    pub version: u64,
    /// Commit time; must match the referenced transaction.
    pub created_at: i64,
    /// Rules profile; must match the referenced transaction.
    pub rules_profile: u16,
    /// Rules digest; must match the referenced transaction.
    pub rules_digest: [u8; 32],
    /// Knowledge-state digest after this version.
    pub state_digest: [u8; 32],
    /// Active-graph digest after this version.
    pub graph_digest: [u8; 32],
    /// Digest covering the previous chain digest and this transaction.
    pub chain_digest: [u8; 32],
    /// RequestOutcome / ImportSession / high-water-mark digest.
    pub bookkeeping_digest: [u8; 32],
    /// Previous manifest name; absent exactly for genesis.
    pub prev_manifest_name: Option<String>,
    /// Previous manifest file digest; present iff `prev_manifest_name` is.
    pub prev_manifest_sha256: Option<[u8; 32]>,
    /// `tx-<tid>.sgl` of the transaction this manifest commits.
    pub tx_file_name: String,
    /// Whole-file SHA-256 of that transaction.
    pub tx_file_sha256: [u8; 32],
}

// ---------------------------------------------------------------------------
// Family gate: never emit a family whose semantics this build cannot check
// ---------------------------------------------------------------------------

/// Validate an event by feeding the bytes this encoder would write through the real dispatch seam.
///
/// This is the family validation bridge on the write side, and it is deliberately *not* a list of tags
/// this module believes are supported: `read_dispatched` applies the registry, the family ownership map
/// and the family decoder itself (`term::read_term`, `assertion::read_assertion`, …). A hard-coded
/// allow-list here would be a second authority on which families exist, and would silently rot — exactly
/// how the previous revision came to emit a registered but semantically unvalidated `Term`.
///
/// The cost is one extra parse per event, on bytes the encoder already holds. The benefit is that a
/// writer can never produce a record whose family rules nothing checked.
///
/// # Errors
///
/// Whatever `read_dispatched` reports, unchanged: `UnsupportedFormat(NoCodec)` for an unlanded family,
/// `Corrupt`/`UnsupportedFormat` for a field-stream violation, `Refused` for ledger refusals.
pub(crate) fn validate_event_through_dispatch(
    tag: u16,
    fields: &[Field<'_>],
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    // One policy: this is the same typed core the public decode helper uses. Validating and
    // decoding are not two opinions about the same bytes, they are the same call with the
    // payload dropped.
    match decode_fields_typed(tag, fields, ledger) {
        FormatOutcome::Ok(_) => FormatOutcome::Ok(()),
        FormatOutcome::Format(error) => FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
    }
}

/// Encode one event from a field slice, then decode it through the family readers.
///
/// The single typed core behind both the validator and the public owned-event decoder: the
/// bytes are produced by the real [`crate::event::write_event`] and consumed by the real
/// [`crate::dispatch::read_dispatched`], so a caller cannot obtain a payload that a fresh
/// decode of the same event would reject.
pub(crate) fn decode_fields_typed(
    tag: u16,
    fields: &[Field<'_>],
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<crate::dispatch::DispatchedPayload> {
    let mut scratch: Vec<u8> = Vec::new();
    {
        let mut writer = Writer::new(&mut scratch, ledger);
        if let Err(error) = crate::event::write_event(&mut writer, tag, 0b0001, fields) {
            return write_error_outcome(error);
        }
    }
    let mut reader = crate::wire::Reader::new(&scratch);
    match crate::dispatch::read_dispatched(&mut reader, ledger) {
        FormatOutcome::Ok(dispatched) => match dispatched {
            // Every one of the 17 registered families yields `Record`; `Validated` is never
            // constructed by this path. Reaching the other arm would mean a family was added
            // without a decode arm, which is this build's defect - reported as Internal, never
            // dressed up as an allocation refusal.
            crate::dispatch::DispatchedEvent::Record { payload, .. } => FormatOutcome::Ok(payload),
            crate::dispatch::DispatchedEvent::Validated { .. } => {
                FormatOutcome::Format(FormatError::Internal(
                    sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
                ))
            }
        },
        FormatOutcome::Format(error) => FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
    }
}

// ---------------------------------------------------------------------------
// Header consistency
// ---------------------------------------------------------------------------

/// Check a transaction header's internal consistency.
fn check_tx_header(h: &TxHeaderToEncode, event_count: u32, limits: &Limits) -> FormatOutcome<()> {
    if event_count == 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ZeroEvents));
    }
    if event_count > limits.max_events_per_tx {
        return FormatOutcome::Format(FormatError::Limit(
            sage_db_types::framing::LimitKind::EventsPerTx,
        ));
    }

    // Genesis is version 1 with no predecessor; every later version must name one.
    match h.prev_version {
        None => {
            if h.version != 1 {
                return FormatOutcome::Format(FormatError::Corrupt(
                    CorruptReason::RevisionNotMonotonic {
                        expected: 1,
                        found: h.version,
                    },
                ));
            }
        }
        Some(prev) => {
            if h.version != prev.saturating_add(1) {
                return FormatOutcome::Format(FormatError::Corrupt(
                    CorruptReason::RevisionNotMonotonic {
                        expected: prev.saturating_add(1),
                        found: h.version,
                    },
                ));
            }
        }
    }

    if h.expect_version.is_some_and(|e| Some(e) != h.prev_version) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::OutcomeHeaderMismatch {
            tag: crate::transaction::TAG_EXPECT_VERSION,
        }));
    }

    // §4.2: `NonRequest` pairs with the all-zero digest, `V1` with a real one.
    let sentinel = h.payload_digest == [0u8; 32];
    match h.request_canon_version {
        RequestCanonVersion::NonRequest if !sentinel => {
            return FormatOutcome::Format(FormatError::Corrupt(
                CorruptReason::RequestCanonMismatch,
            ));
        }
        RequestCanonVersion::V1 if sentinel => {
            return FormatOutcome::Format(FormatError::Corrupt(
                CorruptReason::RequestCanonMismatch,
            ));
        }
        _ => {}
    }

    // The string ceilings are checked before any byte is written, so a refusal costs nothing.
    for (tag, text) in [
        (crate::transaction::TAG_PRINCIPAL_ID, &h.principal_id),
        (crate::transaction::TAG_REQUEST_ID, &h.request_id),
    ] {
        if text.len() as u64 > u64::from(limits.max_string_bytes) {
            return FormatOutcome::Format(FormatError::Limit(
                sage_db_types::framing::LimitKind::String {
                    tag: Some(tag),
                    len: text.len() as u32,
                    max: limits.max_string_bytes,
                },
            ));
        }
    }
    FormatOutcome::Ok(())
}

/// Check one event is legal *and semantically validatable* before it is written.
fn check_event(event: &EventToEncode<'_>, limits: &Limits) -> FormatOutcome<()> {
    if event.fields.is_empty() {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    if event.fields.len() > limits.max_event_fields as usize {
        return FormatOutcome::Format(FormatError::Limit(
            sage_db_types::framing::LimitKind::EventFields,
        ));
    }
    let mut previous: Option<u16> = None;
    for field in &event.fields {
        if previous.is_some_and(|prev| field.tag() <= prev) {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldOrder));
        }
        previous = Some(field.tag());
        if field.value().is_empty() {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ZeroLengthField {
                tag: field.tag(),
            }));
        }
        if field.value().len() as u64 > u64::from(limits.max_string_bytes) {
            return FormatOutcome::Format(FormatError::Limit(
                sage_db_types::framing::LimitKind::String {
                    tag: Some(field.tag()),
                    len: field.value().len() as u32,
                    max: limits.max_string_bytes,
                },
            ));
        }
        if !crate::registry::is_registered_field(field.tag()) {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::FieldTag(field.tag()),
            ));
        }
    }
    if !crate::registry::EVENTS
        .iter()
        .any(|spec| spec.tag == event.tag)
    {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::EventTag(
            event.tag,
        )));
    }
    FormatOutcome::Ok(())
}

// ---------------------------------------------------------------------------
// Block layout
// ---------------------------------------------------------------------------

/// One codec-none block to emit.
#[derive(Clone, Copy, Debug)]
struct BlockPlan {
    /// Offset into the body.
    offset: usize,
    /// Length of this block's payload.
    len: usize,
}

/// Split a body into blocks that each satisfy the block ceilings.
///
/// A single block is preferred: `storage-format` §2.2 allows `1..N` blocks, and one block means the
/// reader can borrow the payload without joining. Only when one block would exceed a ceiling is the
/// body split, and each piece is sized to the effective maximum.
fn plan_blocks(body_len: usize, limits: &Limits) -> FormatOutcome<Vec<BlockPlan>> {
    let raw_max = limits
        .max_block_raw_bytes
        .min(limits.max_block_encoded_bytes) as usize;
    if raw_max == 0 {
        // A zero block ceiling cannot hold even an empty body's block; that is a configuration this
        // encoder cannot satisfy, and saying so is better than emitting an illegal block.
        return FormatOutcome::Format(FormatError::Limit(
            sage_db_types::framing::LimitKind::BlockRaw,
        ));
    }
    if body_len <= raw_max {
        return FormatOutcome::Ok(vec![BlockPlan {
            offset: 0,
            len: body_len,
        }]);
    }

    let count = body_len.div_ceil(raw_max);
    let mut plans = Vec::new();
    if plans.try_reserve_exact(count).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation {
            requested: count.saturating_mul(core::mem::size_of::<BlockPlan>()),
        });
    }
    let mut offset = 0usize;
    while offset < body_len {
        let len = raw_max.min(body_len - offset);
        plans.push(BlockPlan { offset, len });
        offset += len;
    }
    FormatOutcome::Ok(plans)
}

/// Encode a complete `SGLB` file around `body`, split into ceiling-legal blocks.
///
/// Block ceilings are enforced **here**, from `max_block_raw_bytes`/`max_block_encoded_bytes`, and are
/// never inferred from the file ceiling.
fn write_sglb(
    out: &mut Vec<u8>,
    kind: Kind,
    body: &[u8],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    let plans = match plan_blocks(body.len(), limits).into_result() {
        Ok(plans) => plans,
        Err(other) => {
            return propagate(other);
        }
    };

    let blocks_len: usize = plans
        .iter()
        .map(|p| BLOCK_HEADER_LEN + p.len)
        .sum::<usize>();
    let payload_len = (blocks_len + TRAILER_LEN) as u64;
    let total = FILE_HEADER_LEN + payload_len as usize;

    // File ceiling first: cheap, and an oversized result must not allocate.
    let ceiling = match kind {
        Kind::Transaction => limits.max_tx_file_bytes,
        Kind::Manifest => limits.max_manifest_file_bytes,
        // A writer may never emit the reserved checkpoint kind (§2.5): this build's own reader must
        // report it as UNSUPPORTED_FORMAT, so producing one would be self-inflicted.
        Kind::ReservedCheckpoint => {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::FileKind(Kind::ReservedCheckpoint.code()),
            ));
        }
    };
    if total as u64 > ceiling {
        let limit = match kind {
            Kind::Transaction => sage_db_types::framing::LimitKind::TxFile,
            Kind::Manifest => sage_db_types::framing::LimitKind::ManifestFile,
            Kind::ReservedCheckpoint => sage_db_types::framing::LimitKind::ManifestFile,
        };
        return FormatOutcome::Format(FormatError::Limit(limit));
    }

    // Admission before allocation, for the image itself.
    if let Err(error) = ledger.try_charge(total as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    if out.try_reserve_exact(total).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation { requested: total });
    }

    let header = FileHeader {
        layout: Layout::Sglb,
        kind,
        payload_len,
        header_crc32c: 0,
    };
    // `crc_input` is the 32-byte header with the CRC field zeroed: the documented construction.
    let mut header_bytes = header.crc_input();
    let header_crc = crc32c::crc32c(&header_bytes);
    header_bytes[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4]
        .copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&header_bytes);

    for plan in &plans {
        let piece = &body[plan.offset..plan.offset + plan.len];
        let block = BlockHeader {
            kind: 1,
            flags: BLOCK_FLAG_REQUIRED | (CODEC_NONE << 1),
            encoded_len: u32::try_from(piece.len()).unwrap_or(u32::MAX),
            raw_len: u32::try_from(piece.len()).unwrap_or(u32::MAX),
            crc32c: crc32c::crc32c(piece),
        };
        if block.encoded_len as usize != piece.len() || block.raw_len as usize != piece.len() {
            // One block physically cannot exceed u32; refusing beats emitting a truncated length.
            return FormatOutcome::Format(FormatError::Limit(
                sage_db_types::framing::LimitKind::BlockEncoded,
            ));
        }
        out.extend_from_slice(&block.kind.to_le_bytes());
        out.extend_from_slice(&block.flags.to_le_bytes());
        out.extend_from_slice(&block.encoded_len.to_le_bytes());
        out.extend_from_slice(&block.raw_len.to_le_bytes());
        out.extend_from_slice(&block.crc32c.to_le_bytes());
        out.extend_from_slice(piece);
    }

    // §2.3: SHA-256 over the header (with its real CRC) and every block byte, excluding the trailer.
    let digest = crate::digest::raw_sha256(out);
    out.extend_from_slice(&digest);

    debug_assert_eq!(out.len(), total);
    FormatOutcome::Ok(())
}

// ---------------------------------------------------------------------------
// Admitted encoding helpers
// ---------------------------------------------------------------------------

/// Reserve a field table **after** admitting the slots it will retain.
///
/// The retained cost is `count * size_of::<Field>()`. Admitting that first is what makes a budget
/// refusal meaningful: an allocation that already happened cannot be undone by a later refusal.
/// Admit and reserve a field table, with an error type that cannot carry a format verdict.
///
/// This is the implementation. A field table has exactly two failure modes - a ledger
/// refusal and an allocator refusal - so the error type says so and no caller ever has to
/// map an impossible arm. [`reserve_fields`] wraps this for callers that already speak
/// [`FormatOutcome`].
///
/// # Errors
///
/// [`Refusal`] for a budget, cancellation or allocation refusal.
pub(crate) fn reserve_fields_narrow<'a>(
    count: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<Field<'a>>, Refusal> {
    let bytes = count.saturating_mul(core::mem::size_of::<Field<'a>>()) as u64;
    ledger.try_charge(bytes)?;
    ledger.checkpoint()?;
    let mut fields: Vec<Field<'a>> = Vec::new();
    if fields.try_reserve_exact(count).is_err() {
        return Err(Refusal::Allocation {
            requested: bytes as usize,
        });
    }
    Ok(fields)
}

/// Admit and reserve a field table, in [`FormatOutcome`] form.
///
/// Widens [`reserve_fields_narrow`] and nothing else: a field table has no format verdict
/// to produce, so this wrapper cannot invent one.
pub(crate) fn reserve_fields<'a>(
    count: usize,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<Vec<Field<'a>>> {
    match reserve_fields_narrow(count, ledger) {
        Ok(fields) => FormatOutcome::Ok(fields),
        Err(refusal) => FormatOutcome::Refused(refusal),
    }
}

/// Encode a `str` field value (`u32` length then bytes) into `out`, admitting the bytes first.
fn encode_str_admitted(
    value: &str,
    out: &mut Vec<u8>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    let total = 4 + value.len();
    if let Err(error) = ledger.try_charge(total as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if out.try_reserve_exact(total).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation { requested: total });
    }
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    FormatOutcome::Ok(())
}

/// Encode an extensions map, admitting its bytes before allocating them.
///
/// The count prefix and each key/value pair are admitted together, so the map cannot be built on credit.
fn encode_extensions_admitted(
    ext: &Extensions,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<Vec<u8>> {
    let entries = ext.entries();
    let total: usize = 4 + entries
        .iter()
        .map(|(k, v)| 4 + k.len() + 4 + v.len())
        .sum::<usize>();
    if let Err(error) = ledger.try_charge(total as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve_exact(total).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation { requested: total });
    }
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (key, value) in entries {
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
    FormatOutcome::Ok(out)
}

// ---------------------------------------------------------------------------
// Transaction encoder
// ---------------------------------------------------------------------------

/// Encode a complete transaction file.
///
/// ## What the caller may rely on
///
/// The sink receives exactly **one** complete image per call, and a failed encode writes **nothing** —
/// so a caller never has to distinguish a partial file from a complete one. That is the property a caller
/// actually depends on, and it holds for both destination shapes: for a sink because the image is built
/// before `write` is called, and for [`crate::current::encode_current`]'s caller-owned `Vec` because a
/// non-empty buffer is refused before anything is charged or appended.
///
/// This function deliberately does **not** restate that pristine-buffer guard. The caller's buffer is not
/// in play here — there is no buffer to be non-empty — so copying the check would suggest every encoder
/// needs it and teach the next reader a rule that is really specific to the `&mut Vec` shape.
/// The image is handed to `sink` in **one** `write` call with the caller's ledger. That does not make the
/// sink's own write atomic or durable, and a sink is free to write some bytes and then fail.
///
/// # Errors
///
/// * `Corrupt` for an inconsistent header, an illegal event field stream, or a failed §4.3/§7.2
///   composition check;
/// * `Limit` for any exceeded ceiling (file, block, event fields, string, events-per-tx);
/// * `UnsupportedFormat` for an unregistered tag, or for a family whose semantics this build cannot
///   validate yet (`NoCodec`);
/// * `Refused` for a ledger refusal, a caller stop, or an allocator refusal.
pub fn encode_tx(
    tx: &TxToEncode<'_>,
    sink: &mut dyn BytesSink,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    encode_tx_parts(&tx.header, &tx.events, sink, limits, ledger)
}

/// The canonical transaction-encoding core.
///
/// Takes the header and the per-event inputs **by reference**, so a caller that already
/// owns both (the Store preparing a commit) never clones a header or collects a second
/// event table. [`encode_tx`] and [`crate::prepared::encode_prepared`] both delegate here,
/// which is what makes them the same serialization rather than two that must be kept in
/// agreement.
pub(crate) fn encode_tx_parts(
    header: &TxHeaderToEncode,
    events: &[EventToEncode<'_>],
    sink: &mut dyn BytesSink,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    let event_count = u32::try_from(events.len()).unwrap_or(u32::MAX);
    match check_tx_header(header, event_count, limits) {
        FormatOutcome::Ok(()) => {}
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    }
    for event in events {
        match check_event(event, limits) {
            FormatOutcome::Ok(()) => {}
            FormatOutcome::Format(error) => return FormatOutcome::Format(error),
            FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
        }
        // The family's own reader validates what we are about to write.
        match validate_event_through_dispatch(event.tag, &event.fields, ledger) {
            FormatOutcome::Ok(()) => {}
            FormatOutcome::Format(error) => return FormatOutcome::Format(error),
            FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
        }
    }
    // The same §4.3/§7.2 composition rules the reader enforces, applied to what we are about to write.
    match check_composition_of(header, events) {
        FormatOutcome::Ok(()) => {}
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    }

    // Every variable-length value is admitted and reserved BEFORE the writer exists, so nothing is
    // allocated on credit. These buffers live in this frame; the field list borrows them.
    let mut principal_bytes: Vec<u8> = Vec::new();
    match encode_str_admitted(&header.principal_id, &mut principal_bytes, ledger) {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    let mut request_bytes: Vec<u8> = Vec::new();
    match encode_str_admitted(&header.request_id, &mut request_bytes, ledger) {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    let ext_bytes = if header.extensions.entries().is_empty() {
        None // §8.1: absence is expressed by omitting the field.
    } else {
        match encode_extensions_admitted(&header.extensions, ledger) {
            FormatOutcome::Ok(bytes) => Some(bytes),
            other => return propagate(other),
        }
    };

    let created = header.created_at.to_le_bytes();
    let version = header.version.to_le_bytes();
    let prev = header.prev_version.unwrap_or(0).to_le_bytes();
    let count = event_count.to_le_bytes();
    let profile = header.rules_profile.to_le_bytes();
    let canon = header.request_canon_version.code().to_le_bytes();
    let expect = header.expect_version.map(u64::to_le_bytes);

    let field_count = 12 + usize::from(expect.is_some()) + usize::from(ext_bytes.is_some());
    let mut fields = match reserve_fields(field_count, ledger) {
        FormatOutcome::Ok(fields) => fields,
        other => return propagate(other),
    };
    fields.push(Field::new(crate::transaction::TAG_CREATED_AT, &created));
    if let Some(ext) = ext_bytes.as_ref() {
        fields.push(Field::new(crate::transaction::TAG_EXTENSIONS, ext));
    }
    fields.push(Field::new(
        crate::transaction::TAG_KB_UUID,
        header.kb_uuid.as_bytes(),
    ));
    fields.push(Field::new(
        crate::transaction::TAG_RULES_DIGEST,
        &header.rules_digest,
    ));
    fields.push(Field::new(
        crate::transaction::TAG_PRINCIPAL_ID,
        &principal_bytes,
    ));
    fields.push(Field::new(
        crate::transaction::TAG_REQUEST_ID,
        &request_bytes,
    ));
    fields.push(Field::new(
        crate::transaction::TAG_PAYLOAD_DIGEST,
        &header.payload_digest,
    ));
    fields.push(Field::new(crate::transaction::TAG_VERSION, &version));
    fields.push(Field::new(crate::transaction::TAG_TX_ID, &header.tx_id));
    fields.push(Field::new(crate::transaction::TAG_PREV_VERSION, &prev));
    fields.push(Field::new(crate::transaction::TAG_EVENT_COUNT, &count));
    fields.push(Field::new(crate::transaction::TAG_RULES_PROFILE, &profile));
    fields.push(Field::new(
        crate::transaction::TAG_REQUEST_CANON_VERSION,
        &canon,
    ));
    if let Some(expect) = expect.as_ref() {
        fields.push(Field::new(crate::transaction::TAG_EXPECT_VERSION, expect));
    }
    // §4.2 requires strictly ascending tags; sorting here is safe because the set is a constant.
    fields.sort_unstable_by_key(|f| f.tag());

    let mut body: Vec<u8> = Vec::new();
    {
        let mut writer = Writer::new(&mut body, ledger);
        if let Err(error) = write_header(&mut writer, &fields) {
            return write_error_outcome(error);
        }
        for event in events {
            if let Err(error) = write_event(&mut writer, event.tag, 0b0001, &event.fields) {
                return write_error_outcome(error);
            }
        }
    }

    let mut image: Vec<u8> = Vec::new();
    match write_sglb(&mut image, Kind::Transaction, &body, limits, ledger) {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    match sink.write(&image, ledger) {
        Ok(()) => FormatOutcome::Ok(()),
        Err(refusal) => FormatOutcome::Refused(refusal),
    }
}

// ---------------------------------------------------------------------------
// Manifest encoder
// ---------------------------------------------------------------------------

/// Encode a complete manifest file.
///
/// The image is handed to `sink` in one `write` call. See [`encode_tx`] for the caller-visible
/// invariant — one complete image per call, nothing written on failure — and for why this encoder does
/// not carry a pristine-destination guard.
///
/// # Errors
///
/// * `Corrupt` for an inconsistent header, a genesis/non-genesis predecessor violation, or a name that
///   does not match its grammar (§2.4);
/// * `Limit` for a file, block, header-field or string ceiling;
/// * `Refused` for a ledger refusal, a caller stop, or an allocator refusal.
pub fn encode_manifest(
    m: &DecodedManifest,
    sink: &mut dyn BytesSink,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    if !crate::manifest::is_tx_file_name(&m.tx_file_name) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
            crate::manifest::TAG_TX_FILE_NAME,
        )));
    }
    match (&m.prev_manifest_name, m.prev_manifest_sha256) {
        (None, None) => {
            if m.version != 1 {
                return FormatOutcome::Format(FormatError::Corrupt(
                    CorruptReason::ManifestTxMismatch,
                ));
            }
        }
        (Some(name), Some(_)) => {
            if m.version <= 1 {
                return FormatOutcome::Format(FormatError::Corrupt(
                    CorruptReason::ManifestTxMismatch,
                ));
            }
            if !crate::file::is_valid_manifest_name(name.as_bytes()) {
                return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
                    crate::manifest::TAG_PREV_MANIFEST_NAME,
                )));
            }
        }
        // Half a link looks verifiable and is not.
        _ => {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
                crate::manifest::TAG_PREV_MANIFEST_NAME,
            )));
        }
    }
    for (tag, text) in [
        (crate::manifest::TAG_TX_FILE_NAME, m.tx_file_name.as_str()),
        (
            crate::manifest::TAG_PREV_MANIFEST_NAME,
            m.prev_manifest_name.as_deref().unwrap_or(""),
        ),
    ] {
        if !text.is_empty() && text.len() as u64 > u64::from(limits.max_string_bytes) {
            return FormatOutcome::Format(FormatError::Limit(
                sage_db_types::framing::LimitKind::String {
                    tag: Some(tag),
                    len: text.len() as u32,
                    max: limits.max_string_bytes,
                },
            ));
        }
    }

    let mut tx_name_bytes: Vec<u8> = Vec::new();
    match encode_str_admitted(&m.tx_file_name, &mut tx_name_bytes, ledger) {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    let prev_name_bytes = match m.prev_manifest_name.as_deref() {
        None => None,
        Some(name) => {
            let mut bytes: Vec<u8> = Vec::new();
            match encode_str_admitted(name, &mut bytes, ledger) {
                FormatOutcome::Ok(()) => Some(bytes),
                other => return propagate(other),
            }
        }
    };

    let created = m.created_at.to_le_bytes();
    let version = m.version.to_le_bytes();
    let profile = m.rules_profile.to_le_bytes();

    let field_count =
        11 + usize::from(prev_name_bytes.is_some()) + usize::from(m.prev_manifest_sha256.is_some());
    let mut fields = match reserve_fields(field_count, ledger) {
        FormatOutcome::Ok(fields) => fields,
        other => return propagate(other),
    };
    fields.push(Field::new(crate::manifest::TAG_CREATED_AT, &created));
    fields.push(Field::new(
        crate::manifest::TAG_KB_UUID,
        m.kb_uuid.as_bytes(),
    ));
    fields.push(Field::new(
        crate::manifest::TAG_RULES_DIGEST,
        &m.rules_digest,
    ));
    fields.push(Field::new(crate::manifest::TAG_VERSION, &version));
    fields.push(Field::new(crate::manifest::TAG_RULES_PROFILE, &profile));
    fields.push(Field::new(
        crate::manifest::TAG_STATE_DIGEST,
        &m.state_digest,
    ));
    fields.push(Field::new(
        crate::manifest::TAG_GRAPH_DIGEST,
        &m.graph_digest,
    ));
    fields.push(Field::new(
        crate::manifest::TAG_CHAIN_DIGEST,
        &m.chain_digest,
    ));
    fields.push(Field::new(
        crate::manifest::TAG_BOOKKEEPING_DIGEST,
        &m.bookkeeping_digest,
    ));
    if let Some(name) = prev_name_bytes.as_ref() {
        fields.push(Field::new(crate::manifest::TAG_PREV_MANIFEST_NAME, name));
    }
    if let Some(sha) = m.prev_manifest_sha256.as_ref() {
        fields.push(Field::new(crate::manifest::TAG_PREV_MANIFEST_SHA256, sha));
    }
    fields.push(Field::new(
        crate::manifest::TAG_TX_FILE_NAME,
        &tx_name_bytes,
    ));
    fields.push(Field::new(
        crate::manifest::TAG_TX_FILE_SHA256,
        &m.tx_file_sha256,
    ));
    fields.sort_unstable_by_key(|f| f.tag());

    let mut body: Vec<u8> = Vec::new();
    {
        let mut writer = Writer::new(&mut body, ledger);
        if let Err(error) = write_header(&mut writer, &fields) {
            return write_error_outcome(error);
        }
    }

    let mut image: Vec<u8> = Vec::new();
    match write_sglb(&mut image, Kind::Manifest, &body, limits, ledger) {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    match sink.write(&image, ledger) {
        Ok(()) => FormatOutcome::Ok(()),
        Err(refusal) => FormatOutcome::Refused(refusal),
    }
}

// ---------------------------------------------------------------------------
// Bridges
// ---------------------------------------------------------------------------

/// Run the shared composition validator over a transaction about to be encoded.
///
/// The writer has no decoded records and no wire stream, so it reads the facts from the field list it is
/// about to serialize. That is exactly why [`crate::transaction::CompositionChecker`] takes facts rather
/// than a representation: the reader builds them from a decoded record or a wire event, the writer builds
/// them from the same `(tag, fields)` pairs, and both get the same verdict.
fn check_composition_of(
    header: &TxHeaderToEncode,
    events: &[EventToEncode<'_>],
) -> FormatOutcome<()> {
    use crate::transaction::{CompositionChecker, CompositionFacts};

    let mut check = match CompositionChecker::new(
        header.version,
        header.request_canon_version.code(),
        header.payload_digest,
        header.principal_id.as_bytes(),
        header.request_id.as_bytes(),
    ) {
        FormatOutcome::Ok(c) => c,
        other => return propagate(other),
    };

    for event in events {
        let mut facts = CompositionFacts::tag_only(event.tag);
        let field = |tag: u16| {
            event
                .fields
                .iter()
                .find(|f| f.tag() == tag)
                .map(|f| f.value())
        };
        if let Some(bytes) = field(crate::transaction::TAG_PRINCIPAL_ID) {
            facts.principal_id = crate::transaction::str_payload(bytes).ok();
        }
        if let Some(bytes) = field(crate::transaction::TAG_REQUEST_ID) {
            facts.request_id = crate::transaction::str_payload(bytes).ok();
        }
        if let Some(bytes) =
            field(crate::transaction::TAG_PAYLOAD_DIGEST_FIELD).filter(|b| b.len() == 32)
        {
            {
                let mut digest = [0u8; 32];
                digest.copy_from_slice(bytes);
                facts.payload_digest = Some(digest);
            }
        }
        if let Some(bytes) = field(crate::transaction::TAG_COMMIT_VERSION).filter(|b| b.len() == 8)
        {
            {
                facts.commit_version = Some(u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]));
            }
        }
        if let Some(bytes) = field(crate::transaction::TAG_ID_COUNTER).filter(|b| b.len() == 2) {
            facts.id_space = Some(u16::from_le_bytes([bytes[0], bytes[1]]));
        }
        if let Some(bytes) =
            field(crate::transaction::TAG_HIGH_WATER_MARK).filter(|b| b.len() == 16)
        {
            let mut raw = [0u8; 16];
            raw.copy_from_slice(bytes);
            facts.high_water_mark = Some(u128::from_le_bytes(raw));
        }
        match check.observe(&facts) {
            FormatOutcome::Ok(()) => {}
            other => return propagate(other),
        }
    }
    match check.finish() {
        FormatOutcome::Ok(()) => {}
        other => return propagate(other),
    }
    FormatOutcome::Ok(())
}

/// Re-raise a nested failure as this function's return type, keeping refusals as refusals.
///
/// `Ok` is unreachable: the helper is only called with a value already known to be an error, which is
/// why the arm yields no value rather than inventing one.
fn propagate<T, U>(outcome: FormatOutcome<T>) -> FormatOutcome<U> {
    match outcome {
        FormatOutcome::Format(e) => FormatOutcome::Format(e),
        FormatOutcome::Refused(r) => FormatOutcome::Refused(r),
        FormatOutcome::Ok(_) => unreachable!("propagate called with a success"),
    }
}

/// Bridge a `WriteError`, widening `Infallible` to the caller's `()`.
///
/// `from_write_error` returns `FormatOutcome<Infallible>` because a successful *conversion* produces no
/// value: only the failure arms are reachable, so the `Ok` arm is `match never {}`.
fn write_error_outcome<T>(error: WriteError) -> FormatOutcome<T> {
    match from_write_error(error) {
        FormatOutcome::Format(e) => FormatOutcome::Format(e),
        FormatOutcome::Refused(r) => FormatOutcome::Refused(r),
        FormatOutcome::Ok(never) => match never {},
    }
}

/// The operator action implied by an encode outcome.
#[must_use]
pub fn classify_encode<T>(outcome: &FormatOutcome<T>) -> OperatorAction {
    classify(outcome)
}

/// Enumerate the blob references an event declares, through the shared bounded sink.
///
/// A writer must be able to answer "what does this reference?" using the same table the reader uses,
/// rather than a second list.
///
/// # Errors
///
/// [`Refusal`] when the sink's ledger refuses an admission.
pub fn for_each_event_blob_ref(
    event: &Event<'_>,
    sink: &mut dyn BlobRefSink,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Refusal> {
    crate::dispatch::for_each_blob_ref(event, sink, ledger)
}
