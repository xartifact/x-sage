//! Operation 9 v1 (`NativeImportStep`) payload codec.
//!
//! # What this is
//!
//! The canonical bytes of one administrative import step request, carried as the
//! single operation of a real P1 `RequestFrame`. This module owns the layout and its
//! validation; it does **not** frame, hash or identify the request. The frame, the
//! `request`-domain digest and the idempotency key come from the existing P1 codec
//! (`sage_db_format::request`), so there is no second framing authority and no second
//! digest.
//!
//! # Layout (little-endian, no padding)
//!
//! ```text
//! NativeImportStep v1 payload
//!   package_ref       [32]     package blob digest = ImportSession.plan_ref
//!   source_digest     [32]     digest of the imported source material
//!   stage             u16      0 Prepare, 1 Validate, 2 Import, 3 Verify, 4 Activate
//!   batch_index       u32      zero-based index of this batch within the stage
//!   batch_count       u32      total batches in this stage (>= 1)
//!   first_event       u32      index of the first mapped event this batch carries
//!   event_count       u32      how many mapped events it carries (0 is legal for an
//!                              empty final batch)
//!   import_session_id str      u32 UTF-8 byte length, then exactly those bytes
//! ```
//!
//! The read set is empty: an import step reads no record baseline, and the whole-KB
//! expectation is not part of this payload. The executor derives it from the actual
//! publication (see `execute`), so it is a preparing precondition rather than a
//! caller-controlled identity field.
//!
//! # Why the ranges are validated here
//!
//! `batch_count >= 1`, `batch_index < batch_count`, and `first_event + event_count`
//! must not overflow `u32`. A payload that fails those is not a request that could
//! ever be honoured, so it is refused before any allocation rather than being decoded
//! into a value the executor would have to re-police.

use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::ImportStage;

/// Operation kind of `NativeImportStep`.
pub const NATIVE_IMPORT_STEP: u16 = 9;
/// The only payload version this module registers.
pub const OPERATION_VERSION: u16 = 1;

/// Bytes of a digest field.
const DIGEST_BYTES: usize = 32;
/// Bytes of the fixed field block before the session string.
const FIXED_BYTES: usize = DIGEST_BYTES + DIGEST_BYTES + 2 + 4 + 4 + 4 + 4;
/// Bytes of a `str` length prefix.
const LEN_BYTES: usize = 4;
/// The registered operation ceiling, imported rather than restated.
use sage_db_store::typed::MAX_PAYLOAD;

/// One import step as the executor states it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImportStep<'a> {
    /// The package blob this session was prepared from; becomes `ImportSession.plan_ref`.
    pub package_ref: [u8; 32],
    /// Digest of the imported source material.
    pub source_digest: [u8; 32],
    /// Which stage this step records.
    pub stage: ImportStage,
    /// Zero-based batch number within the stage.
    pub batch_index: u32,
    /// Total batches in the stage; `>= 1`.
    pub batch_count: u32,
    /// Index of the first mapped event this batch carries.
    pub first_event: u32,
    /// How many mapped events this batch carries.
    pub event_count: u32,
    /// The import session this step belongs to.
    pub import_session_id: &'a str,
}

/// Why an import-step payload is not acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepPayloadError {
    /// `batch_count` was zero, so no batch could ever be selected.
    ZeroBatchCount,
    /// `batch_index` was not below `batch_count`.
    BatchIndexOutOfRange {
        /// Supplied index.
        index: u32,
        /// Supplied count.
        count: u32,
    },
    /// `first_event + event_count` overflowed `u32`.
    EventRangeOverflow {
        /// Supplied first index.
        first: u32,
        /// Supplied count.
        count: u32,
    },
    /// A stage value outside the canonical `0..=4` domain.
    UnknownStage(u16),
    /// The payload ended before a declared value did.
    Truncated,
    /// Bytes remained after the last declared value.
    Trailing,
    /// A length-prefixed value is not valid UTF-8.
    NotUtf8,
    /// The payload exceeds the registered operation ceiling.
    TooLarge {
        /// Measured length.
        len: u64,
        /// The ceiling.
        max: u32,
    },
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
}

/// Validate the value-level domains before any byte is produced.
fn check(step: &ImportStep<'_>) -> Result<(), StepPayloadError> {
    if step.batch_count == 0 {
        return Err(StepPayloadError::ZeroBatchCount);
    }
    if step.batch_index >= step.batch_count {
        return Err(StepPayloadError::BatchIndexOutOfRange {
            index: step.batch_index,
            count: step.batch_count,
        });
    }
    if step.first_event.checked_add(step.event_count).is_none() {
        return Err(StepPayloadError::EventRangeOverflow {
            first: step.first_event,
            count: step.event_count,
        });
    }
    Ok(())
}

/// Encode a `NativeImportStep` v1 payload.
///
/// # Errors
///
/// [`StepPayloadError`] for a zero batch count, an out-of-range batch index, an
/// overflowing event range, a value that cannot carry its `u32` prefix, a payload over
/// the operation ceiling, or the caller's own refusal.
pub fn encode_step(
    step: &ImportStep<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, StepPayloadError> {
    check(step)?;
    let name_len =
        u32::try_from(step.import_session_id.len()).map_err(|_| StepPayloadError::TooLarge {
            len: u64::MAX,
            max: MAX_PAYLOAD,
        })?;
    let total = FIXED_BYTES
        .checked_add(LEN_BYTES)
        .and_then(|n| n.checked_add(step.import_session_id.len()))
        .ok_or(StepPayloadError::TooLarge {
            len: u64::MAX,
            max: MAX_PAYLOAD,
        })?;
    if total > MAX_PAYLOAD as usize {
        return Err(StepPayloadError::TooLarge {
            len: total as u64,
            max: MAX_PAYLOAD,
        });
    }
    ledger
        .checkpoint()
        .map_err(|stop| StepPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(total as u64)
        .map_err(|error| StepPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| StepPayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| StepPayloadError::Refused(Refusal::Allocation { requested: total }))?;
    out.extend_from_slice(&step.package_ref);
    out.extend_from_slice(&step.source_digest);
    out.extend_from_slice(&(step.stage as u16).to_le_bytes());
    out.extend_from_slice(&step.batch_index.to_le_bytes());
    out.extend_from_slice(&step.batch_count.to_le_bytes());
    out.extend_from_slice(&step.first_event.to_le_bytes());
    out.extend_from_slice(&step.event_count.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(step.import_session_id.as_bytes());
    Ok(out)
}

/// A decoded step payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedStep<'a> {
    /// The package blob digest.
    pub package_ref: [u8; 32],
    /// The source-material digest.
    pub source_digest: [u8; 32],
    /// The stage.
    pub stage: ImportStage,
    /// Zero-based batch number.
    pub batch_index: u32,
    /// Total batches in the stage.
    pub batch_count: u32,
    /// First mapped event index.
    pub first_event: u32,
    /// Mapped event count.
    pub event_count: u32,
    /// The session identity, borrowed.
    pub import_session_id: &'a str,
}

/// A bounds-checked cursor.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], StepPayloadError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(StepPayloadError::Truncated)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(StepPayloadError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, StepPayloadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, StepPayloadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn digest(&mut self) -> Result<[u8; 32], StepPayloadError> {
        let bytes = self.take(DIGEST_BYTES)?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(bytes);
        Ok(raw)
    }

    fn str(&mut self) -> Result<&'a str, StepPayloadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| StepPayloadError::NotUtf8)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Decode a `NativeImportStep` v1 payload.
///
/// Applies the same value-level domains as [`encode_step`], so an encoder cannot emit
/// bytes its own decoder refuses.
///
/// # Errors
///
/// [`StepPayloadError`] for a payload over the ceiling, a truncated or trailing
/// length, an unregistered stage, or a value-domain violation.
pub fn decode_step<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedStep<'a>, StepPayloadError> {
    if payload.len() as u64 > MAX_PAYLOAD as u64 {
        return Err(StepPayloadError::TooLarge {
            len: payload.len() as u64,
            max: MAX_PAYLOAD,
        });
    }
    // Decoding walks the payload once; charge that work rather than doing it for free.
    ledger
        .checkpoint()
        .map_err(|stop| StepPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(payload.len() as u64)
        .map_err(|error| StepPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| StepPayloadError::Refused(Refusal::Budget(error)))?;

    let mut cursor = Cursor::new(payload);
    let package_ref = cursor.digest()?;
    let source_digest = cursor.digest()?;
    let raw_stage = cursor.u16()?;
    let stage =
        ImportStage::from_u16(raw_stage).ok_or(StepPayloadError::UnknownStage(raw_stage))?;
    let batch_index = cursor.u32()?;
    let batch_count = cursor.u32()?;
    let first_event = cursor.u32()?;
    let event_count = cursor.u32()?;
    let import_session_id = cursor.str()?;
    if !cursor.at_end() {
        return Err(StepPayloadError::Trailing);
    }
    let step = ImportStep {
        package_ref,
        source_digest,
        stage,
        batch_index,
        batch_count,
        first_event,
        event_count,
        import_session_id,
    };
    check(&step)?;
    Ok(DecodedStep {
        package_ref,
        source_digest,
        stage,
        batch_index,
        batch_count,
        first_event,
        event_count,
        import_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetError;
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

    fn step() -> ImportStep<'static> {
        ImportStep {
            package_ref: [0x11; 32],
            source_digest: [0x22; 32],
            stage: ImportStage::Import,
            batch_index: 1,
            batch_count: 3,
            first_event: 10,
            event_count: 5,
            import_session_id: "session-1",
        }
    }

    #[test]
    fn round_trips_every_field() {
        let mut l = Open;
        let bytes = encode_step(&step(), &mut l).expect("encodes");
        let decoded = decode_step(&bytes, &mut l).expect("decodes");
        assert_eq!(decoded.package_ref, [0x11; 32]);
        assert_eq!(decoded.source_digest, [0x22; 32]);
        assert_eq!(decoded.stage, ImportStage::Import);
        assert_eq!(decoded.batch_index, 1);
        assert_eq!(decoded.batch_count, 3);
        assert_eq!(decoded.first_event, 10);
        assert_eq!(decoded.event_count, 5);
        assert_eq!(decoded.import_session_id, "session-1");
    }

    #[test]
    fn zero_batch_count_is_refused_in_both_directions() {
        let mut l = Open;
        let mut bad = step();
        bad.batch_count = 0;
        assert_eq!(
            encode_step(&bad, &mut l),
            Err(StepPayloadError::ZeroBatchCount)
        );
    }

    #[test]
    fn batch_index_must_be_below_the_count() {
        let mut l = Open;
        let mut bad = step();
        bad.batch_index = 3;
        assert_eq!(
            encode_step(&bad, &mut l),
            Err(StepPayloadError::BatchIndexOutOfRange { index: 3, count: 3 })
        );
    }

    #[test]
    fn an_overflowing_event_range_is_refused() {
        let mut l = Open;
        let mut bad = step();
        bad.first_event = u32::MAX;
        bad.event_count = 1;
        assert_eq!(
            encode_step(&bad, &mut l),
            Err(StepPayloadError::EventRangeOverflow {
                first: u32::MAX,
                count: 1
            })
        );
    }

    #[test]
    fn an_unregistered_stage_is_refused_on_decode() {
        let mut l = Open;
        let mut bytes = encode_step(&step(), &mut l).expect("encodes");
        // Overwrite the stage with 9, which is outside 0..=4.
        bytes[64] = 9;
        bytes[65] = 0;
        assert_eq!(
            decode_step(&bytes, &mut l),
            Err(StepPayloadError::UnknownStage(9))
        );
    }

    #[test]
    fn trailing_and_truncated_bytes_are_refused() {
        let mut l = Open;
        let bytes = encode_step(&step(), &mut l).expect("encodes");
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(decode_step(&long, &mut l), Err(StepPayloadError::Trailing));
        let short = &bytes[..bytes.len() - 1];
        assert_eq!(decode_step(short, &mut l), Err(StepPayloadError::Truncated));
    }
}
