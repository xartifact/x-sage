//! Canonical persisted result reader, storage-digests §4.2.
//!
//! A result is published only after every field and the complete byte extent validate.
//! It borrows the hash-verified blob; no record arrays, strings or temporary copies are
//! allocated. The byte ceiling is caller policy, separate from malformed bytes.
//! Entry work uses the one SharedLedger; borrowed scalar access is fixed-width and metered.
//! This reader establishes format validity, not blob authenticity or Store durability.
use crate::field::{read_record_key, read_revision, read_watermark};
use crate::wire::{ReadError, Reader};
use sage_db_types::{
    framing::Refusal,
    ledger::SharedLedger,
    persist::{RevPair, Watermark},
};

/// A result did not decode; no partial result is returned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultReadError {
    /// A wire or canonical scalar was invalid.
    Wire(ReadError),
    /// This format does not interpret the result kind.
    UnsupportedKind(u16),
    /// No committed result may name version zero.
    InvalidVersion,
    /// Affected revisions are not strictly ascending.
    AffectedOrder {
        /// First invalid zero-based row index.
        index: u32,
    },
    /// Allocation counters are not strictly ascending by space.
    CounterOrder {
        /// First invalid zero-based row index.
        index: u32,
    },
    /// A declared extent cannot be represented on this platform.
    ExtentOverflow,
    /// The caller's blob ceiling refused interpretation, not a data verdict.
    Ceiling {
        /// Actual byte extent.
        size: u64,
        /// Caller-supplied maximum.
        ceiling: u64,
    },
    /// Genuine caller budget/control refusal.
    Refused(Refusal),
}
impl From<ReadError> for ResultReadError {
    fn from(error: ReadError) -> Self {
        Self::Wire(error)
    }
}
fn step(ledger: &mut dyn SharedLedger) -> Result<(), ResultReadError> {
    ledger
        .checkpoint()
        .map_err(|e| ResultReadError::Refused(Refusal::Stopped(e)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|e| ResultReadError::Refused(Refusal::Budget(e)))?;
    Ok(())
}
fn exit(ledger: &mut dyn SharedLedger) -> Result<(), ResultReadError> {
    ledger
        .checkpoint()
        .map_err(|e| ResultReadError::Refused(Refusal::Stopped(e)))
}
fn extent(count: u32, width: usize) -> Result<usize, ResultReadError> {
    usize::try_from(count)
        .ok()
        .and_then(|n| n.checked_mul(width))
        .ok_or(ResultReadError::ExtentOverflow)
}
fn pair(reader: &mut Reader<'_>) -> Result<RevPair, ReadError> {
    Ok(RevPair {
        key: read_record_key(reader)?,
        revision: read_revision(reader)?,
    })
}
/// A complete immutable result view, borrowing its source blob.
#[derive(Clone, Copy, Debug)]
pub struct ResultPayload<'a> {
    version: u64,
    affected_count: u32,
    counter_count: u32,
    affected: &'a [u8],
    counters: &'a [u8],
}
impl ResultPayload<'_> {
    /// Original durable commit version recorded by the result.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
    /// Number of affected revision pairs, fully validated at construction.
    #[must_use]
    pub const fn affected_count(&self) -> u32 {
        self.affected_count
    }
    /// Number of advanced allocation counters, fully validated at construction.
    #[must_use]
    pub const fn counter_count(&self) -> u32 {
        self.counter_count
    }
    /// Read one affected pair with fixed-width work and no allocation.
    ///
    /// # Errors
    /// Actual caller refusal; canonical wire errors remain preserved if an invariant fails.
    pub fn affected_at(
        &self,
        index: u32,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<RevPair>, ResultReadError> {
        step(ledger)?;
        if index >= self.affected_count {
            exit(ledger)?;
            return Ok(None);
        }
        let start = extent(index, 26)?;
        let value = pair(&mut Reader::new(&self.affected[start..start + 26]))?;
        exit(ledger)?;
        Ok(Some(value))
    }
    /// Read one counter with fixed-width work and no allocation.
    ///
    /// # Errors
    /// Actual caller refusal; canonical wire errors remain preserved if an invariant fails.
    pub fn counter_at(
        &self,
        index: u32,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<Watermark>, ResultReadError> {
        step(ledger)?;
        if index >= self.counter_count {
            exit(ledger)?;
            return Ok(None);
        }
        let start = extent(index, 18)?;
        let value = read_watermark(&mut Reader::new(&self.counters[start..start + 18]))?;
        exit(ledger)?;
        Ok(Some(value))
    }
}
/// Read the entire canonical result payload, retaining only borrowed fixed-width spans.
///
/// Counts are checked against actual remaining wire bytes before their loops, then each
/// scalar row is charged and validated. No allocation or copying is performed; bytes are
/// already owned/admitted by the blob reader. Stop is checked at entry, every row and exit.
///
/// # Errors
/// Precise wire, kind, version, ordering, ceiling, extent and actual caller refusal.
pub fn read_result_payload<'a>(
    bytes: &'a [u8],
    max_bytes: u64,
    ledger: &mut dyn SharedLedger,
) -> Result<ResultPayload<'a>, ResultReadError> {
    step(ledger)?;
    let size = u64::try_from(bytes.len()).map_err(|_| ResultReadError::ExtentOverflow)?;
    if size > max_bytes {
        return Err(ResultReadError::Ceiling {
            size,
            ceiling: max_bytes,
        });
    }
    let mut reader = Reader::new(bytes);
    let kind = reader.u16()?;
    if kind != 0 {
        return Err(ResultReadError::UnsupportedKind(kind));
    }
    let version = reader.u64()?;
    if version == 0 {
        return Err(ResultReadError::InvalidVersion);
    }
    let affected_count = reader.u32()?;
    let affected = reader.take_exact(extent(affected_count, 26)?)?;
    // Read/check BOTH extents and trailing bytes before any declared-count loop.
    let counter_count = reader.u32()?;
    let counters = reader.take_exact(extent(counter_count, 18)?)?;
    reader.finish()?;
    let mut input = Reader::new(affected);
    let mut previous = None;
    for index in 0..affected_count {
        step(ledger)?;
        let value = pair(&mut input)?;
        if previous.is_some_and(|p| p >= value) {
            return Err(ResultReadError::AffectedOrder { index });
        }
        previous = Some(value);
    }
    let mut input = Reader::new(counters);
    let mut previous = None;
    for index in 0..counter_count {
        step(ledger)?;
        let value = read_watermark(&mut input)?;
        if previous.is_some_and(|p| p >= value.space) {
            return Err(ResultReadError::CounterOrder { index });
        }
        previous = Some(value.space);
    }
    exit(ledger)?;
    Ok(ResultPayload {
        version,
        affected_count,
        counter_count,
        affected,
        counters,
    })
}
