//! `bookkeeping_digest` assembly: `storage-digests.md` §4.6.
//!
//! This domain exists because of the acyclicity requirement, not for tidiness:
//! `RequestOutcome` carries `payload_digest`, so putting it inside `state_digest` would
//! make `payload_digest`'s canonical bytes reference a digest that references it. Split
//! out, `state_digest` and `bookkeeping_digest` are parallel: both are published by the
//! manifest and neither reads the other.
//!
//! Three sets, three natural keys, and two count/width spaces that are easy to conflate:
//!
//! | set | order | note |
//! |---|---|---|
//! | outcomes | `(principal_id, request_id)` | the idempotency key, identity included |
//! | imports | `import_session_id` | one row per stage event; a stage is immutable |
//! | counters | `space` | `high_water_mark` is **`u128`**, matching tag 165 |
//!
//! Two deliberate exclusions: `extensions` (tag 14) is a record value the format must
//! preserve, but §4.6's `Outcome`/`Import` rows do not carry it, so it is covered by
//! `state_digest`'s record bodies and not here; and `result_kind` is written as the
//! constant `committed` because major 0 has no other legal value (§6.2 tag 179), which is
//! why the record types carry no field for it.

use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{CountPair, ImportSessionRecord, RequestOutcomeEvent, Watermark};

use crate::digest::{Domain, digest};
use crate::state::{DigestAssemblyError, StructureDefect};

/// `result_kind` in major 0 is only `0 committed` (`storage-format.md` §6.2, tag 179).
const RESULT_KIND_COMMITTED: u16 = 0;

/// `expected_counts` bound (`storage-format.md` §6.2, tag 186).
const MAX_EXPECTED_COUNTS: usize = 32;

/// Encode the §4.6 payload into `out`, which must be empty.
///
/// # Errors
///
/// [`DigestAssemblyError`]; on any error `out` is left empty.
pub fn encode_bookkeeping_payload(
    outcomes: &[RequestOutcomeEvent],
    imports: &[ImportSessionRecord],
    counters: &[Watermark],
    out: &mut Vec<u8>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), DigestAssemblyError> {
    ledger.checkpoint()?;
    if !out.is_empty() {
        return Err(StructureDefect::BufferNotEmpty.error());
    }

    // Pass 1: validate in place and measure. No temporary row buffers, so the extent is
    // exact and the single admission below is the only one for the payload.
    let mut total = 4usize;
    let mut previous: Option<(&str, &str)> = None;
    for (index, outcome) in outcomes.iter().enumerate() {
        ledger.checkpoint()?;
        if outcome.principal_id.is_empty() {
            return Err(StructureDefect::EmptyIdentity {
                index,
                field: "principal_id",
            }
            .error());
        }
        if outcome.request_id.is_empty() {
            return Err(StructureDefect::EmptyIdentity {
                index,
                field: "request_id",
            }
            .error());
        }
        if outcome.payload_digest == [0u8; 32] {
            return Err(StructureDefect::SentinelPayloadDigest { index }.error());
        }
        let key = (outcome.principal_id.as_str(), outcome.request_id.as_str());
        if previous.is_some_and(|prev| key <= prev) {
            return Err(StructureDefect::OutcomesNotAscending { index }.error());
        }
        previous = Some(key);
        total = total
            .checked_add(
                str_bytes(&outcome.principal_id)
                    + str_bytes(&outcome.request_id)
                    + 32 // payload_digest
                    + 2  // result_kind
                    + 32 // result_ref
                    + 8, // commit_version
            )
            .ok_or(StructureDefect::TooLarge.error())?;
    }

    total = total
        .checked_add(4)
        .ok_or(StructureDefect::TooLarge.error())?;
    let mut previous_session: Option<&str> = None;
    for (index, import) in imports.iter().enumerate() {
        ledger.checkpoint()?;
        if import.import_session_id.is_empty() {
            return Err(StructureDefect::EmptyIdentity {
                index,
                field: "import_session_id",
            }
            .error());
        }
        if previous_session.is_some_and(|prev| import.import_session_id.as_str() <= prev) {
            return Err(StructureDefect::ImportsNotAscending { index }.error());
        }
        previous_session = Some(import.import_session_id.as_str());
        if import.expected_counts.len() > MAX_EXPECTED_COUNTS {
            return Err(StructureDefect::ExpectedCountsTooLong { index }.error());
        }
        let mut previous_kind = None;
        for pair in &import.expected_counts {
            if previous_kind.is_some_and(|prev| pair.kind.as_u16() <= prev) {
                return Err(StructureDefect::ExpectedCountsNotAscending { index }.error());
            }
            previous_kind = Some(pair.kind.as_u16());
        }
        total = total
            .checked_add(
                str_bytes(&import.import_session_id)
                    + 2                                // stage
                    + 32                               // source_digest
                    + 32                               // plan_ref
                    + 4                                // expected_counts count
                    + import.expected_counts.len() * 10 // kind:u16 | count:u64
                    + 1, // import_complete
            )
            .ok_or(StructureDefect::TooLarge.error())?;
    }

    total = total
        .checked_add(4)
        .ok_or(StructureDefect::TooLarge.error())?;
    let mut previous_space: Option<u16> = None;
    for (index, counter) in counters.iter().enumerate() {
        ledger.checkpoint()?;
        if previous_space.is_some_and(|prev| counter.space.as_u16() <= prev) {
            return Err(StructureDefect::CountersNotAscending { index }.error());
        }
        previous_space = Some(counter.space.as_u16());
        if !counter.is_in_range() {
            return Err(StructureDefect::WatermarkBeyondSpace { index }.error());
        }
        total = total
            .checked_add(2 + 16) // space:u16 | high_water_mark:u128
            .ok_or(StructureDefect::TooLarge.error())?;
    }

    let outcome_count =
        u32::try_from(outcomes.len()).map_err(|_| StructureDefect::TooLarge.error())?;
    let import_count =
        u32::try_from(imports.len()).map_err(|_| StructureDefect::TooLarge.error())?;
    let counter_count =
        u32::try_from(counters.len()).map_err(|_| StructureDefect::TooLarge.error())?;

    ledger.try_charge(total as u64)?;
    out.try_reserve_exact(total)
        .map_err(|_| DigestAssemblyError::Allocation { requested: total })?;

    out.extend_from_slice(&outcome_count.to_le_bytes());
    for outcome in outcomes {
        out.extend_from_slice(&(outcome.principal_id.len() as u32).to_le_bytes());
        out.extend_from_slice(outcome.principal_id.as_bytes());
        out.extend_from_slice(&(outcome.request_id.len() as u32).to_le_bytes());
        out.extend_from_slice(outcome.request_id.as_bytes());
        out.extend_from_slice(&outcome.payload_digest);
        out.extend_from_slice(&RESULT_KIND_COMMITTED.to_le_bytes());
        out.extend_from_slice(&outcome.result_ref);
        out.extend_from_slice(&outcome.commit_version.to_le_bytes());
    }
    out.extend_from_slice(&import_count.to_le_bytes());
    for import in imports {
        out.extend_from_slice(&(import.import_session_id.len() as u32).to_le_bytes());
        out.extend_from_slice(import.import_session_id.as_bytes());
        out.extend_from_slice(&import.stage.as_u16().to_le_bytes());
        out.extend_from_slice(&import.source_digest);
        out.extend_from_slice(&import.plan_ref);
        out.extend_from_slice(&(import.expected_counts.len() as u32).to_le_bytes());
        for pair in &import.expected_counts {
            write_count_pair(out, pair);
        }
        out.push(u8::from(import.import_complete));
    }
    out.extend_from_slice(&counter_count.to_le_bytes());
    for counter in counters {
        out.extend_from_slice(&counter.space.as_u16().to_le_bytes());
        out.extend_from_slice(&counter.hwm.to_le_bytes());
    }
    debug_assert_eq!(out.len(), total);
    Ok(())
}

fn write_count_pair(out: &mut Vec<u8>, pair: &CountPair) {
    out.extend_from_slice(&pair.kind.as_u16().to_le_bytes());
    out.extend_from_slice(&pair.count.to_le_bytes());
}

/// Bytes a string contributes: `u32` length prefix + UTF-8 bytes.
const fn str_bytes(value: &str) -> usize {
    4 + value.len()
}

/// `bookkeeping_digest` over the §4.6 projection.
///
/// # Errors
///
/// As [`encode_bookkeeping_payload`].
pub fn bookkeeping_digest(
    outcomes: &[RequestOutcomeEvent],
    imports: &[ImportSessionRecord],
    counters: &[Watermark],
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], DigestAssemblyError> {
    let mut payload = Vec::new();
    encode_bookkeeping_payload(outcomes, imports, counters, &mut payload, ledger)?;
    ledger.checkpoint()?;
    Ok(digest(Domain::Bookkeeping, &payload))
}
