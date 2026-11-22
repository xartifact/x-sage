//! `SGLB` envelope and the transaction file (kind = 2).
//!
//! Layers, outermost first:
//!
//! 1. **Envelope** (§2): 32-byte header, 1..N body blocks, 32-byte trailer. This module owns the
//!    *whole-file* reading of it, because a transaction file is exactly one such file; block-level
//!    arithmetic stays in [`crate::file`].
//! 2. **`tx_header`** (§4.2): a `field_count`-prefixed stream of `Field` envelopes with a fixed
//!    permitted/required/forbidden set.
//! 3. **Events** (§4.1): a `Field[event_count]` sequence, each read through [`read_dispatched`] so
//!    framing, registry acceptance and family *availability* are applied by their owner and not
//!    restated here.
//!
//! ## What this module refuses to pretend
//!
//! ## Scope limit, stated precisely
//!
//! **This module does not validate per-family record content.** As handed over, `read_dispatched`
//! applies transport framing, registry acceptance (tag registered, REQUIRED flag present, fields
//! registered) and family *availability* — `CodecFamily::of` and the `Unlanded` check. It does **not**
//! call the family decoders (`term.rs`, `assertion.rs`, …), so a well-framed event whose fields
//! violate its family's permitted/required/forbidden sets is not rejected here.
//!
//! Consequently [`read_transaction`] establishes: the envelope is intact, the header is legal and
//! self-consistent, every event is well-framed and registered, and `event_count` matches. It does
//! **not** establish that a transaction's events are semantically valid. Do not describe its success
//! as "the transaction is valid" until the family validator bridge is wired.
//!
//! Two things it does get right in the meantime:
//!
//! * a registered tag whose codec is absent from the build surfaces as
//!   `UnsupportedFormat(NoCodec)` and is **propagated unchanged** — an incomplete build is not a
//!   damaged file, and rewriting it to `Corrupt` would tell an operator to restore a backup because
//!   of a missing feature;
//! * a tag that is not registered at all is a foreign file, not a skippable extra.
//!
//! ## Why events are streamed, not collected
//!
//! `event_count` is up to 65 536. Returning `Vec<Event>` would allocate per event and hold the whole
//! transaction resident. [`read_transaction`] instead hands each event to a caller-supplied
//! [`EventVisitor`], and the ledger charges as it goes, so peak memory is one event regardless of
//! transaction size.

use sage_db_types::framing::GenesisShapeRule;
use sage_db_types::framing::{
    FormatError, FormatOutcome, LimitKind, Limits, OperatorAction, Refusal, UnsupportedReason,
    classify,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;

use crate::dispatch::{DispatchedEvent, read_dispatched};
use crate::file::{
    BLOCK_FLAG_REQUIRED, BLOCK_HEADER_LEN, BlockHeader, CODEC_NONE, FILE_HEADER_LEN, FileHeader,
    Kind, Layout, TRAILER_LEN,
};
use crate::wire::{ReadError, Reader};

/// Tag of `tx_header.event_count` (§4.2).
pub const TAG_EVENT_COUNT: u16 = 195;
/// Tag of `tx_header.tx_id` (§4.2).
pub const TAG_TX_ID: u16 = 193;
/// Tag of `tx_header.version` (§4.2).
pub const TAG_VERSION: u16 = 192;
/// Tag of `tx_header.prev_version` (§4.2).
pub const TAG_PREV_VERSION: u16 = 194;
/// Tag of `tx_header.kb_uuid` (§4.2).
pub const TAG_KB_UUID: u16 = 144;
/// Tag of `tx_header.request_canon_version` (§4.2).
pub const TAG_REQUEST_CANON_VERSION: u16 = 206;
/// Tag of `tx_header.request_id` (§4.2).
pub const TAG_REQUEST_ID: u16 = 177;
/// Tag of `tx_header.payload_digest` (§4.2).
pub const TAG_PAYLOAD_DIGEST: u16 = 178;
/// Tag of `tx_header.principal_id` (§4.2).
pub const TAG_PRINCIPAL_ID: u16 = 176;
/// Tag of `tx_header.rules_digest` (§4.2).
pub const TAG_RULES_DIGEST: u16 = 163;
/// Tag of `tx_header.created_at` (§4.2).
pub const TAG_CREATED_AT: u16 = 8;
/// Tag of `tx_header.rules_profile` (§4.2).
pub const TAG_RULES_PROFILE: u16 = 196;
/// Tag of the optional `tx_header.expect_version` (§4.2).
pub const TAG_EXPECT_VERSION: u16 = 197;
/// Tag of the optional `tx_header.extensions` (§4.2).
pub const TAG_EXTENSIONS: u16 = 14;

/// Bytes of the fixed `tx_header` part before its first field: `field_count:u16`.
const TX_HEADER_COUNT_LEN: usize = 2;

/// A transaction's header, decoded and cross-checked.
///
/// Cross-field consistency (`tx_id` matching the file name, `expect_version` equalling
/// `prev_version`, `version == prev_version + 1`) is checked by
/// [`TransactionHeader::check_against`] rather than at decode time, because the facts needed live
/// partly outside the file: the file name, and the previous committed version. Reporting a mismatch
/// needs both sides, and inventing one inside the decoder would be a second authority on the chain.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransactionHeader {
    /// `192 version`: this transaction's version; must be `prev_version + 1`.
    pub version: u64,
    /// `194 prev_version`: the version this transaction follows; 0 for genesis.
    pub prev_version: u64,
    /// `195 event_count`: must equal the number of events actually parsed. Encoded as `u32` (§7.5),
    /// with a legal range of 1..=65536 — so it is not a `u16` on the wire even though the ceiling is.
    pub event_count: u32,
    /// `193 tx_id`: 16 bytes, must equal the `<tid>` in the file name.
    pub tx_id: [u8; 16],
    /// `144 kb_uuid`: must match the chain.
    pub kb_uuid: KbUuid,
    /// `206 request_canon_version`: 0 = non-request (genesis), 1 = caller request v1.
    pub request_canon_version: u16,
    /// `178 payload_digest`: all-zero is the genesis sentinel.
    pub payload_digest: [u8; 32],
    /// `163 rules_digest`.
    pub rules_digest: [u8; 32],
    /// `196 rules_profile`.
    pub rules_profile: u16,
    /// `177 request_id`: a string (§7.5), not a fixed 16-byte id. Genesis uses the literal
    /// `genesis`, which is why it cannot be a byte array.
    ///
    /// Owned because this value escapes the read: the body may be an owned joined buffer, so a
    /// borrowed `&str` could not outlive it. Two short strings per transaction is the whole cost, and
    /// they are read once per commit, not per event.
    pub request_id: String,
    /// `176 principal_id`: a string (§7.5). Genesis uses the literal `system`.
    pub principal_id: String,
    /// `8 created_at`.
    pub created_at: u64,
    /// `197 expect_version`, when present; must equal `prev_version`.
    pub expect_version: Option<u64>,
    /// Tags actually present, ascending, for the forbidden-set check.
    present: [bool; 192],
}

impl TransactionHeader {
    /// Whether this transaction is the genesis transaction (`version == 1`).
    #[must_use]
    pub const fn is_genesis(&self) -> bool {
        self.version == 1 && self.prev_version == 0
    }

    /// Whether the all-zero `payload_digest` sentinel agrees with `request_canon_version`.
    ///
    /// §4.2 pairs them: genesis uses `request_canon_version = 0` **and** the zero digest. A build
    /// that accepted one without the other would let a real request masquerade as genesis (or the
    /// reverse), and `request_id` de-duplication keys off that distinction.
    #[must_use]
    pub fn request_canon_agrees(&self) -> bool {
        let zero = self.payload_digest == [0u8; 32];
        match self.request_canon_version {
            0 => zero,
            1 => !zero,
            // >= 2 is UNSUPPORTED_FORMAT, reported at decode time, not here.
            _ => true,
        }
    }

    /// Cross-check against facts only the caller holds.
    ///
    /// # Errors
    ///
    /// * `Corrupt(RevisionNotMonotonic)` when `version != prev_version + 1`;
    /// * `Corrupt(TxIdMismatch)` when the header id differs from the file name;
    /// * `Corrupt(OutcomeHeaderMismatch)` when `expect_version != prev_version`;
    /// * `Corrupt(RequestCanonMismatch)` when the sentinel and the canon version disagree.
    pub fn check_against(&self, file_tx_id: Option<[u8; 16]>) -> FormatOutcome<()> {
        if self.version != self.prev_version.saturating_add(1) {
            return FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::RevisionNotMonotonic {
                    expected: self.prev_version.saturating_add(1),
                    found: self.version,
                },
            ));
        }
        if file_tx_id.is_some_and(|expected_id| expected_id != self.tx_id) {
            return FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::TxIdMismatch,
            ));
        }
        if self
            .expect_version
            .is_some_and(|expect| expect != self.prev_version)
        {
            return FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::OutcomeHeaderMismatch {
                    tag: TAG_EXPECT_VERSION,
                },
            ));
        }
        if !self.request_canon_agrees() {
            return FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::RequestCanonMismatch,
            ));
        }
        FormatOutcome::Ok(())
    }

    /// Whether the header carries a tag. Used for the §4.2 forbidden-set check.
    #[must_use]
    pub fn carries(&self, tag: u16) -> bool {
        self.present.get(tag as usize).copied().unwrap_or(false)
    }
}

/// Fields §4.2 forbids in a `tx_header`.
///
/// A permitted-list alone is not enough: these tags belong to records, and accepting one here would
/// silently accept a record field in a header, where nothing would ever validate it.
const FORBIDDEN_IN_TX_HEADER: [u16; 13] = [
    1,  // record_key
    2,  // revision
    4,  // status
    5,  // recorded_by
    6,  // actor_origin
    9,  // updated_at
    10, // extensions-adjacent record fields
    11, 12, 13, // record fields
    198, 199, 200, // digests reserved outside 163/178
];

fn is_forbidden_in_tx_header(tag: u16) -> bool {
    if FORBIDDEN_IN_TX_HEADER.contains(&tag) {
        return true;
    }
    // §4.2: the record-owned range `16..=191` is forbidden in a tx header *except* the four
    // fields that the header itself legitimately carries (144, 163, 176, 177, 178).
    matches!(tag, 16..=191)
        && !matches!(
            tag,
            TAG_KB_UUID | TAG_RULES_DIGEST | TAG_PRINCIPAL_ID | TAG_REQUEST_ID | TAG_PAYLOAD_DIGEST
        )
}

/// The result of reading a whole transaction file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransactionSummary {
    /// Decoded header.
    pub header: TransactionHeader,
    /// Events actually visited; must equal `header.event_count`.
    pub events_seen: u32,
    /// SHA-256 the trailer declared (`§2.3`), for the store to compare with `CURRENT`.
    pub trailer_sha256: [u8; 32],
}

/// Receives each validated event in order.
///
/// A trait rather than a returned collection: `event_count` reaches 65 536, and the caller usually
/// folds each event into a record set as it arrives. Collecting them would allocate once per event
/// and keep the transaction resident.
pub trait EventVisitor {
    /// Visit one event. `event_seq` is its 0-based dense index (§4.1), which is the only definition
    /// of the sequence number — there is no field for it in the envelope.
    ///
    /// # Errors
    ///
    /// [`Refusal`] when the visitor's own ledger refuses.
    fn visit(
        &mut self,
        event_seq: u32,
        event: &DispatchedEvent<'_>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal>;
}

/// A visitor that discards events, for callers that only need the header and the counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardEvents;

impl EventVisitor for DiscardEvents {
    fn visit(
        &mut self,
        _event_seq: u32,
        _event: &DispatchedEvent<'_>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        // Still a cancellation point: a caller who stops wants the loop to end even when the
        // payload is being discarded.
        ledger.checkpoint().map_err(Refusal::Stopped)
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// A parsed `SGLB` envelope: header plus the concatenated body.
///
/// `body` borrows the caller's buffer. Multiple blocks are concatenated logically, not copied: the
/// spec allows a block boundary to fall inside a field, so the decoder must see one contiguous body,
/// and the caller's buffer already is one.
pub struct Envelope<'a> {
    /// The 32-byte header.
    pub header: FileHeader,
    /// Block headers in order, for CRC verification by the caller.
    pub blocks: Vec<BlockHeader>,
    /// The concatenated block payloads.
    ///
    /// Borrowed when the file has one block (the common case), owned when the payloads had to be
    /// joined across several block headers. `Cow` rather than an unconditional `Vec` so the single
    /// block path allocates nothing, and rather than `&'a [u8]` so the multi-block path does not leak.
    pub body: std::borrow::Cow<'a, [u8]>,
    /// The trailer's declared SHA-256.
    pub trailer_sha256: [u8; 32],
}

/// Parse the outer envelope of an `SGLB` file.
///
/// Verifies, in order: the header CRC over the zeroed-CRC-field construction, all fixed header
/// fields, the `payload_len` identity, every block header (bounds, kind, flags, codec, CRC over the
/// *encoded* bytes), and the trailer hash over header-phus-blocks.
///
/// # Errors
///
/// * `UnsupportedFormat` for a foreign magic, major, kind, block kind or reserved codec;
/// * `Corrupt` for header/trailer/block damage and length disagreement;
/// * `Limit` when the file or a block exceeds its ceiling;
/// * `Refused` for a ledger budget or caller stop.
pub fn read_envelope<'a>(
    bytes: &'a [u8],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<Envelope<'a>> {
    use crate::file::{HEADER_CRC_OFFSET, HEADER_PAYLOAD_LEN_OFFSET, block_codec};
    use sage_db_types::framing::{CorruptReason, LimitKind as LK};

    if let Err(error) = ledger.try_charge(bytes.len() as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }

    if bytes.len() as u64 > limits.max_tx_file_bytes {
        return FormatOutcome::Format(FormatError::Limit(LK::TxFile));
    }
    if bytes.len() < FILE_HEADER_LEN + TRAILER_LEN {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::TruncatedHeader));
    }

    // Magic, major, minor: a foreign file is UNSUPPORTED, never CORRUPT.
    let magic = &bytes[0..4];
    if magic != Layout::Sglb.magic() {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Magic));
    }
    let major = u16::from_le_bytes([bytes[4], bytes[5]]);
    if major != 0 {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Major(
            major,
        )));
    }
    let minor = u16::from_le_bytes([bytes[6], bytes[7]]);
    if minor != 0 {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Minor(
            minor,
        )));
    }
    let kind_code = u16::from_le_bytes([bytes[8], bytes[9]]);
    let kind = match kind_code {
        1 => Kind::Manifest,
        2 => Kind::Transaction,
        other => {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::FileKind(other),
            ));
        }
    };
    let flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    if flags != 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags));
    }
    let header_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    if header_len as usize != FILE_HEADER_LEN {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::TruncatedHeader));
    }

    let payload_len = u64::from_le_bytes([
        bytes[HEADER_PAYLOAD_LEN_OFFSET],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 1],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 2],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 3],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 4],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 5],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 6],
        bytes[HEADER_PAYLOAD_LEN_OFFSET + 7],
    ]);
    // `payload_len == file_len - 32`, which also localises truncation and concatenation.
    if payload_len != (bytes.len() - FILE_HEADER_LEN) as u64 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PayloadLenMismatch));
    }

    // Header CRC over the whole 32 B header with the CRC field zeroed: 32 bytes in, NOT 28.
    let mut crc_input = [0u8; FILE_HEADER_LEN];
    crc_input.copy_from_slice(&bytes[..FILE_HEADER_LEN]);
    let stored_header_crc = u32::from_le_bytes([
        bytes[HEADER_CRC_OFFSET],
        bytes[HEADER_CRC_OFFSET + 1],
        bytes[HEADER_CRC_OFFSET + 2],
        bytes[HEADER_CRC_OFFSET + 3],
    ]);
    crc_input[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].fill(0);
    if crc32c::crc32c(&crc_input) != stored_header_crc {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderCrc));
    }
    let reserved = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    if reserved != 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags));
    }

    // Blocks tile the region between the header and the trailer exactly.
    let body_region = &bytes[FILE_HEADER_LEN..bytes.len() - TRAILER_LEN];
    let mut offset = 0usize;
    let mut blocks: Vec<BlockHeader> = Vec::new();
    while offset < body_region.len() {
        if body_region.len() - offset < BLOCK_HEADER_LEN {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
        }
        let h = &body_region[offset..offset + BLOCK_HEADER_LEN];
        let block = BlockHeader {
            kind: u16::from_le_bytes([h[0], h[1]]),
            flags: u16::from_le_bytes([h[2], h[3]]),
            encoded_len: u32::from_le_bytes([h[4], h[5], h[6], h[7]]),
            raw_len: u32::from_le_bytes([h[8], h[9], h[10], h[11]]),
            crc32c: u32::from_le_bytes([h[12], h[13], h[14], h[15]]),
        };

        if block.kind != 1 {
            // Either an unknown block kind, or a registered one this build does not know.
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::BlockKind(block.kind),
            ));
        }
        if block.flags & BLOCK_FLAG_REQUIRED == 0 {
            // A block that does not declare itself required cannot be skipped safely.
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
        }
        if !block.reserved_flags_clear() {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
        }
        if block_codec(block.flags) != CODEC_NONE {
            // zstd is reserved in major 0: a writer must not emit it, a reader must not skip it.
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::Codec(block_codec(block.flags)),
            ));
        }
        if block.encoded_len == 0 || block.raw_len == 0 {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
        }
        if block.encoded_len as u64 > limits.max_block_encoded_bytes {
            return FormatOutcome::Format(FormatError::Limit(LK::BlockEncoded));
        }
        if block.raw_len as u64 > limits.max_block_raw_bytes {
            return FormatOutcome::Format(FormatError::Limit(LK::BlockRaw));
        }

        let total = match block.total_len() {
            Some(total) if total <= body_region.len() - offset => total,
            // Either overflow or a length running past the body.
            _ => return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds)),
        };
        let data = &body_region[offset + BLOCK_HEADER_LEN..offset + total];
        // CRC is over the ENCODED data, which for codec none is the data itself.
        if crc32c::crc32c(data) != block.crc32c {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockCrc));
        }

        blocks.push(block);
        offset += total;
    }
    if blocks.is_empty() {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
    }

    // Trailer: SHA-256 over the header (with its real CRC bytes) and every block byte.
    let trailer_at = bytes.len() - TRAILER_LEN;
    let mut declared = [0u8; 32];
    declared.copy_from_slice(&bytes[trailer_at..]);
    let mut hasher = Sha256Lite::new();
    hasher.update(&bytes[..trailer_at]);
    if hasher.finish() != declared {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::TrailerSha));
    }

    // The body is the concatenation of block *payloads*, excluding the 16-byte block headers. With
    // one block the payload is already a contiguous subslice, so nothing is copied; with several, a
    // block header can sit in the middle of a field, so the payloads must be joined once.
    let body: std::borrow::Cow<'a, [u8]> = if blocks.len() == 1 {
        std::borrow::Cow::Borrowed(&body_region[BLOCK_HEADER_LEN..])
    } else {
        let payload_total: usize = blocks.iter().map(|b| b.encoded_len as usize).sum();
        // A real allocation proportional to the transaction, so it is charged like one.
        if let Err(error) = ledger.try_charge(payload_total as u64) {
            return FormatOutcome::Refused(Refusal::Budget(error));
        }
        let mut joined: Vec<u8> = Vec::new();
        if joined.try_reserve_exact(payload_total).is_err() {
            return FormatOutcome::Refused(Refusal::Allocation {
                requested: payload_total,
            });
        }
        let mut at = 0usize;
        for block in &blocks {
            let start = at + BLOCK_HEADER_LEN;
            let end = start + block.encoded_len as usize;
            joined.extend_from_slice(&body_region[start..end]);
            at = end;
        }
        std::borrow::Cow::Owned(joined)
    };

    FormatOutcome::Ok(Envelope {
        header: FileHeader {
            layout: Layout::Sglb,
            kind,
            payload_len,
            header_crc32c: stored_header_crc,
        },
        blocks,
        body,
        trailer_sha256: declared,
    })
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

/// Read a transaction file: envelope, header, then every event through the dispatch seam.
///
/// `file_tx_id` is the `<tid>` parsed from the file name, when the caller has a name to check
/// against; pass `None` for a purely structural read.
///
/// ## What `Ok` means
///
/// `Ok` means *structural* success: the envelope verifies, the header is legal and internally
/// consistent, every event is well-framed and registry-accepted, and `event_count` matches the events
/// present. It does **not** mean the events are semantically valid — family record validation is not
/// part of the dispatch seam yet (see the module docs). Callers must not report a transaction as
/// committed-and-valid on the strength of this alone.
///
/// # Errors
///
/// Whatever the envelope, header or dispatch layer reported, unchanged. In particular a family codec
/// absent from this build surfaces as `UnsupportedFormat(NoCodec)` and is never rewritten to
/// `Corrupt`.
///
/// Note that `Ok` is structural only; see the function docs for the exact extent.
pub fn read_transaction(
    bytes: &[u8],
    limits: &Limits,
    file_tx_id: Option<[u8; 16]>,
    visitor: &mut dyn EventVisitor,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<TransactionSummary> {
    let envelope = match read_envelope(bytes, limits, ledger) {
        FormatOutcome::Ok(e) => e,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    if envelope.header.kind != Kind::Transaction {
        // A manifest read as a transaction is a caller error about which file is which; the bytes
        // are legal, so this is not corruption.
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FileKind(
            Kind::Manifest.code(),
        )));
    }

    let (header, events_offset) = match read_tx_header(&envelope.body, limits, file_tx_id, ledger) {
        FormatOutcome::Ok(v) => v,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    // Header-only composition rule first: `version == 1` ⟺ canon 0 ⟺ all-zero sentinel. This needs no
    // events, so it is decided before the event stream costs anything.
    let mut check = match CompositionChecker::new(
        header.version,
        header.request_canon_version,
        header.payload_digest,
        header.principal_id.as_bytes(),
        header.request_id.as_bytes(),
    ) {
        FormatOutcome::Ok(c) => c,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    // Streamed, not buffered. Buffering the dispatched events would mean retaining
    // `count * size_of::<(u16, DispatchedEvent)>()` — a table this function never admitted, and one that
    // a 65 536-event transaction makes large. Composition state is O(1), so it needs no retention.
    //
    // Consequence, stated plainly: the visitor is called **per event as it is decoded**, so a later
    // failure (a broken composition rule, a bad frame) leaves the visitor with a *partial* set. That is
    // the pre-existing contract of this seam — the visitor must treat its own state as scratch and
    // discard it on error. It is not all-or-nothing, and no caller may assume otherwise.
    let mut reader = Reader::new(&envelope.body[events_offset..]);
    let expected_events = header.event_count;
    let mut seen: u32 = 0;
    for seq in 0..expected_events {
        // The tag is needed for composition; peek it so a decoded `Record` payload needs no accessor.
        let tag = match reader.peek_event_tag() {
            Ok(tag) => tag,
            Err(error) => {
                return crate::dispatch::from_read_error(crate::wire::ReadError::Data(error));
            }
        };
        let dispatched = match read_dispatched(&mut reader, ledger) {
            FormatOutcome::Ok(dispatched) => dispatched,
            FormatOutcome::Format(error) => return FormatOutcome::Format(error),
            FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
        };
        let facts = composition_facts(tag, &dispatched);
        match check.observe(&facts) {
            FormatOutcome::Ok(()) => {}
            FormatOutcome::Format(error) => return FormatOutcome::Format(error),
            FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
        }
        if let Err(refusal) = visitor.visit(seq, &dispatched, ledger) {
            return FormatOutcome::Refused(refusal);
        }
        seen += 1;
    }

    match check.finish() {
        FormatOutcome::Ok(()) => {}
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    }

    // `event_count` must equal the events actually parsed. Extra bytes after the declared count are
    // as wrong as a short stream: either one means the header and the body disagree.
    if !reader.is_empty() {
        return FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::EventCount,
        ));
    }
    if seen != expected_events {
        return FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::EventCount,
        ));
    }
    if expected_events == 0 {
        // §4.1 rule 1: a transaction with no events has nothing to replay yet consumes a version.
        return FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::ZeroEvents,
        ));
    }

    FormatOutcome::Ok(TransactionSummary {
        header,
        events_seen: seen,
        trailer_sha256: envelope.trailer_sha256,
    })
}

/// Read only a transaction's header, without touching the event stream.
///
/// This exists because the header is independently useful — the store compares its fields against the
/// manifest and the previous version before deciding anything — and because the event layer can stop
/// early for a legitimate reason (`NoCodec`, a visitor refusing) without the header being in doubt.
/// A caller that needs "what does this transaction claim?" should not have to decode events to find
/// out, and must not infer the header from a failed event read.
///
/// # Errors
///
/// Envelope and header classification only. A wrong file kind, a damaged header or a foreign field
/// tag is reported here; nothing about the events is examined.
pub fn read_transaction_header(
    bytes: &[u8],
    limits: &Limits,
    file_tx_id: Option<[u8; 16]>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<TransactionHeader> {
    let envelope = match read_envelope(bytes, limits, ledger) {
        FormatOutcome::Ok(e) => e,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };
    if envelope.header.kind != Kind::Transaction {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FileKind(
            Kind::Manifest.code(),
        )));
    }
    match read_tx_header(&envelope.body, limits, file_tx_id, ledger) {
        FormatOutcome::Ok((header, _events_offset)) => FormatOutcome::Ok(header),
        FormatOutcome::Format(error) => FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
    }
}

// ---------------------------------------------------------------------------
// Single-file composition: §4.3 genesis shape and §7.2 outcome derivation/join
// ---------------------------------------------------------------------------

/// The tag and field view a [`CompositionChecker`] needs from a dispatched event.
///
/// A natural/structural family yields a decoded record and exposes no `Event`, so its composition facts
/// are read from the record itself where the record carries them, and otherwise only the tag is known.
/// That is deliberate: the three composition rules are stated in terms of tag, `164`, `176`/`177`/`178`
/// and `181`, all of which the record exposes for the families that have them.
/// Build the composition facts for one dispatched event.
///
/// Two representations exist for the same values: an identified family yields a `Validated` event with
/// wire fields (`u32 len || bytes` for a `str`), while a natural/structural family yields a decoded
/// `Record` with typed fields. Both are reduced to the same [`CompositionFacts`], so the validator never
/// needs to know which path produced the event — and a decoded record is the *better* source, because
/// its fields are already type-checked.
fn composition_facts<'a>(tag: u16, dispatched: &'a DispatchedEvent<'a>) -> CompositionFacts<'a> {
    let mut facts = CompositionFacts::tag_only(tag);
    match dispatched {
        DispatchedEvent::Validated { event, .. } => {
            if let Some(bytes) = event.field(TAG_PRINCIPAL_ID) {
                facts.principal_id = str_payload(bytes).ok();
            }
            if let Some(bytes) = event.field(TAG_REQUEST_ID) {
                facts.request_id = str_payload(bytes).ok();
            }
            if let Some(bytes) = event
                .field(TAG_PAYLOAD_DIGEST_FIELD)
                .filter(|b| b.len() == 32)
            {
                {
                    let mut digest = [0u8; 32];
                    digest.copy_from_slice(bytes);
                    facts.payload_digest = Some(digest);
                }
            }
            if let Some(bytes) = event.field(TAG_COMMIT_VERSION).filter(|b| b.len() == 8) {
                {
                    facts.commit_version = Some(u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]));
                }
            }
            if let Some(bytes) = event.field(TAG_ID_COUNTER).filter(|b| b.len() == 2) {
                facts.id_space = Some(u16::from_le_bytes([bytes[0], bytes[1]]));
            }
            if let Some(bytes) = event.field(TAG_HIGH_WATER_MARK).filter(|b| b.len() == 16) {
                let mut raw = [0u8; 16];
                raw.copy_from_slice(bytes);
                facts.high_water_mark = Some(u128::from_le_bytes(raw));
            }
        }
        // A decoded record is asked for the same facts through one narrow trait method, so the
        // composition rules never have to know which family produced the event.
        DispatchedEvent::Record { payload, .. } => match payload {
            crate::dispatch::DispatchedPayload::RequestOutcome(record) => {
                facts.principal_id = Some(record.principal_id.as_bytes());
                facts.request_id = Some(record.request_id.as_bytes());
                facts.payload_digest = Some(record.payload_digest);
                facts.commit_version = Some(record.commit_version);
            }
            crate::dispatch::DispatchedPayload::IdAllocation(record) => {
                // `IdSpace` is a fieldless enum whose discriminants are the wire counters (0..=8).
                facts.id_space = Some(record.allocation.space as u16);
                facts.high_water_mark = Some(record.allocation.hwm);
            }
            // Other families contribute no composition fact; the tag alone is enough for them.
            _ => {}
        },
    }
    facts
}

/// The composition facts one event contributes.
///
/// Built either from a decoded record's typed fields or from a wire field stream, so the validator is
/// genuinely shared: a writer assembling an event it has not serialized and a reader holding a decoded
/// record produce the same struct.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompositionFacts<'a> {
    /// Event tag.
    pub tag: u16,
    /// `176 principal_id` payload bytes (no length prefix).
    pub principal_id: Option<&'a [u8]>,
    /// `177 request_id` payload bytes (no length prefix).
    pub request_id: Option<&'a [u8]>,
    /// `178 payload_digest`.
    pub payload_digest: Option<[u8; 32]>,
    /// `181 commit_version`.
    pub commit_version: Option<u64>,
    /// `164 counter`, the id space selector.
    pub id_space: Option<u16>,
    /// `165 high_water_mark`. Genesis requires 0 in every space, so an allocation carrying a non-zero
    /// mark would claim ids that were never allocated — the one fact §4.3 states about the mark.
    pub high_water_mark: Option<u128>,
}

impl<'a> CompositionFacts<'a> {
    /// Facts for an event known only by tag.
    #[must_use]
    pub const fn tag_only(tag: u16) -> Self {
        Self {
            tag,
            principal_id: None,
            request_id: None,
            payload_digest: None,
            commit_version: None,
            id_space: None,
            high_water_mark: None,
        }
    }
}

/// Strip the `u32` length prefix from a wire `str` field value.
pub(crate) fn str_payload(value: &[u8]) -> Result<&[u8], ()> {
    if value.len() < 4 {
        return Err(());
    }
    let len = u32::from_le_bytes([value[0], value[1], value[2], value[3]]) as usize;
    if value.len() != 4 + len {
        return Err(());
    }
    Ok(&value[4..])
}

/// A genesis-shape violation, naming which §4.3 rule failed.
fn genesis_shape(rule: sage_db_types::framing::GenesisShapeRule) -> FormatOutcome<()> {
    FormatOutcome::Format(FormatError::Corrupt(
        sage_db_types::framing::CorruptReason::GenesisShape { rule },
    ))
}

/// A composition-count violation.
fn outcome_count(seen: usize) -> FormatOutcome<()> {
    FormatOutcome::Format(FormatError::Corrupt(
        sage_db_types::framing::CorruptReason::OutcomeCount {
            found: u16::try_from(seen).unwrap_or(u16::MAX),
        },
    ))
}

/// A composition join violation naming the field that disagreed.
fn outcome_field(tag: u16) -> FormatOutcome<()> {
    FormatOutcome::Format(FormatError::Corrupt(
        sage_db_types::framing::CorruptReason::OutcomeHeaderMismatch { tag },
    ))
}

/// Tag of `Genesis`.
pub const GENESIS_TAG: u16 = 224;
/// Tag of `IdAllocation`.
pub const TAG_ID_ALLOCATION: u16 = 176;
/// Tag of `RequestOutcomeRecorded`.
pub const TAG_REQUEST_OUTCOME: u16 = 192;
/// `IdAllocation.counter`, the id space selector.
pub const TAG_ID_COUNTER: u16 = 164;
/// `IdAllocation.high_water_mark`; `u128`, mandatory. Genesis requires 0 in every space (§4.3).
pub const TAG_HIGH_WATER_MARK: u16 = 165;
/// `RequestOutcome.commit_version`.
pub const TAG_COMMIT_VERSION: u16 = 181;
/// `payload_digest`, which appears in the header and again inside a `RequestOutcome`.
pub const TAG_PAYLOAD_DIGEST_FIELD: u16 = 178;
/// Nine id spaces, so genesis carries exactly nine `IdAllocation` events.
const ID_SPACE_COUNT: usize = 9;

/// Streaming validator for the single-file composition rules of a transaction.
///
/// Three rules, all pure functions of the header and the ordered event stream:
///
/// 1. **§4.3 genesis shape.** `version == 1` means exactly one `Genesis` (224) then nine
///    `IdAllocation` (176) whose counters are the dense set `0..8`, and no `RequestOutcome`.
/// 2. **§7.2 derivation.** `version == 1` ⟺ `request_canon_version == 0` ⟺ the payload digest is the
///    all-zero sentinel. Any contradiction is `Corrupt`.
/// 3. **§7.2 ordinary outcomes.** `version >= 2` carries exactly one `RequestOutcomeRecorded` (192)
///    whose `176`/`177`/`178` are byte-equal to the header's and whose `181 commit_version` equals the
///    header's `192 version`.
///
/// Rule 2 is header-only and runs in [`CompositionChecker::new`]: a `version == 1` frame carrying canon
/// version 1 is already contradictory regardless of its events. Rules 1 and 3 are keyed off `version`,
/// never off "an outcome happens to be present": zero outcomes is legal at version 1, illegal at 2.
///
/// Streaming, so the state is O(1) and validating a 65 536-event transaction retains nothing. The same
/// checker runs on the read path and the write path, which is what keeps the two from drifting.
///
/// Not checked, deliberately: anything needing another version of the database — revision continuity,
/// prior high water marks, `chain_digest` agreement. Those are fold/replay facts.
#[derive(Debug)]
pub struct CompositionChecker<'a> {
    version: u64,
    header_principal: &'a [u8],
    header_request: &'a [u8],
    header_digest: [u8; 32],
    seen: usize,
    outcomes: u8,
}

impl<'a> CompositionChecker<'a> {
    /// Start a check, applying the header-only rule immediately.
    ///
    /// # Errors
    ///
    /// `Corrupt(RequestCanonMismatch)` when `version == 1`, `request_canon_version == 0` and the
    /// all-zero digest sentinel do not all agree.
    pub fn new(
        version: u64,
        request_canon_version: u16,
        payload_digest: [u8; 32],
        header_principal: &'a [u8],
        header_request: &'a [u8],
    ) -> FormatOutcome<Self> {
        let sentinel = payload_digest == [0u8; 32];
        let is_genesis_version = version == 1;
        let canon_is_zero = request_canon_version == 0;
        if is_genesis_version != canon_is_zero || is_genesis_version != sentinel {
            return FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::RequestCanonMismatch,
            ));
        }
        FormatOutcome::Ok(Self {
            version,
            header_principal,
            header_request,
            header_digest: payload_digest,
            seen: 0,
            outcomes: 0,
        })
    }

    /// Observe one event, in commit order, from its composition facts.
    ///
    /// # Errors
    ///
    /// `Corrupt(OutcomeCount)` or `Corrupt(OutcomeHeaderMismatch)` as soon as a rule breaks, so a caller
    /// rejects without reading the rest of the file.
    pub fn observe(&mut self, facts: &CompositionFacts<'_>) -> FormatOutcome<()> {
        let seen = self.seen;
        if self.version == 1 {
            let expected: Option<u16> = if seen == 0 {
                Some(GENESIS_TAG)
            } else if seen <= ID_SPACE_COUNT {
                Some(TAG_ID_ALLOCATION)
            } else {
                None
            };
            if expected != Some(facts.tag) {
                // Which rule failed depends on what appeared where.
                if seen == 0 {
                    self.seen += 1;
                    return genesis_shape(GenesisShapeRule::MissingGenesis);
                }
                if facts.tag == GENESIS_TAG {
                    self.seen += 1;
                    return genesis_shape(GenesisShapeRule::GenesisNotFirst);
                }
                self.seen += 1;
                // `seen` is the 0-based position of the offending event. Whether the truthful count is
                // `seen` or `seen + 1` is UNREACHABLE through the public path: a wrong tag is rejected by
                // the family validator before this rule runs (verified — a genesis with event 1 retagged
                // 176 -> 192 returns ForbiddenField{event:192, field:164}, not a shape reason). Rather
                // than guess, this reports the count of events that were definitely present. Revisit if
                // the dispatch seam ever stops rejecting foreign tags first.
                return genesis_shape(GenesisShapeRule::EventCount {
                    found: u16::try_from(seen).unwrap_or(u16::MAX),
                });
            }
            if facts.tag == TAG_ID_ALLOCATION {
                // `counter` must be the dense 0..8 set: the k-th allocation names space k-1.
                match facts.id_space {
                    Some(space) if usize::from(space) == seen - 1 => {}
                    Some(space) => {
                        self.seen += 1;
                        return genesis_shape(GenesisShapeRule::AllocationOrder { space });
                    }
                    None => {
                        self.seen += 1;
                        return genesis_shape(GenesisShapeRule::MissingSpace {
                            space: u16::try_from(seen - 1).unwrap_or(0),
                        });
                    }
                }
                // §4.3: every high water mark is 0 at genesis. A non-zero mark would claim ids that no
                // event in this transaction allocated, which no later version can reconcile.
                match facts.high_water_mark {
                    Some(0) => {}
                    Some(hwm) => {
                        let space = facts.id_space.unwrap_or_default();
                        self.seen += 1;
                        return genesis_shape(GenesisShapeRule::NonZeroWatermark { space, hwm });
                    }
                    // Missing `165` is a field-level absence, not a shape rule we can name further.
                    None => {
                        self.seen += 1;
                        return outcome_field(TAG_HIGH_WATER_MARK);
                    }
                }
            }
            self.seen += 1;
            return FormatOutcome::Ok(());
        }

        if facts.tag == TAG_REQUEST_OUTCOME {
            self.outcomes = self.outcomes.saturating_add(1);
            if self.outcomes > 1 {
                self.seen += 1;
                // The count of outcomes actually present, which is what the field means.
                return outcome_count(usize::from(self.outcomes));
            }
            for (field_tag, expected, actual) in [
                (TAG_PRINCIPAL_ID, self.header_principal, facts.principal_id),
                (TAG_REQUEST_ID, self.header_request, facts.request_id),
            ] {
                match actual {
                    Some(value) if value == expected => {}
                    _ => {
                        self.seen += 1;
                        return outcome_field(field_tag);
                    }
                }
            }
            match facts.payload_digest {
                Some(value) if value == self.header_digest => {}
                _ => {
                    self.seen += 1;
                    return outcome_field(TAG_PAYLOAD_DIGEST_FIELD);
                }
            }
            match facts.commit_version {
                Some(commit) if commit == self.version => {}
                _ => {
                    self.seen += 1;
                    return outcome_field(TAG_COMMIT_VERSION);
                }
            }
        }
        self.seen += 1;
        FormatOutcome::Ok(())
    }

    /// Finish once every event has been observed.
    ///
    /// # Errors
    ///
    /// `Corrupt(GenesisShape)` when a `version == 1` transaction did not have the exact §4.3 shape, or
    /// `Corrupt(OutcomeCount)` when a non-genesis transaction does not carry exactly one outcome. The
    /// genesis arm is a shape failure, not an outcome count — genesis is exempt from that rule.
    pub fn finish(self) -> FormatOutcome<()> {
        let corrupt = |found: u16| {
            FormatOutcome::Format(FormatError::Corrupt(
                sage_db_types::framing::CorruptReason::OutcomeCount { found },
            ))
        };
        if self.version == 1 {
            if self.seen != ID_SPACE_COUNT + 1 {
                return genesis_shape(GenesisShapeRule::EventCount {
                    found: u16::try_from(self.seen).unwrap_or(u16::MAX),
                });
            }
            return FormatOutcome::Ok(());
        }
        if self.outcomes != 1 {
            return corrupt(u16::from(self.outcomes));
        }
        FormatOutcome::Ok(())
    }
}

/// Decode the `tx_header` and return it with the offset at which the events begin.
///
/// Returns `(header, event_offset)`.
fn read_tx_header(
    body: &[u8],
    limits: &Limits,
    file_tx_id: Option<[u8; 16]>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<(TransactionHeader, usize)> {
    use sage_db_types::framing::CorruptReason;

    if body.len() < TX_HEADER_COUNT_LEN {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    let field_count = u16::from_le_bytes([body[0], body[1]]);
    if field_count == 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    if u64::from(field_count) > limits.max_header_fields as u64 {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::HeaderFields));
    }

    let mut reader = Reader::new(body);
    let mut present = [false; 192];

    // The header envelope is owned by `crate::event::read_header`: count ceiling, ascending strict
    // order, zero flags, non-zero lengths. Hand-parsing it here was a second authority on the same
    // bytes and read `tag` where the envelope has `tag|flags|len`.
    let fields = match crate::event::read_header(&mut reader, ledger) {
        Ok(fields) => fields,
        Err(error) => return header_read_error(error),
    };
    if fields.len() != field_count as usize {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    // Bytes consumed by the whole header = declared region minus what is left of it.
    let header_envelope_bytes = body.len() - reader.remaining();

    // Every header field we need; `Option` distinguishes absent from zero.
    let mut version = None;
    let mut prev_version = None;
    let mut event_count = None;
    let mut tx_id = None;
    let mut kb_uuid = None;
    let mut request_canon_version = None;
    let mut payload_digest = None;
    let mut rules_digest = None;
    let mut rules_profile = None;
    let mut request_id: Option<String> = None;
    let mut principal_id: Option<String> = None;
    let mut created_at = None;
    let mut expect_version = None;

    for field in &fields {
        let tag = field.tag();
        if is_forbidden_in_tx_header(tag) {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
        }
        if let Some(slot) = present.get_mut(tag as usize) {
            *slot = true;
        }

        // Read this field's value (its own slice, no envelope) by tag. Unknown-but-not-forbidden
        // header tags are refused rather than skipped: a header field nobody validates is a field
        // nobody can rely on.
        let mut value = Reader::new(field.value());
        if let Err(error) = read_header_field(
            tag,
            &mut value,
            limits,
            ledger,
            &mut version,
            &mut prev_version,
            &mut event_count,
            &mut tx_id,
            &mut kb_uuid,
            &mut request_canon_version,
            &mut payload_digest,
            &mut rules_digest,
            &mut rules_profile,
            &mut request_id,
            &mut principal_id,
            &mut created_at,
            &mut expect_version,
        ) {
            return header_read_error(error);
        }
        if !value.is_empty() {
            // Slack bytes in a value mean it was not what its registered type says.
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
        }
    }

    // Required set (§4.2) must be complete; a missing one is a defect we must report, not a file we
    // can pretend is fine.
    macro_rules! required {
        ($value:expr, $tag:expr) => {
            match $value {
                Some(v) => v,
                None => {
                    return FormatOutcome::Format(FormatError::Internal(
                        sage_db_types::framing::InternalReason::MissingRequired {
                            event: None,
                            tag: $tag,
                        },
                    ));
                }
            }
        };
    }
    let header = TransactionHeader {
        version: required!(version, TAG_VERSION),
        prev_version: required!(prev_version, TAG_PREV_VERSION),
        event_count: required!(event_count, TAG_EVENT_COUNT),
        tx_id: required!(tx_id, TAG_TX_ID),
        kb_uuid: required!(kb_uuid, TAG_KB_UUID),
        request_canon_version: required!(request_canon_version, TAG_REQUEST_CANON_VERSION),
        payload_digest: required!(payload_digest, TAG_PAYLOAD_DIGEST),
        rules_digest: required!(rules_digest, TAG_RULES_DIGEST),
        rules_profile: required!(rules_profile, TAG_RULES_PROFILE),
        request_id: required!(request_id, TAG_REQUEST_ID),
        principal_id: required!(principal_id, TAG_PRINCIPAL_ID),
        created_at: required!(created_at, TAG_CREATED_AT),
        expect_version,
        present,
    };

    // `request_canon_version >= 2` is a future major capability, not damage.
    if header.request_canon_version >= 2 {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(
            UnsupportedReason::RulesNotSupported,
        ));
    }

    // `event_count` is the authority for how many events follow, but zero is illegal outright.
    if header.event_count == 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ZeroEvents));
    }
    if header.event_count > limits.max_events_per_tx {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::EventsPerTx));
    }

    match header.check_against(file_tx_id) {
        FormatOutcome::Ok(()) => {}
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    }

    let consumed = header_envelope_bytes;
    FormatOutcome::Ok((header, consumed))
}

/// Map a `read_header` failure onto the shared classification, preserving refusals.
fn header_read_error(error: ReadError) -> FormatOutcome<(TransactionHeader, usize)> {
    match error {
        ReadError::Budget(_) | ReadError::Stopped(_) => {
            match crate::dispatch::from_read_error(error) {
                FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
                FormatOutcome::Format(e) => FormatOutcome::Format(e),
                FormatOutcome::Ok(()) => {
                    FormatOutcome::Refused(Refusal::Allocation { requested: 0 })
                }
            }
        }
        ReadError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
        ReadError::Data(crate::wire::DecodeError::CountTooLong { .. }) => {
            FormatOutcome::Format(FormatError::Limit(LimitKind::HeaderFields))
        }
        _ => FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::FieldCount,
        )),
    }
}

/// Read one header field body into its slot.
///
/// Kept as one function with out-parameters rather than fourteen near-identical arms at the call site:
/// the tags, widths and required-ness come from §4.2, and this is the single place that maps them.
#[allow(clippy::too_many_arguments)]
fn read_header_field(
    tag: u16,
    reader: &mut Reader<'_>,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
    version: &mut Option<u64>,
    prev_version: &mut Option<u64>,
    event_count: &mut Option<u32>,
    tx_id: &mut Option<[u8; 16]>,
    kb_uuid: &mut Option<KbUuid>,
    request_canon_version: &mut Option<u16>,
    payload_digest: &mut Option<[u8; 32]>,
    rules_digest: &mut Option<[u8; 32]>,
    rules_profile: &mut Option<u16>,
    request_id: &mut Option<String>,
    principal_id: &mut Option<String>,
    created_at: &mut Option<u64>,
    expect_version: &mut Option<u64>,
) -> Result<(), ReadError> {
    match tag {
        TAG_VERSION => *version = Some(reader.u64()?),
        TAG_PREV_VERSION => *prev_version = Some(reader.u64()?),
        TAG_EVENT_COUNT => *event_count = Some(reader.u32()?),
        TAG_TX_ID => *tx_id = Some(read_16(reader)?),
        TAG_KB_UUID => *kb_uuid = Some(KbUuid::from_bytes(read_16(reader)?)),
        TAG_REQUEST_CANON_VERSION => *request_canon_version = Some(reader.u16()?),
        TAG_PAYLOAD_DIGEST => *payload_digest = Some(reader.digest32()?),
        TAG_RULES_DIGEST => *rules_digest = Some(reader.digest32()?),
        TAG_RULES_PROFILE => *rules_profile = Some(reader.u16()?),
        TAG_REQUEST_ID => {
            *request_id = Some(crate::field::read_str(
                reader,
                limits.max_string_bytes,
                ledger,
            )?);
        }
        TAG_PRINCIPAL_ID => {
            *principal_id = Some(crate::field::read_str(
                reader,
                limits.max_string_bytes,
                ledger,
            )?);
        }
        TAG_CREATED_AT => *created_at = Some(reader.u64()?),
        TAG_EXPECT_VERSION => *expect_version = Some(reader.u64()?),
        TAG_EXTENSIONS => {
            // Extensions are opaque here, but they must still be consumed exactly.
            crate::field::read_extensions(reader, ledger)?;
        }
        // An unregistered header tag is a foreign file, not a damaged one.
        _ => {
            return Err(ReadError::Uninterpretable(
                crate::registry::SchemaError::UnknownField {
                    event: 0,
                    field: tag,
                },
            ));
        }
    }
    Ok(())
}

fn read_16(reader: &mut Reader<'_>) -> Result<[u8; 16], ReadError> {
    let slice = reader.take_exact(16)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(slice);
    Ok(out)
}

/// Operator action for a transaction read.
#[must_use]
pub fn classify_transaction<T>(outcome: &FormatOutcome<T>) -> OperatorAction {
    classify(outcome)
}

/// Bridge a wire error, keeping the four classes intact.
#[must_use]
pub fn from_wire_error(error: ReadError) -> FormatOutcome<()> {
    crate::dispatch::from_read_error(error)
}

// ---------------------------------------------------------------------------
// SHA-256 for trailer verification
// ---------------------------------------------------------------------------

/// Streaming SHA-256 for the trailer.
///
/// A local implementation rather than a dependency because the trailer is the only place this crate
/// needs SHA-256 and the algorithm is fixed; `crc32c` is a dependency precisely because CRC-32C is
/// **not** something to hand-roll. Kept incremental so a 64 MiB transaction is hashed without holding
/// a second copy.
struct Sha256Lite {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Sha256Lite {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0u8; 64],
            buffered: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        if self.buffered > 0 {
            let need = 64 - self.buffered;
            let take = need.min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        let (chunks, _) = block.as_chunks::<4>();
        for (word, chunk) in w.iter_mut().zip(chunks) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
            self.state[5],
            self.state[6],
            self.state[7],
        );
        for (round, word) in w.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[round])
                .wrapping_add(*word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.length * 8;
        let mut padding = [0u8; 72];
        padding[0] = 0x80;
        let pad_len = if self.buffered < 56 {
            56 - self.buffered
        } else {
            120 - self.buffered
        };
        self.update(&padding[..pad_len]);
        // `update` counted the padding; restore the true length for the trailer field.
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}
