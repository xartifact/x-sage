//! The P1 bounded request frame: bytes and the `request`-domain digest.
//!
//! Design authority: `docs/design/storage-digests.md` §4.7, §4.7.2 and §4.7.3.
//!
//! # What P1 owns here, and what it deliberately does not
//!
//! §4.7 splits responsibility explicitly. P1 fixes **how** a caller's opaque operation
//! payloads are packed into a hashable frame: field order, length prefixes, ceilings, the
//! read set, and the expected version. P2 assigns meaning to `op_kind`, `op_version` and
//! the payload bytes.
//!
//! This module therefore validates widths and limits and nothing else. An unregistered
//! `(op_kind, op_version)` pair is a **legal frame with unknown semantics**: it is
//! accepted and preserved byte-for-byte so a replay cannot lose it, and the admission
//! layer refuses it in P2. Guessing at an operation table here would freeze P2 semantics
//! into the persistence contract, which §4.7 rules out by name.
//!
//! # Why this is not a second `FormatError`
//!
//! §9's four classes are verdicts about **disk bytes**: `CORRUPT` means "restore from
//! backup". A request frame is caller input, never a stored file, so an empty request or
//! an unsorted read set is an `INVALID_ARGUMENT`/`EMPTY_REQUEST` — telling an operator to
//! restore a backup because a client sent a malformed request would be a category error.
//! [`RequestOutcome`] therefore carries a request-domain verdict beside, not inside,
//! [`Refusal`], and a spent budget stays a refusal: §4.7.2's ceilings say
//! `BUDGET_EXCEEDED`, but this module **never fabricates a remaining allowance** to
//! express a normative limit — the limit is a [`RequestError::Limit`], and a real
//! [`BudgetError`] is only ever the ledger's own.
//!
//! # Canonical bytes (§4.7)
//!
//! ```text
//! canon_version:u16 = 1
//! op_count:u32                          // 1..=256
//! Op[op_count]
//! read_set:u32 ‖ RevPair[read_set]      // numeric ascending, no duplicates, 0..=1024
//! expected_version:u8 ‖ u64?            // 0 = absent; 1 = followed by u64 >= 1
//!
//! Op := op_kind:u16 | op_version:u16 | payload_len:u32 | payload[payload_len]
//! ```
//!
//! The `Op` sequence is **not** sorted: operation order is part of the request, and
//! reordering is a different request with a different digest. The read set **is** sorted
//! because it is a set; sorting it silently would let two different client byte strings
//! produce one digest, so disorder is refused instead.
//!
//! # Excluded from the frame, by construction
//!
//! There is no place in this layout for `RequestOutcome`, `principal_id`, any timestamp,
//! `version`/`commit_version`, or any post-commit digest. §4.7.3 excludes each one for a
//! stated reason (self-reference, or a value that changes on retry and would therefore
//! break idempotency). This module cannot emit them because the layout has no field for
//! them — a stronger guarantee than a check that would have to run.

use sage_db_types::framing::Refusal;
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{RecordKey, RevPair};

use crate::digest::{Domain, digest_bounded};
use crate::field::{read_record_key, write_record_key, write_revision};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// The only canonical request version this build writes or accepts.
pub const CANON_VERSION: u16 = 1;
/// Largest accepted operation count (§4.7.2).
pub const MAX_OPS: u32 = 256;
/// Largest accepted single payload (§4.7.2).
pub const MAX_PAYLOAD: u32 = 65_536;
/// Smallest accepted single payload: an operation with no bytes carries no request.
pub const MIN_PAYLOAD: u32 = 1;
/// Largest accepted read-set size (§4.7.2).
pub const MAX_READ_SET: u32 = 1024;
/// The field tag this module attributes a read-set ceiling to, so a limit diagnostic
/// names the collection that was too long rather than collapsing to tag 0.
pub const READ_SET_LIMIT_TAG: u16 = 114;
/// Largest accepted whole frame (§4.7.2): 1 MiB.
pub const MAX_FRAME_BYTES: u64 = 1024 * 1024;

/// One opaque operation.
///
/// The three fields are exactly what the format validates and no more: two `u16`
/// identifiers and a borrowed payload. Borrowing is deliberate — a request frame is
/// hashed and forwarded, not retained, so copying every payload would double the peak
/// memory of every request for no gain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequestOp<'a> {
    /// Caller-defined operation kind. Opaque to P1.
    pub kind: u16,
    /// Caller-defined operation version. Opaque to P1.
    pub version: u16,
    /// Caller-owned canonical payload bytes, `1..=65536`.
    pub payload: &'a [u8],
}

impl<'a> RequestOp<'a> {
    /// An operation with no payload is not a request; see [`RequestError::Limit`].
    #[must_use]
    pub fn new(kind: u16, version: u16, payload: &'a [u8]) -> Self {
        Self {
            kind,
            version,
            payload,
        }
    }
}

/// A request frame as the caller supplies it.
///
/// Module-local rather than a `persist` type because it is **frame input**, not a stored
/// record: it has no revision, no audit trail and no identity. The read set reuses the
/// canonical [`RevPair`] so the ordering rule has one definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequestFrame<'a> {
    /// Operations in caller order; order is part of the request.
    pub ops: &'a [RequestOp<'a>],
    /// Records the request read, ascending by numeric `RevPair` with no duplicates.
    pub read_set: &'a [RevPair],
    /// The version the caller expects, when it pins one.
    pub expected_version: Option<u64>,
}

impl<'a> RequestFrame<'a> {
    /// A frame over already-ordered inputs.
    #[must_use]
    pub const fn new(
        ops: &'a [RequestOp<'a>],
        read_set: &'a [RevPair],
        expected_version: Option<u64>,
    ) -> Self {
        Self {
            ops,
            read_set,
            expected_version,
        }
    }
}

/// A decoded request frame.
///
/// The payload slices borrow the caller's buffer, so decoding retains only the read set
/// and the operation table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecodedRequest<'a> {
    /// Operations in their wire order.
    pub ops: Vec<RequestOp<'a>>,
    /// The read set, validated ascending and unique.
    pub read_set: Vec<RevPair>,
    /// The version the caller pinned, if any.
    pub expected_version: Option<u64>,
}

impl<'a> DecodedRequest<'a> {
    /// This request as a frame, so it can be re-encoded without copying any payload.
    ///
    /// The returned lifetime is the borrow of `self`, not the buffer's: `RequestOp` is
    /// covariant in its lifetime, so the shorter borrow is the honest signature.
    #[must_use]
    pub fn frame(&self) -> RequestFrame<'_> {
        RequestFrame {
            ops: &self.ops,
            read_set: &self.read_set,
            expected_version: self.expected_version,
        }
    }
}

/// Which §4.7.2 ceiling a request exceeded, with the measured value and its bound.
///
/// Measured values travel with the verdict so a caller can report "257 > 256" without
/// re-deriving anything, and so no path has to invent a `max` it did not compare against.
/// Each bound below is the normative constant, never a derived byte remainder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestLimit {
    /// `op_count` outside `1..=256`. Zero is [`RequestError::Empty`], not this.
    Ops {
        /// The declared or supplied count.
        count: u32,
        /// The normative maximum.
        max: u32,
    },
    /// One `payload_len` outside `1..=65536`.
    ///
    /// No `tag`: an operation's kind is caller-opaque, and presenting it as a field tag
    /// would name a registry field that is not in this frame.
    OpPayload {
        /// The declared or supplied length.
        len: u32,
        /// The normative minimum.
        min: u32,
        /// The normative maximum.
        max: u32,
    },
    /// `read_set` exceeds `1024` entries.
    ReadSet {
        /// The declared or supplied count.
        count: u32,
        /// The normative maximum.
        max: u32,
    },
    /// The whole frame exceeds 1 MiB.
    FrameBytes {
        /// The measured or predicted frame length.
        len: u64,
        /// The normative maximum.
        max: u64,
    },
}

impl RequestLimit {
    /// The normative bound this verdict was decided against.
    ///
    /// Exposed so a caller formats a diagnostic from the same constant that refused the
    /// input, rather than restating a limit that could drift.
    #[must_use]
    pub const fn bound(&self) -> u64 {
        match self {
            Self::Ops { max, .. } => *max as u64,
            Self::OpPayload { max, .. } => *max as u64,
            Self::ReadSet { max, .. } => *max as u64,
            Self::FrameBytes { max, .. } => *max,
        }
    }
}

/// Why a request frame is not acceptable.
///
/// Deliberately **not** [`sage_db_types::framing::FormatError`]: none of these is a
/// verdict about stored bytes, so none of them may tell an operator to restore a backup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestError {
    /// `op_count == 0`: a request with no operations has nothing to hash or to apply.
    Empty,
    /// A set-valued or enumerated field violated its canonical form.
    InvalidArgument(InvalidArgument),
    /// A normative ceiling was exceeded.
    ///
    /// Deliberately module-local rather than [`sage_db_types::framing::LimitKind`]: those
    /// variants name *persisted file* structures (`CURRENT`, transaction header fields),
    /// and a request frame is caller input. Reporting a request ceiling through a file
    /// variant would tell an operator about a file that does not exist.
    Limit(RequestLimit),
    /// The frame declared a canonical version this build does not implement.
    ///
    /// §4.7.3 fixes `1` as the only canonical version and makes `>= 2` unsupported rather
    /// than forward-compatible. Version `0` is a different fault entirely and is reported
    /// as [`InvalidArgument::CanonVersionZero`]: a genesis transaction is not a request, so
    /// a frame claiming canonical version 0 is malformed rather than ahead of this build.
    UnsupportedCanonVersion(u16),
    /// The computed digest was the reserved all-zero sentinel.
    ///
    /// §4.7.3: all-zero `payload_digest` means "this transaction has no caller request".
    /// A real request that hashed to zero must be refused as an internal failure rather
    /// than accepted, or the sentinel and a genuine digest would be ambiguous.
    SentinelCollision,
}

/// Which canonical-form rule an input broke.
///
/// One variant per distinguishable fault. Collapsing several `DecodeError`s into a single
/// "malformed request" would tell a caller the wrong thing to fix, which is the same
/// class-collapse defect this crate rejects in `dispatch::decode_reason`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidArgument {
    /// The read set was not strictly ascending by numeric `RevPair`.
    ReadSetNotAscending,
    /// Two read-set entries named the same record revision.
    ReadSetDuplicate,
    /// The `expected_version` presence byte was neither 0 nor 1.
    ExpectedVersionFlag(u8),
    /// `expected_version` was present but not `>= 1`.
    ExpectedVersionValue(u64),
    /// A payload was empty; `1..=65536` is the declared range.
    EmptyPayload,
    /// A payload exceeded `65536` bytes.
    PayloadTooLong,
    /// A count exceeded the ceiling declared for that field.
    CountTooLong,
    /// A record key named a kind this build cannot represent.
    RecordKeyKind(u16),
    /// The frame declared canonical version `0`.
    ///
    /// Distinct from [`RequestError::UnsupportedCanonVersion`]: genesis is identified by
    /// `version == 1` at the transaction layer and is *not* a caller request, so a request
    /// frame claiming canon 0 is invalid, not from a newer build.
    CanonVersionZero,
    /// The frame ended before the structure did.
    Truncated,
    /// Bytes remained after the frame was fully decoded.
    TrailingBytes,
    /// The canonical version or an operation identifier was not parseable.
    ///
    /// Note what this does **not** mean: an unregistered but well-formed
    /// `(op_kind, op_version)` is accepted and preserved. This variant covers only bytes
    /// that cannot be read as a `u16` at all.
    UnknownOpKind,
    /// A string field was not valid UTF-8.
    NotUtf8,
    /// This module's own invariant broke; never a caller-input verdict.
    InternalInvariant(u8),
}

/// A request operation's complete result.
///
/// Three arms, mirroring [`sage_db_types::framing::FormatOutcome`]'s shape for the same
/// reason: a refusal is not a verdict. `Budget`/`Stopped`/`Allocation` stay themselves so
/// "retry with more allowance" is never reported as "your request was malformed".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RequestOutcome<T> {
    /// The operation completed.
    Ok(T),
    /// The request itself was unacceptable.
    Rejected(RequestError),
    /// The operation did not complete for a reason that is not a request verdict.
    Refused(Refusal),
}

impl<T> RequestOutcome<T> {
    /// The value, when the operation completed.
    ///
    /// # Errors
    ///
    /// The rejection or refusal otherwise, unchanged.
    pub fn into_result(self) -> Result<T, Self> {
        match self {
            Self::Ok(value) => Ok(value),
            other => Err(other),
        }
    }

    /// The rejection, when the request was unacceptable.
    #[must_use]
    pub const fn rejection(&self) -> Option<&RequestError> {
        match self {
            Self::Rejected(error) => Some(error),
            _ => None,
        }
    }

    /// The refusal, when the operation did not complete for a non-verdict reason.
    #[must_use]
    pub const fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused(refusal) => Some(*refusal),
            _ => None,
        }
    }
}

fn reject<T>(error: RequestError) -> RequestOutcome<T> {
    RequestOutcome::Rejected(error)
}

fn from_budget<T>(error: BudgetError) -> RequestOutcome<T> {
    RequestOutcome::Refused(Refusal::Budget(error))
}

/// Map a transport read failure.
///
/// A request frame is caller input, so a malformed byte string is a rejected argument and
/// never one of §9's four *disk* classes. The mapping is faithful rather than collapsed:
/// each [`DecodeError`] keeps its own [`InvalidArgument`] reason, because telling a caller
/// "your read set was unsorted" when the real fault was a truncated payload is the same
/// class-collapse defect this crate rejects elsewhere. Budget, cancellation and allocation
/// pass through as refusals.
fn from_read<T>(error: ReadError) -> RequestOutcome<T> {
    match error {
        ReadError::Data(inner) => {
            RequestOutcome::Rejected(RequestError::InvalidArgument(invalid_from_decode(inner)))
        }
        // The request frame never consults the event/field registry, so this arm means
        // this module's own dispatch was wired wrong, not that the caller's bytes are bad.
        ReadError::Uninterpretable(_) => RequestOutcome::Rejected(RequestError::InvalidArgument(
            InvalidArgument::UnknownOpKind,
        )),
        ReadError::Budget(error) => RequestOutcome::Refused(Refusal::Budget(error)),
        ReadError::Stopped(stop) => RequestOutcome::Refused(Refusal::Stopped(stop)),
        ReadError::Allocation { requested } => {
            RequestOutcome::Refused(Refusal::Allocation { requested })
        }
    }
}

/// One `DecodeError` -> one caller-facing reason, without collapsing distinct faults.
fn invalid_from_decode(error: DecodeError) -> InvalidArgument {
    match error {
        DecodeError::Truncated { .. } => InvalidArgument::Truncated,
        DecodeError::Trailing { .. } => InvalidArgument::TrailingBytes,
        DecodeError::CountTooLong { .. } => InvalidArgument::CountTooLong,
        DecodeError::StringTooLong { .. } => InvalidArgument::PayloadTooLong,
        DecodeError::UnknownEnum { tag, .. } => InvalidArgument::RecordKeyKind(tag),
        DecodeError::InvalidRevision { .. } => InvalidArgument::ExpectedVersionValue(0),
        DecodeError::NotUtf8 | DecodeError::NulInString => InvalidArgument::NotUtf8,
        DecodeError::FieldOrder { .. } => InvalidArgument::ReadSetNotAscending,
        DecodeError::ZeroLengthField { .. } => InvalidArgument::EmptyPayload,
        DecodeError::FieldFlags { .. }
        | DecodeError::EventFlags { .. }
        | DecodeError::UnregisteredField { .. }
        | DecodeError::InvalidExtensionKey
        | DecodeError::InvalidCheckpointLocation
        | DecodeError::MissingField { .. }
        | DecodeError::ForbiddenField { .. }
        | DecodeError::StatusMismatch { .. }
        | DecodeError::ActorOriginMismatch { .. }
        | DecodeError::WrongRevision { .. }
        | DecodeError::RecordKeyKindMismatch { .. }
        | DecodeError::WatermarkOutOfRange { .. }
        | DecodeError::IdOutOfRange { .. } => InvalidArgument::UnknownOpKind,
    }
}

/// Map a transport write failure.
///
/// Validation runs before any byte is produced, so a `Data` verdict here means this
/// module's own invariant broke, not that the caller was wrong.
fn from_write<T>(error: WriteError) -> RequestOutcome<T> {
    match error {
        WriteError::Budget(error) => RequestOutcome::Refused(Refusal::Budget(error)),
        WriteError::Stopped(stop) => RequestOutcome::Refused(Refusal::Stopped(stop)),
        WriteError::Allocation { requested } => {
            RequestOutcome::Refused(Refusal::Allocation { requested })
        }
        WriteError::Data(_)
        | WriteError::MissingRequiredField { .. }
        | WriteError::Internal { .. } => RequestOutcome::Rejected(RequestError::InvalidArgument(
            InvalidArgument::InternalInvariant(0),
        )),
    }
}

/// Check every normative ceiling and canonical-form rule before any byte is produced.
///
/// Order matters: counts and widths are checked from the outside in, so a hostile frame
/// is refused before it can drive an allocation.
fn validate(frame: &RequestFrame<'_>) -> Result<u64, RequestError> {
    if frame.ops.is_empty() {
        return Err(RequestError::Empty);
    }
    if frame.ops.len() > MAX_OPS as usize {
        // No field tag exists for the operation table: it is a frame container, not a
        // registered field. `None` says exactly that instead of inventing tag 0.
        return Err(RequestError::Limit(RequestLimit::Ops {
            count: frame.ops.len() as u32,
            max: MAX_OPS,
        }));
    }
    if frame.read_set.len() > MAX_READ_SET as usize {
        // `tag: None`: the read set is a frame container, not a registered field. Borrowing
        // tag 114 (`base_revisions`) would imply a field that is not present in this
        // request, so the request-domain error names it instead.
        return Err(RequestError::Limit(RequestLimit::ReadSet {
            count: frame.read_set.len() as u32,
            max: MAX_READ_SET,
        }));
    }
    if frame.expected_version == Some(0) {
        return Err(RequestError::InvalidArgument(
            InvalidArgument::ExpectedVersionValue(0),
        ));
    }

    let mut previous: Option<RevPair> = None;
    for pair in frame.read_set {
        if !pair.key.id_is_in_range() {
            return Err(RequestError::InvalidArgument(
                InvalidArgument::RecordKeyKind(pair.key.kind().as_u16()),
            ));
        }
        if let Some(prev) = previous {
            if prev == *pair {
                return Err(RequestError::InvalidArgument(
                    InvalidArgument::ReadSetDuplicate,
                ));
            }
            if prev > *pair {
                return Err(RequestError::InvalidArgument(
                    InvalidArgument::ReadSetNotAscending,
                ));
            }
        }
        previous = Some(*pair);
    }

    // Exact encoded length, computed rather than estimated: the frame ceiling and the
    // buffer admission both want the same number.
    let mut total: u64 = 2 + 4 + 4 + 1;
    for op in frame.ops {
        let len = op.payload.len();
        // One range 1..=65536 per §4.7.2, so 0 and 65537 share a class rather than 0
        // becoming an argument error. `tag: None` because `op.kind` is opaque:
        // passing it as a field tag would present an unregistered operation
        // identifier as a registered field.
        if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&(len as u32)) {
            return Err(RequestError::Limit(RequestLimit::OpPayload {
                len: len as u32,
                min: MIN_PAYLOAD,
                max: MAX_PAYLOAD,
            }));
        }
        total += 8 + len as u64;
    }
    total += frame.read_set.len() as u64 * 26;
    if frame.expected_version.is_some() {
        total += 8;
    }
    if total > MAX_FRAME_BYTES {
        return Err(RequestError::Limit(RequestLimit::FrameBytes {
            len: total,
            max: MAX_FRAME_BYTES,
        }));
    }
    Ok(total)
}

/// Encode a request frame into its canonical bytes.
///
/// The buffer is admitted once, through the caller's ledger, before it is reserved; the
/// writer then runs prepaid so no byte is charged twice.
///
/// # Errors
///
/// [`RequestOutcome::Rejected`] for any §4.7.2 violation, [`RequestOutcome::Refused`] for
/// a real budget, cancellation or allocation refusal.
pub fn encode_request(
    frame: &RequestFrame<'_>,
    ledger: &mut dyn SharedLedger,
) -> RequestOutcome<Vec<u8>> {
    let len = match validate(frame) {
        Ok(len) => len,
        Err(error) => return reject(error),
    };
    if let Err(error) = ledger.try_charge(len) {
        return from_budget(error);
    }
    if let Err(stop) = ledger.checkpoint() {
        return RequestOutcome::Refused(Refusal::Stopped(stop));
    }
    let capacity = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buf = Vec::new();
    if buf.try_reserve_exact(capacity).is_err() {
        return RequestOutcome::Refused(Refusal::Allocation {
            requested: capacity,
        });
    }
    let mut writer = Writer::prepaid(&mut buf, ledger, capacity);
    match write_body(&mut writer, frame) {
        Ok(()) => {
            // Prepayout check: the exact-length computation and the emitted bytes must
            // agree, or a later reader would see a frame its own size field denies.
            if buf.len() != capacity {
                return reject(RequestError::InvalidArgument(
                    InvalidArgument::InternalInvariant(0),
                ));
            }
            RequestOutcome::Ok(buf)
        }
        Err(error) => from_write(error),
    }
}

fn write_body(writer: &mut Writer<'_>, frame: &RequestFrame<'_>) -> Result<(), WriteError> {
    writer.u16(CANON_VERSION)?;
    writer.count(frame.ops.len())?;
    for op in frame.ops {
        writer.u16(op.kind)?;
        writer.u16(op.version)?;
        writer.count(op.payload.len())?;
        writer.bytes(op.payload)?;
    }
    writer.count(frame.read_set.len())?;
    for pair in frame.read_set {
        write_record_key(writer, pair.key)?;
        write_revision(writer, pair.revision)?;
    }
    match frame.expected_version {
        None => writer.u8(0)?,
        Some(version) => {
            writer.u8(1)?;
            writer.u64(version)?;
        }
    }
    Ok(())
}

/// Decode canonical request bytes.
///
/// Every count is checked against the format ceiling and against the bytes actually
/// remaining **before** the corresponding table is reserved, so a hostile count cannot
/// become an allocation.
///
/// # Errors
///
/// [`RequestOutcome::Rejected`] for a malformed or over-limit frame,
/// [`RequestOutcome::Refused`] for a real budget, cancellation or allocation refusal.
pub fn decode_request<'a>(
    bytes: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> RequestOutcome<DecodedRequest<'a>> {
    // Frame ceiling first: it is the cheapest check and bounds everything below.
    if bytes.len() as u64 > MAX_FRAME_BYTES {
        return reject(RequestError::Limit(RequestLimit::FrameBytes {
            len: bytes.len() as u64,
            max: MAX_FRAME_BYTES,
        }));
    }
    let mut reader = Reader::new(bytes);

    let canon = match reader.u16() {
        Ok(value) => value,
        Err(error) => return from_read(error),
    };
    if canon == 0 {
        // Genesis is identified at the transaction layer by `version == 1`; it is not a
        // caller request. A frame claiming canon 0 is malformed, not from a newer build.
        return reject(RequestError::InvalidArgument(
            InvalidArgument::CanonVersionZero,
        ));
    }
    if canon != CANON_VERSION {
        // §4.7.3: `1` is the only canonical version and `>= 2` is unsupported rather than
        // forward-compatible. Not a ceiling, so it is not reported as one.
        return reject(RequestError::UnsupportedCanonVersion(canon));
    }

    // Read the raw count, then apply the ceiling here. `Reader::count` would return
    // `CountTooLong`, which `from_read` maps to `InvalidArgument` -- making the read path
    // classify an over-ceiling request differently from the write path. §4.7.2 makes it a
    // ceiling diagnostic, so both paths must say `Limit`.
    let op_count = match reader.u32() {
        Ok(count) => count,
        Err(error) => return from_read(error),
    };
    if op_count == 0 {
        return reject(RequestError::Empty);
    }
    if op_count > MAX_OPS {
        return reject(RequestError::Limit(RequestLimit::Ops {
            count: op_count,
            max: MAX_OPS,
        }));
    }
    // Feasibility before reservation: each op needs 8 bytes of header plus >= 1 payload.
    let minimum_ops = (op_count as usize).saturating_mul(9);
    if minimum_ops > reader.remaining() {
        // The declared count is legal; the bytes are not there. Reporting this as a ceiling
        // with a derived `max` would make "too big" and "truncated" indistinguishable.
        return reject(RequestError::InvalidArgument(InvalidArgument::Truncated));
    }
    if let Err(error) = ledger.try_charge_entries(u64::from(op_count)) {
        return from_budget(error);
    }
    let op_slots = (op_count as u64).saturating_mul(core::mem::size_of::<RequestOp<'_>>() as u64);
    if let Err(error) = ledger.try_charge(op_slots) {
        return from_budget(error);
    }
    if let Err(stop) = ledger.checkpoint() {
        return RequestOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut ops = Vec::new();
    if ops.try_reserve_exact(op_count as usize).is_err() {
        return RequestOutcome::Refused(Refusal::Allocation {
            requested: op_slots as usize,
        });
    }
    for _ in 0..op_count {
        let kind = match reader.u16() {
            Ok(value) => value,
            Err(error) => return from_read(error),
        };
        let version = match reader.u16() {
            Ok(value) => value,
            Err(error) => return from_read(error),
        };
        let len = match reader.u32() {
            Ok(value) => value,
            Err(error) => return from_read(error),
        };
        if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&len) {
            return reject(RequestError::Limit(RequestLimit::OpPayload {
                len,
                min: MIN_PAYLOAD,
                max: MAX_PAYLOAD,
            }));
        }
        let payload = match reader.take_exact(len as usize) {
            Ok(value) => value,
            Err(error) => return from_read(error),
        };
        // `kind`/`version` are carried through untouched: P1 does not interpret them.
        ops.push(RequestOp {
            kind,
            version,
            payload,
        });
    }

    // Same reason as the op count above: a declared-but-impossible count is a ceiling
    // diagnostic on both paths, not an `InvalidArgument` on one of them.
    let read_count = match reader.u32() {
        Ok(count) => count,
        Err(error) => return from_read(error),
    };
    if read_count > MAX_READ_SET {
        return reject(RequestError::Limit(RequestLimit::ReadSet {
            count: read_count,
            max: MAX_READ_SET,
        }));
    }
    let minimum_read = (read_count as usize).saturating_mul(26);
    if minimum_read > reader.remaining() {
        return reject(RequestError::InvalidArgument(InvalidArgument::Truncated));
    }
    if let Err(error) = ledger.try_charge_entries(u64::from(read_count)) {
        return from_budget(error);
    }
    let read_slots = (read_count as u64).saturating_mul(core::mem::size_of::<RevPair>() as u64);
    if let Err(error) = ledger.try_charge(read_slots) {
        return from_budget(error);
    }
    if let Err(stop) = ledger.checkpoint() {
        return RequestOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut read_set = Vec::new();
    if read_set.try_reserve_exact(read_count as usize).is_err() {
        return RequestOutcome::Refused(Refusal::Allocation {
            requested: read_slots as usize,
        });
    }
    let mut previous: Option<RevPair> = None;
    for _ in 0..read_count {
        let key = match read_record_key(&mut reader) {
            Ok(key) => key,
            Err(error) => return from_read(error),
        };
        let revision = match crate::field::read_revision(&mut reader) {
            Ok(revision) => revision,
            Err(error) => return from_read(error),
        };
        let pair = RevPair { key, revision };
        if let Some(prev) = previous {
            if prev == pair {
                return reject(RequestError::InvalidArgument(
                    InvalidArgument::ReadSetDuplicate,
                ));
            }
            if prev > pair {
                return reject(RequestError::InvalidArgument(
                    InvalidArgument::ReadSetNotAscending,
                ));
            }
        }
        previous = Some(pair);
        read_set.push(pair);
    }

    let flag = match reader.u8() {
        Ok(value) => value,
        Err(error) => return from_read(error),
    };
    let expected_version = match flag {
        0 => None,
        1 => {
            let value = match reader.u64() {
                Ok(value) => value,
                Err(error) => return from_read(error),
            };
            if value == 0 {
                return reject(RequestError::InvalidArgument(
                    InvalidArgument::ExpectedVersionValue(0),
                ));
            }
            Some(value)
        }
        other => {
            return reject(RequestError::InvalidArgument(
                InvalidArgument::ExpectedVersionFlag(other),
            ));
        }
    };

    // Trailing bytes mean the frame was not the canonical encoding of what it claims.
    if !reader.is_empty() {
        return reject(RequestError::InvalidArgument(
            InvalidArgument::TrailingBytes,
        ));
    }
    if let Err(stop) = ledger.checkpoint() {
        return RequestOutcome::Refused(Refusal::Stopped(stop));
    }

    RequestOutcome::Ok(DecodedRequest {
        ops,
        read_set,
        expected_version,
    })
}

/// The canonical request digest: `digest("request", canonical bytes)`.
///
/// This is the value that tx header `178` and `RequestOutcome.178` must both carry. An
/// all-zero result is refused rather than returned, because §4.7.3 reserves that value to
/// mean "this transaction has no caller request".
///
/// # Errors
///
/// As [`encode_request`], plus [`RequestError::SentinelCollision`].
pub fn request_digest(
    frame: &RequestFrame<'_>,
    ledger: &mut dyn SharedLedger,
) -> RequestOutcome<[u8; 32]> {
    let bytes = match encode_request(frame, ledger) {
        RequestOutcome::Ok(bytes) => bytes,
        RequestOutcome::Rejected(error) => return reject(error),
        RequestOutcome::Refused(refusal) => return RequestOutcome::Refused(refusal),
    };
    match digest_bounded(Domain::Request, &bytes, ledger) {
        Ok(digest) => {
            if digest == [0u8; 32] {
                return reject(RequestError::SentinelCollision);
            }
            RequestOutcome::Ok(digest)
        }
        Err(error) => RequestOutcome::Refused(match error {
            crate::digest::BoundedHashError::Budget(budget) => Refusal::Budget(budget),
            crate::digest::BoundedHashError::Stopped(stop) => Refusal::Stopped(stop),
        }),
    }
}

/// The digest of already-encoded frame bytes, without re-encoding.
///
/// Used when a caller received the frame and must verify the persisted `178` without
/// reconstructing a [`RequestFrame`] — the comparison is the point, so re-deriving the
/// bytes would defeat it.
///
/// **This does not validate structure.** Any input within the frame ceiling is hashed,
/// including a non-frame byte string or a frame with trailing bytes; `Ok` means "hashed",
/// not "a legal request". Pair with [`decode_request`] when structure matters, and compare
/// digests only between values that were both validated.
///
/// # Errors
///
/// As [`request_digest`], plus a frame-ceiling rejection.
pub fn request_digest_of(bytes: &[u8], ledger: &mut dyn SharedLedger) -> RequestOutcome<[u8; 32]> {
    if bytes.len() as u64 > MAX_FRAME_BYTES {
        return reject(RequestError::Limit(RequestLimit::FrameBytes {
            len: bytes.len() as u64,
            max: MAX_FRAME_BYTES,
        }));
    }
    match digest_bounded(Domain::Request, bytes, ledger) {
        Ok(digest) => {
            if digest == [0u8; 32] {
                return reject(RequestError::SentinelCollision);
            }
            RequestOutcome::Ok(digest)
        }
        Err(error) => RequestOutcome::Refused(match error {
            crate::digest::BoundedHashError::Budget(budget) => Refusal::Budget(budget),
            crate::digest::BoundedHashError::Stopped(stop) => Refusal::Stopped(stop),
        }),
    }
}

/// The payload digest value that means "no caller request" (§4.7.3).
#[must_use]
pub const fn sentinel_digest() -> [u8; 32] {
    [0u8; 32]
}

/// Whether a persisted `payload_digest` is the genesis sentinel rather than a real digest.
#[must_use]
pub fn is_sentinel_digest(digest: [u8; 32]) -> bool {
    digest == sentinel_digest()
}

/// A read-set entry's canonical numeric key, for callers that must order one themselves.
///
/// Exposed so a caller never reimplements "numeric ascending" with byte order: the LE
/// encoding of a `u128` is not numerically ordered, which §1.2 names as an outright wrong
/// implementation.
#[must_use]
pub fn read_set_key(key: RecordKey, revision: RecordRevision) -> (u16, u128, u64) {
    (key.kind().as_u16(), key.id(), revision.get())
}
