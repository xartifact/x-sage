//! `graph_digest` assembly: `storage-digests.md` §4.5.
//!
//! The projection is deliberately narrow. `atom`, `slug`, `domains`, `triggers`,
//! `body_ref`, evidence, proposals, timestamps, `extensions`, `rules_digest` and
//! `engine_version` are **not** inputs: a body-only edit must not invalidate a proof
//! cache, and the rules digest is its own cache-key component rather than graph data.
//!
//! Order is by **numeric id** (`Term.id`, `Assertion.id`) — unlike
//! [`crate::state`] §4.4.2, which orders by key *bytes*. Both exist because a
//! little-endian `u128` assertion id sorts differently from its numeric value; using one
//! rule for both digests is the defect the two modules exist to separate.
//!
//! `revision` is a semantic input (D2): a proof leaf binds "this assertion, at this
//! revision", so two revisions of one endpoint must produce different graph digests or a
//! cache would return a proof bound to the superseded revision.

use sage_db_types::ledger::SharedLedger;
use sage_db_types::record::{AssertionRecord, TermMeta};
use sage_db_types::relation::RelationId;

use crate::digest::{Domain, digest};
use crate::state::{DigestAssemblyError, StructureDefect};

/// `u32 term id` + `u8 layer`.
const TERM_BYTES: usize = 5;
/// `u128 id` + `u64 revision` + `u32 from` + `u16 relation` + `u32 target` +
/// `u16 confidence`.
const ASSERTION_BYTES: usize = 16 + 8 + 4 + 2 + 4 + 2;

/// Encode the §4.5 payload into `out`, which must be empty.
///
/// `terms` and `assertions` must already be in ascending id order with no duplicates —
/// the shape [`sage_db_types::graph::BaseGraph`] hands out after construction. The
/// active-only filter is the fold's: a retired term or assertion never reaches this
/// projection, because "active graph" is a state property, not a digest option.
///
/// # Errors
///
/// [`DigestAssemblyError`]; on any error `out` is left empty.
pub fn encode_graph_payload(
    terms: &[TermMeta],
    assertions: &[AssertionRecord],
    out: &mut Vec<u8>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), DigestAssemblyError> {
    ledger.checkpoint()?;
    if !out.is_empty() {
        return Err(StructureDefect::BufferNotEmpty.error());
    }
    let term_count = u32::try_from(terms.len()).map_err(|_| StructureDefect::TooLarge.error())?;
    let assertion_count =
        u32::try_from(assertions.len()).map_err(|_| StructureDefect::TooLarge.error())?;

    // Pass 1: validate both slices in place, with no scratch buffer and no sorting.
    let mut previous_term: Option<u32> = None;
    for (index, term) in terms.iter().enumerate() {
        ledger.checkpoint()?;
        if previous_term.is_some_and(|prev| term.id.get() <= prev) {
            return Err(StructureDefect::TermsNotAscending { index }.error());
        }
        previous_term = Some(term.id.get());
    }
    let mut previous_assertion: Option<u128> = None;
    for (index, assertion) in assertions.iter().enumerate() {
        ledger.checkpoint()?;
        if previous_assertion.is_some_and(|prev| assertion.id.get() <= prev) {
            return Err(StructureDefect::AssertionsNotAscending { index }.error());
        }
        previous_assertion = Some(assertion.id.get());
        // `RelationId` is a closed 0..=6 enum, so this cannot fail; the check is kept as a
        // documented invariant rather than an assumption about a future widened set.
        if RelationId::from_u8(assertion.relation.get()).is_none() {
            return Err(StructureDefect::UnknownRelation { index }.error());
        }
    }

    let total = 4 + terms.len() * TERM_BYTES + 4 + assertions.len() * ASSERTION_BYTES;
    ledger.try_charge(total as u64)?;
    out.try_reserve_exact(total)
        .map_err(|_| DigestAssemblyError::Allocation { requested: total })?;

    // Pass 2: fixed-width little-endian, values taken straight from the projections.
    out.extend_from_slice(&term_count.to_le_bytes());
    for term in terms {
        out.extend_from_slice(&term.id.get().to_le_bytes());
        out.push(term.layer.get());
    }
    out.extend_from_slice(&assertion_count.to_le_bytes());
    for assertion in assertions {
        out.extend_from_slice(&assertion.id.get().to_le_bytes());
        out.extend_from_slice(&assertion.revision.get().to_le_bytes());
        out.extend_from_slice(&assertion.from.get().to_le_bytes());
        out.extend_from_slice(&u16::from(assertion.relation.get()).to_le_bytes());
        out.extend_from_slice(&assertion.target.get().to_le_bytes());
        // Tag 21's persisted code, from its single declaration in `sage-db-types`: a cast
        // here would depend on declaration order instead of the registered value.
        out.extend_from_slice(&assertion.confidence.as_u16().to_le_bytes());
    }
    debug_assert_eq!(out.len(), total);
    Ok(())
}

/// `graph_digest` over the §4.5 projection.
///
/// # Errors
///
/// As [`encode_graph_payload`].
pub fn graph_digest(
    terms: &[TermMeta],
    assertions: &[AssertionRecord],
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], DigestAssemblyError> {
    let mut payload = Vec::new();
    encode_graph_payload(terms, assertions, &mut payload, ledger)?;
    ledger.checkpoint()?;
    Ok(digest(Domain::Graph, &payload))
}
