//! File-level classifications, ceilings and sinks, shared by every format surface.
//!
//! Design authority: `docs/design/storage-format.md` §1.3 (limits), §9 (the four
//! error classes) and §9.2 (the decision table).
//!
//! # Why the refusal classes are not [`FormatError`] variants
//!
//! `storage-format.md` §9 fixes **four** classes for bytes: `UNSUPPORTED_FORMAT`,
//! `CORRUPT`, `FORMAT_LIMIT` and `INTERNAL`. Each prescribes a different operator
//! action, and none of them is "retry" or "abandon".
//!
//! Budget exhaustion, caller cancellation and allocator refusal are *not* verdicts
//! about the file. Turning them into one of the four classes would tell an operator
//! to restore a backup (or migrate) because a request ran out of allowance - the
//! exact laundering this module exists to prevent. They therefore travel beside the
//! classification in [`FormatOutcome`], losslessly, and the classes stay four.
//!
//! The bytes-to-class mapping itself needs the decoder's own error type, so it lives
//! in `sage-db-format`; this crate declares only what is dependency-free.

use crate::ledger::{BudgetError, SharedLedger};
use crate::query::ControlStop;

/// A legal file this build cannot interpret.
///
/// Distinct from damage: the bytes may be a perfectly good file from another major,
/// another kind, or a feature this binary does not implement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnsupportedReason {
    /// Not an SGLB file at all.
    Magic,
    /// A format major this build does not implement.
    Major(u16),
    /// A non-zero minor; `major 0` has no minor capability.
    Minor(u16),
    /// A registered-kind space value this build does not know, including reserved 3.
    FileKind(u16),
    /// A block kind outside the single kind `major 0` allows.
    BlockKind(u16),
    /// A compression codec this build does not implement.
    Codec(u16),
    /// An event tag absent from the closed registry of this major.
    EventTag(u16),
    /// A field tag absent from the closed registry of this major.
    FieldTag(u16),
    /// A registered field carried a value outside its closed domain.
    EnumValue {
        /// The field tag involved.
        tag: u16,
        /// The value that appeared.
        value: u16,
    },
    /// A registered event whose codec is not part of this build.
    ///
    /// The bytes are legal in this major; this binary simply cannot interpret that
    /// event. Named separately from [`Self::EventTag`] so the two are never confused:
    /// an unregistered tag is a foreign file, an unlanded codec is an incomplete build.
    NoCodec {
        /// The registered event tag.
        tag: u16,
    },
    /// An allocation high-water mark beyond its space's declared bound.
    WatermarkOutOfRange {
        /// The allocation space whose bound was exceeded.
        space: u16,
        /// The value that appeared.
        value: u128,
    },
    /// An identified record's key beyond the bound its kind declares.
    IdOutOfRange {
        /// The record kind whose bound was exceeded.
        kind: u16,
    },
    /// Byte-valid canonical rules that this build cannot evaluate.
    RulesNotSupported,
}

/// Why the bytes cannot be any legal file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorruptReason {
    /// Fewer than 32 header bytes.
    TruncatedHeader,
    /// The header CRC did not match.
    HeaderCrc,
    /// A reserved header flag bit was set.
    HeaderFlags,
    /// `payload_len` disagreed with the actual body length.
    PayloadLenMismatch,
    /// A block length ran outside the body region.
    BlockBounds,
    /// A block CRC did not match.
    BlockCrc,
    /// The trailer hash did not match.
    TrailerSha,
    /// Field tags were not strictly ascending.
    FieldOrder,
    /// `field_count` disagreed with the parsed field stream.
    FieldCount,
    /// A string was not valid UTF-8, or contained a NUL byte.
    ///
    /// §9.2 classes this CORRUPT. Named separately from [`Self::FieldCount`] because
    /// reporting a bad string as a field-count mismatch sends an operator looking for
    /// the wrong defect.
    InvalidString,
    /// A stored public checkpoint slot has an invalid slug or ordinal.
    InvalidCheckpointLocation,
    /// An extension key violated its grammar or limits.
    InvalidExtensionKey,
    /// A revision was zero.
    ZeroRevision {
        /// The value that appeared.
        value: u64,
    },
    /// `event_count` disagreed with the parsed event stream.
    EventCount,
    /// A transaction carried no events.
    ZeroEvents,
    /// A field was encoded with length zero, which no registered type allows.
    ZeroLengthField {
        /// The offending field tag.
        tag: u16,
    },
    /// A record key's kind contradicted its event.
    RecordKeyKindMismatch {
        /// The event tag.
        event: u16,
        /// The kind that appeared.
        key_kind: u16,
    },
    /// `recorded_by` contradicted `actor_origin`.
    ActorOriginMismatch,
    /// A revision was not the predecessor plus one.
    RevisionNotMonotonic {
        /// The revision that was required.
        expected: u64,
        /// The revision that appeared.
        found: u64,
    },
    /// A required field for this event was absent.
    MissingField {
        /// The event tag.
        event: u16,
        /// The missing field tag.
        field: u16,
    },
    /// A field this event may not carry was present.
    ForbiddenField {
        /// The event tag.
        event: u16,
        /// The forbidden field tag.
        field: u16,
    },
    /// The status field contradicted the event kind.
    StatusMismatch {
        /// The event tag.
        event: u16,
    },
    /// A revision was zero, or otherwise impossible for the event that carried it.
    WrongRevision {
        /// The event tag; `0` when the inconsistency is not tied to one event.
        event: u16,
        /// The revision present.
        revision: u64,
    },
    /// A manifest and the transaction it references disagreed on a shared field.
    ManifestTxMismatch,
    /// A non-genesis transaction carried the wrong number of outcomes.
    /// A **non-genesis** transaction carried the wrong number of outcomes.
    ///
    /// Exactly one `RequestOutcomeRecorded` is required per ordinary transaction and
    /// none is allowed in genesis. Genesis is identified by `version == 1`, so a
    /// genesis failure is NOT this variant: reporting "expected 1, found 0" for a
    /// genesis transaction would describe a rule that genesis is exempt from. Use
    /// [`Self::GenesisShape`] for shape violations inside genesis.
    ///
    /// `found` is the number of `RequestOutcomeRecorded` events actually present -
    /// not an event index, not `event_count`. A transaction with two outcomes reports
    /// `found: 2`; reporting `3` because two outcomes were the 2nd and 3rd events
    /// would describe the frame's length rather than the rule that was broken.
    OutcomeCount {
        /// How many `RequestOutcomeRecorded` events were present.
        found: u16,
    },
    /// A `version == 1` transaction did not have the exact genesis shape.
    ///
    /// §4.3 fixes genesis as precisely one `Genesis` event followed by nine
    /// `IdAllocation` events, one per space 0..=8 in ascending order, each with
    /// `high_water_mark = 0`. Anything else is corrupt, and the reason is the shape -
    /// not an outcome count, which genesis is exempt from entirely.
    GenesisShape {
        /// Which shape rule failed.
        rule: GenesisShapeRule,
    },
    /// An outcome field disagreed with the transaction header.
    OutcomeHeaderMismatch {
        /// The field tag involved.
        tag: u16,
    },
    /// `version`, `request_canon_version` and the payload sentinel disagreed.
    RequestCanonMismatch,
    /// A blob's bytes did not hash to the reference that named it.
    RulesRefHashMismatch,
    /// Canonical rules bytes did not match the recorded digest.
    RulesDigestMismatch,
    /// The rules blob described a different profile than the header claimed.
    RulesProfileMismatch,
    /// Rules bytes violated their own canonical form.
    RulesEncodingInvalid,
    /// A recomputed domain digest did not match the recorded value.
    DigestMismatch {
        /// The domain whose digest disagreed.
        domain: u16,
    },
    /// The folded record set held a key twice.
    DuplicateRecordKey {
        /// The state kind that repeated.
        kind: u16,
    },
    /// An engine-generated path name violated its grammar.
    PathName(u16),
    /// A transaction id disagreed with the name that referenced it.
    TxIdMismatch,
}

/// Which genesis shape rule a `version == 1` transaction broke.
///
/// One variant per normative clause of §4.3, so an operator can tell "wrong number of
/// events" from "counters not zero" without reading the transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenesisShapeRule {
    /// The event count is not exactly 10 (one Genesis plus nine allocations).
    EventCount {
        /// Events that were present.
        found: u16,
    },
    /// No `Genesis` event at the head of the transaction.
    MissingGenesis,
    /// An `IdAllocation` event appeared before the `Genesis` event.
    GenesisNotFirst,
    /// An allocation event's space was not in ascending 0..=8 order.
    AllocationOrder {
        /// The space that appeared out of order.
        space: u16,
    },
    /// An allocation event's space appeared twice.
    DuplicateSpace {
        /// The repeated space.
        space: u16,
    },
    /// An allocation event's high-water mark was not zero.
    ///
    /// §4.3: the initial mark is 0. A genesis claiming an advanced counter would make
    /// the `bookkeeping_digest` disagree with every implementation that folds from an
    /// empty KB.
    NonZeroWatermark {
        /// The space that carried a non-zero mark.
        space: u16,
        /// The mark that appeared.
        hwm: u128,
    },
    /// A space in `0..=8` had no allocation event.
    MissingSpace {
        /// The space with no event.
        space: u16,
    },
}

/// Which ceiling an input exceeded.
///
/// Over-limit is not damage and not a foreign version: the structure is what it claims
/// to be, and this build refuses to admit it. Nothing is truncated or partially served.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitKind {
    /// Whole transaction file.
    TxFile,
    /// Whole manifest file.
    ManifestFile,
    /// Whole `CURRENT` file.
    CurrentFile,
    /// One block's raw length.
    BlockRaw,
    /// One block's encoded length.
    BlockEncoded,
    /// One string field.
    String {
        /// The field tag, when the decoder that refused knows it.
        ///
        /// `None` rather than a fabricated `0`: the wire-level length reader
        /// ([`crate::framing`] callers see `DecodeError::StringTooLong`) does not
        /// carry the tag, and inventing one would make two different fields report
        /// the same diagnostic. A family decoder that knows its tag fills it in.
        tag: Option<u16>,
        /// The length that appeared.
        len: u32,
        /// The field's declared maximum.
        max: u32,
    },
    /// Fields in one event.
    EventFields,
    /// Events in one transaction.
    EventsPerTx,
    /// Fields in a transaction or manifest header.
    HeaderFields,
    /// Items in one list or map field.
    ListItems {
        /// The field tag, when the decoder that refused knows it.
        tag: Option<u16>,
        /// The count that appeared.
        count: u32,
        /// The collection's declared maximum.
        max: u32,
    },
    /// A generated file name.
    Name,
}

/// This build's own invariant, not the caller's input.
///
/// Never reported as one of the other three classes: an internal breakage must be
/// logged as a defect, not turned into "your disk is damaged" or "migrate".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InternalReason {
    /// An encoder produced an image its own decoder refuses.
    EncodeDecodeDivergence,
    /// A required field was missing after the policy check accepted the container.
    ///
    /// `event` is `None` for a transaction or manifest **header**, which is a field
    /// container but not an event: there is no event tag to name, and using `0` for it
    /// would look like a real tag in a diagnostic.
    MissingRequired {
        /// The event tag, when the container is an event.
        event: Option<u16>,
        /// The field tag.
        tag: u16,
    },
    /// A real request digest came out all-zero, which is the genesis sentinel.
    SentinelCollision,
}

/// The four byte-level classifications of `storage-format.md` §9.
///
/// Exactly four, and no more: adding a fifth would need a fifth operator action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// Legal file, unsupported version, kind or feature.
    UnsupportedFormat(UnsupportedReason),
    /// Cannot be any legal file.
    Corrupt(CorruptReason),
    /// Structurally valid but over a declared ceiling.
    Limit(LimitKind),
    /// This build broke its own invariant.
    Internal(InternalReason),
}

/// A refusal that says nothing about the bytes.
///
/// Kept apart from [`FormatError`] so "out of budget" can never be read as "the file
/// is damaged". Each variant keeps its own remedy: retry bigger, resubmit, or log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// This attempt's budget ran out. Retry with more allowance or a smaller request.
    Budget(BudgetError),
    /// The caller asked to stop. Not an error.
    Stopped(ControlStop),
    /// The allocator could not provide the space. Nothing was produced.
    Allocation {
        /// Bytes that were needed.
        requested: usize,
    },
}

impl From<BudgetError> for Refusal {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ControlStop> for Refusal {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// A format operation's complete result: bytes verdict, or a non-verdict refusal.
///
/// Nothing is lossy here. Converting a budget refusal into a [`FormatError`] would
/// discard which budget and which unit ran out; converting cancellation into one
/// would report a caller's choice as a damaged file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatOutcome<T> {
    /// The operation completed.
    Ok(T),
    /// The bytes were classified.
    Format(FormatError),
    /// The operation did not complete for a reason that is not a bytes verdict.
    Refused(Refusal),
}

impl<T> FormatOutcome<T> {
    /// Whether the operation completed.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// The classification, when there was one.
    ///
    /// `None` for success and for refusals, because a refusal is not a classification.
    #[must_use]
    pub const fn as_format_error(&self) -> Option<FormatError> {
        match self {
            Self::Format(error) => Some(*error),
            _ => None,
        }
    }

    /// The refusal, when the operation did not complete for a non-verdict reason.
    #[must_use]
    pub const fn as_refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused(refusal) => Some(*refusal),
            _ => None,
        }
    }

    /// Keep this outcome's classification or refusal, replacing a success with `f`.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns; this is the sequencing form that does not re-wrap.
    pub fn and_then<U>(self, f: impl FnOnce(T) -> Result<U, FormatError>) -> FormatOutcome<U> {
        match self {
            Self::Ok(value) => match f(value) {
                Result::Ok(next) => FormatOutcome::Ok(next),
                Err(error) => FormatOutcome::Format(error),
            },
            Self::Format(error) => FormatOutcome::Format(error),
            Self::Refused(refusal) => FormatOutcome::Refused(refusal),
        }
    }

    /// The value, when the operation completed.
    ///
    /// # Errors
    ///
    /// The classification or refusal otherwise, unchanged.
    pub fn into_result(self) -> Result<T, Self> {
        match self {
            Self::Ok(value) => Ok(value),
            other => Err(other),
        }
    }
}

/// What the caller should do about an outcome.
///
/// One action per class. Refusals map to their own actions, never to
/// [`OperatorAction::RestoreBackup`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OperatorAction {
    /// No action; the operation completed.
    Continue,
    /// Bytes cannot be any legal file: stop serving this KB, restore from backup.
    RestoreBackup,
    /// Legal file this build does not support: migrate, and do not retry.
    Migrate,
    /// Over a declared ceiling: reject the input; never truncate and serve it.
    RejectInput,
    /// This build broke an invariant: log a defect.
    ReportDefect,
    /// Budget ran out: retry with more allowance or a smaller request.
    RetryWithBudget,
    /// The caller stopped us: resubmit or abandon; not an error.
    AbandonOrRetry,
}

/// The operator action implied by an outcome.
#[must_use]
pub fn classify<T>(outcome: &FormatOutcome<T>) -> OperatorAction {
    match outcome {
        FormatOutcome::Ok(_) => OperatorAction::Continue,
        FormatOutcome::Format(FormatError::Corrupt(_)) => OperatorAction::RestoreBackup,
        FormatOutcome::Format(FormatError::UnsupportedFormat(_)) => OperatorAction::Migrate,
        FormatOutcome::Format(FormatError::Limit(_)) => OperatorAction::RejectInput,
        FormatOutcome::Format(FormatError::Internal(_)) => OperatorAction::ReportDefect,
        FormatOutcome::Refused(Refusal::Budget(_)) => OperatorAction::RetryWithBudget,
        FormatOutcome::Refused(Refusal::Stopped(_)) => OperatorAction::AbandonOrRetry,
        FormatOutcome::Refused(Refusal::Allocation { .. }) => OperatorAction::RejectInput,
    }
}

/// The admission ceilings of format major 0.
///
/// Values are the normative ones from `storage-format.md` §1.3. A caller may
/// configure a **smaller** limit, never a larger one: the protocol maxima are
/// properties of the format, not of an installation. [`decode_current_bound`] is the
/// one place where that interaction is computed, so a raised `max_name_bytes` cannot
/// manufacture a larger exemption.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Whole transaction file.
    pub max_tx_file_bytes: u64,
    /// Whole manifest file.
    pub max_manifest_file_bytes: u64,
    /// Whole `CURRENT` file.
    pub max_current_file_bytes: u64,
    /// One block's raw length, before decompression.
    pub max_block_raw_bytes: u64,
    /// One block's encoded length, on disk.
    pub max_block_encoded_bytes: u64,
    /// One string field's bytes.
    pub max_string_bytes: u32,
    /// Fields in one event.
    pub max_event_fields: u16,
    /// Events in one transaction.
    pub max_events_per_tx: u32,
    /// Fields in a transaction or manifest header.
    pub max_header_fields: u16,
    /// Items in one list or map field.
    pub max_list_items: u32,
    /// A generated file name.
    pub max_name_bytes: u16,
}

impl Limits {
    /// The protocol maximum of the name component, in bytes.
    ///
    /// Fixed by the layout: a manifest or `CURRENT` name is at most 64 bytes, so a
    /// configuration value above it has no effect.
    pub const PROTOCOL_MAX_NAME_BYTES: u16 = 64;

    /// The normative ceilings of format major 0.
    pub const V1: Self = Self {
        max_tx_file_bytes: 64 * 1024 * 1024,
        max_manifest_file_bytes: 8 * 1024 * 1024,
        max_current_file_bytes: 4096,
        max_block_raw_bytes: 16 * 1024 * 1024,
        max_block_encoded_bytes: 64 * 1024 * 1024,
        max_string_bytes: 1024 * 1024,
        max_event_fields: 64,
        max_events_per_tx: 65_536,
        max_header_fields: 32,
        max_list_items: 65_536,
        max_name_bytes: Self::PROTOCOL_MAX_NAME_BYTES,
    };

    /// The effective name ceiling: configuration can only lower the protocol's cap.
    #[must_use]
    pub const fn effective_name_bytes(&self) -> u16 {
        if self.max_name_bytes < Self::PROTOCOL_MAX_NAME_BYTES {
            self.max_name_bytes
        } else {
            Self::PROTOCOL_MAX_NAME_BYTES
        }
    }
}

/// The effective `CURRENT` file ceiling for these limits.
///
/// `CURRENT` is `74 + name_len` bytes (§3), so the largest possible file is
/// `74 + effective_name_bytes`. The answer is the smaller of that and the configured
/// file ceiling: a caller who raises `max_name_bytes` to 65535 must not thereby
/// obtain a 65 609-byte `CURRENT` exemption, and a caller who lowers
/// `max_current_file_bytes` must not be overruled by the arithmetic bound.
#[must_use]
pub const fn decode_current_bound(limits: &Limits) -> u64 {
    let arithmetic = 74 + limits.effective_name_bytes() as u64;
    if limits.max_current_file_bytes < arithmetic {
        limits.max_current_file_bytes
    } else {
        arithmetic
    }
}

/// A destination for canonical bytes produced during a decode.
///
/// Bytes are charged to the caller's ledger **before** they are handed over, so a
/// scanner cannot grow a caller's buffer past its allowance. The sink receives the
/// ledger rather than owning a counter: one budget, not two.
pub trait BytesSink {
    /// Accept one region of decoded bytes.
    ///
    /// # Errors
    ///
    /// [`Refusal`] when the ledger refuses the admission.
    fn write(&mut self, bytes: &[u8], ledger: &mut dyn SharedLedger) -> Result<(), Refusal>;
}

/// A destination for the blob references one event declares, in field-tag order.
///
/// Deliberately a sink and not a returned `Vec`: enumerating a whole chain through
/// an owned collection would allocate once per event for a value the caller usually
/// only counts or forwards.
pub trait BlobRefSink {
    /// Accept one reference.
    ///
    /// # Errors
    ///
    /// [`Refusal`] when the ledger refuses the admission.
    fn push(
        &mut self,
        tag: u16,
        sha256: [u8; 32],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal>;
}

/// Discards every byte, charging the ledger as if it were retained.
///
/// Useful for a validating pass that must still observe the same budget refusal as a
/// retaining pass; a sink that skipped the charge would make the two disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiscardBytes;

impl BytesSink for DiscardBytes {
    fn write(&mut self, bytes: &[u8], ledger: &mut dyn SharedLedger) -> Result<(), Refusal> {
        ledger.try_charge(bytes.len() as u64)?;
        Ok(())
    }
}

/// Counts references and keeps the last one, without allocating.
///
/// A counting sink is what a "does this event reference anything" check needs; it is
/// not a substitute for a call site that must actually retain the references.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CountBlobRefs {
    count: u64,
    last: Option<(u16, [u8; 32])>,
}

impl CountBlobRefs {
    /// How many references were accepted.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The most recently accepted reference, for diagnostics.
    #[must_use]
    pub const fn last(&self) -> Option<(u16, [u8; 32])> {
        self.last
    }
}

impl BlobRefSink for CountBlobRefs {
    fn push(
        &mut self,
        tag: u16,
        sha256: [u8; 32],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        ledger.try_charge_entries(1)?;
        self.count += 1;
        self.last = Some((tag, sha256));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_bound_is_the_minimum_of_arithmetic_and_configuration() {
        let mut limits = Limits::V1;
        assert_eq!(decode_current_bound(&limits), 74 + 64);
        limits.max_name_bytes = u16::MAX;
        assert_eq!(decode_current_bound(&limits), 138, "64-byte cap wins");
        limits.max_name_bytes = 10;
        assert_eq!(decode_current_bound(&limits), 84);
        limits.max_current_file_bytes = 80;
        assert_eq!(decode_current_bound(&limits), 80, "smaller file cap wins");
    }

    #[test]
    fn a_refusal_is_never_reported_as_restore_or_migrate() {
        let stopped: FormatOutcome<u8> =
            FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled));
        assert_eq!(classify(&stopped), OperatorAction::AbandonOrRetry);
        assert!(
            stopped.as_format_error().is_none(),
            "a stop is not a verdict"
        );
        let corrupt: FormatOutcome<u8> =
            FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderCrc));
        assert_eq!(classify(&corrupt), OperatorAction::RestoreBackup);
        assert!(corrupt.as_refusal().is_none());
    }

    fn outcome_of(error: FormatError) -> FormatOutcome<()> {
        FormatOutcome::Format(error)
    }

    /// A ledger that admits everything, so a sink test exercises the sink.
    struct Admit;

    impl SharedLedger for Admit {
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

    #[test]
    fn outcome_count_is_a_count_not_an_event_index() {
        // Two outcomes as events 3 and 4 of a transaction: the operator question is
        // "how many outcomes", so the answer must be 2. Reporting the index would name
        // the frame's length instead of the rule that broke.
        let error = FormatError::Corrupt(CorruptReason::OutcomeCount { found: 2 });
        let rendered = format!("{error:?}");
        assert!(rendered.contains("found: 2"), "got {rendered}");
    }

    #[test]
    fn a_genesis_shape_failure_is_not_an_outcome_count() {
        // Genesis is exempt from the exactly-one-outcome rule, so its failures must not
        // be described by that rule. Each shape clause has its own reason.
        let cases = [
            GenesisShapeRule::EventCount { found: 9 },
            GenesisShapeRule::MissingGenesis,
            GenesisShapeRule::GenesisNotFirst,
            GenesisShapeRule::AllocationOrder { space: 4 },
            GenesisShapeRule::DuplicateSpace { space: 2 },
            GenesisShapeRule::NonZeroWatermark {
                space: 3,
                hwm: u128::MAX,
            },
            GenesisShapeRule::MissingSpace { space: 8 },
        ];
        for rule in cases {
            let error = FormatError::Corrupt(CorruptReason::GenesisShape { rule });
            assert_eq!(classify(&outcome_of(error)), OperatorAction::RestoreBackup);
        }
    }

    #[test]
    fn a_counting_sink_charges_entries_not_bytes() {
        let mut sink = CountBlobRefs::default();
        let mut led = Admit;
        sink.push(23, [7; 32], &mut led).expect("admitted");
        assert_eq!(sink.count(), 1);
        assert_eq!(sink.last(), Some((23, [7; 32])));
    }
}
