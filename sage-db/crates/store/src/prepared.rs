//! The canonical carrier → encoder header mapping, by move, plus the parts the commit path needs.
//!
//! `PreparedTransaction` is declared in `sage-db-types` (canonical). `TxHeaderToEncode` is declared in
//! `sage-db-format`. The conversion cannot live in either: `types` has no dependencies, so naming a format type
//! there would invert the crate edge, and an inherent impl on a foreign type is not expressible.
//!
//! So it lives here, in the consumer. It is a **physical field mapping**, not a serializer.
//!
//! ## Consumption returns the parts, it does not discard them
//!
//! An earlier revision destructured `events: _, blobs: _`, which silently dropped them — the commit path then had
//! nothing to encode or persist. The carrier is consumed, so the parts that are not header fields are **returned**
//! by move rather than dropped or cloned.
//!
//! ## What is derived
//!
//! `request_canon_version` is not a carrier field. Major 0 fixes it: `version == 1` is `NonRequest`, anything later
//! is `V1`. Deriving it here keeps the version as the single authority. The payload-digest sentinel join is a
//! validity check the encoder performs; doing it here would duplicate a rule rather than apply it.

use sage_db_format::encode::RequestCanonVersion;
pub use sage_db_format::encode::TxHeaderToEncode;
use sage_db_types::framing::Limits;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::prepared::{BlobDeclaration, OwnedEvent, PreparedTransaction};

/// The carrier's non-header parts, returned by move.
///
/// Deliberately a named struct rather than a tuple: the commit path uses these in a fixed order and a tuple would
/// make a swap compile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CarrierParts {
    /// Events in commit order, for the encoder's event path.
    pub events: Vec<OwnedEvent>,
    /// Blobs to make durable, for the persistence path.
    pub blobs: Vec<BlobDeclaration>,
    /// The rules blob address (record tag 166). Retained because it is a durability dependency in its own right:
    /// the transaction names it, so a commit that dropped it could publish a tx whose rules blob was never made
    /// durable.
    pub rules_ref: [u8; 32],
    /// The rules digest the transaction claims (header tag 163).
    pub rules_digest: [u8; 32],
    /// The full rules binding: ref, digest AND profile together.
    ///
    /// The verifier binds all three, so carrying them as one value is what stops a caller assembling a trio that
    /// no single declaration produced.
    pub rules: sage_db_types::prepared::RulesBinding,
}

/// Consume a carrier, yielding the encoder's header and the parts it does not carry.
///
/// `principal_id`, `request_id` and `extensions` are moved into the header; `events` and `blobs` are moved into
/// [`CarrierParts`]. Nothing is cloned and nothing is dropped.
#[must_use]
pub fn split_carrier(carrier: PreparedTransaction) -> (TxHeaderToEncode, CarrierParts) {
    let PreparedTransaction {
        version,
        prev_version,
        tx_id,
        kb_uuid,
        created_at,
        principal_id,
        request_id,
        payload_digest,
        expect_version,
        extensions,
        events,
        blobs,
        rules,
    } = carrier;

    let rules_ref_of_parts = |r: &sage_db_types::prepared::RulesBinding| r.rules_ref;
    let rules_ref_of_parts_digest = |r: &sage_db_types::prepared::RulesBinding| r.rules_digest;
    let header = TxHeaderToEncode {
        kb_uuid,
        version,
        tx_id,
        prev_version,
        created_at,
        principal_id,
        request_id,
        payload_digest,
        // The carrier holds the closed enum; the header takes the wire value.
        rules_profile: rules.profile.as_u16(),
        rules_digest: rules.rules_digest,
        // Derived, not carried: the version is the single authority in major 0.
        request_canon_version: if version == 1 {
            RequestCanonVersion::NonRequest
        } else {
            RequestCanonVersion::V1
        },
        expect_version,
        extensions,
    };
    let parts = CarrierParts {
        events,
        blobs,
        rules_ref: rules_ref_of_parts(&rules),
        rules_digest: rules_ref_of_parts_digest(&rules),
        rules,
    };
    (header, parts)
}

/// Header only, for a caller that has already taken the parts.
///
/// Kept as a separate entry point because a caller may legitimately need only the header (for a header-level
/// check) and should not have to reconstruct the carrier to get it.
#[must_use]
pub fn header_from_carrier(carrier: PreparedTransaction) -> TxHeaderToEncode {
    let (header, _parts) = split_carrier(carrier);
    header
}

/// The rules binding a carrier declares, borrowed before consumption.
///
/// The commit path needs `rules_ref` to prove the rules blob is durable, and taking it by reference avoids a copy
/// of the 32-byte digest array.
#[must_use]
pub const fn rules_ref_of(carrier: &PreparedTransaction) -> [u8; 32] {
    carrier.rules.rules_ref
}

/// Whether a carrier declares a blob by digest.
#[must_use]
pub fn declares_blob(carrier: &PreparedTransaction, sha256: &[u8; 32]) -> bool {
    carrier.blobs.iter().any(|b| &b.sha256 == sha256)
}

/// Encode a carrier's transaction image, returning the bytes and their file hash.
///
/// Consumes the carrier and delegates to the same encoder as persistence. The Store handle uses
/// [`encode_tx_image_borrowed`] with the header and events it already owns.
///
/// # Errors
///
/// The encoder's verdict or refusal, preserved.
pub fn encode_tx_image(
    carrier: PreparedTransaction,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), crate::commit::CommitRefusal> {
    let (header, parts) = split_carrier(carrier);
    encode_tx_image_ref(header, &parts.events, limits, ledger)
}

/// Encode an existing header and event storage by reference, returning their image and file hash.
///
/// The Store derives its transaction digest through this entry point, then moves the same owned header and parts into
/// persistence. Both borrowed and consuming entry points delegate to one encoder implementation.
///
/// # Errors
///
/// The encoder's format verdict or refusal, preserved.
pub fn encode_tx_image_borrowed(
    header: &TxHeaderToEncode,
    events: &[sage_db_types::prepared::OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), crate::commit::CommitRefusal> {
    encode_impl(header, events, limits, ledger)
}

/// Encode from an owned header and borrowed events.
///
/// # Errors
///
/// Whatever the format layer refuses.
pub fn encode_tx_image_ref(
    header: TxHeaderToEncode,
    events: &[sage_db_types::prepared::OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), crate::commit::CommitRefusal> {
    encode_impl(&header, events, limits, ledger)
}

/// The one encoding body. Both entry points reach it, so the bytes and digest cannot differ between them.
fn encode_impl(
    header: &TxHeaderToEncode,
    events: &[sage_db_types::prepared::OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), crate::commit::CommitRefusal> {
    let view = match sage_db_format::prepared::view_for_encode(header, events, limits, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(view) => view,
        sage_db_types::framing::FormatOutcome::Format(error) => {
            return Err(crate::commit::CommitRefusal::Format(error));
        }
        sage_db_types::framing::FormatOutcome::Refused(refusal) => {
            return Err(crate::commit::CommitRefusal::Refused(refusal));
        }
    };
    let mut sink = BytesSinkCollectorDefault::default();
    match sage_db_format::prepared::encode_view(&view, &mut sink, limits, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(()) => {}
        sage_db_types::framing::FormatOutcome::Format(error) => {
            return Err(crate::commit::CommitRefusal::Format(error));
        }
        sage_db_types::framing::FormatOutcome::Refused(refusal) => {
            return Err(crate::commit::CommitRefusal::Refused(refusal));
        }
    }
    let digest = match sage_db_format::digest::raw_sha256_bounded(&sink.bytes, ledger) {
        Ok(digest) => digest,
        // Preserve the ACTUAL refusal class: a budget shortfall and a caller stop are not encoding defects, and
        // collapsing either into `Encode` would tell the caller to fix its bytes when it should retry or stop.
        Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
            return Err(crate::commit::CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
            return Err(crate::commit::CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
    };
    Ok((sink.bytes, digest))
}

/// A sink that retains the complete encoded image.
#[derive(Default)]
struct BytesSinkCollectorDefault {
    bytes: Vec<u8>,
}

impl sage_db_types::framing::BytesSink for BytesSinkCollectorDefault {
    fn write(
        &mut self,
        bytes: &[u8],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), sage_db_types::framing::Refusal> {
        ledger
            .try_charge(bytes.len() as u64)
            .map_err(sage_db_types::framing::Refusal::Budget)?;
        if self.bytes.try_reserve_exact(bytes.len()).is_err() {
            return Err(sage_db_types::framing::Refusal::Allocation {
                requested: bytes.len(),
            });
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
