//! Approval assessment on one real Store publication.
//!
//! This composes authorization, current proposal lookup and object baselines.
//! It deliberately does not parse the proposal's payload or write a decision.
//! The future writer must first check request idempotency, derive the complete
//! payload dependency set, then validate/apply the candidate and persist content
//! and proposal status atomically. An assessment alone never authorizes bypassing
//! those steps, and cannot be replayed against another publication.

use crate::auth::{AuthError, Capability, Grants};
use crate::revision::{BaselineVerdict, RevisionError, check_approval_with};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::open::OutcomeIncomplete;
use sage_db_store::view::{RecordLookup, RecordReadError, ViewBasis};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, RecordKind, RevPair};

/// A review refusal or incomplete authority, never a committed result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewError {
    /// The authenticated grant did not permit review on this KB.
    Authorization(AuthError),
    /// An operation asked to review something other than a proposal.
    NotProposal(RecordKey),
    /// The proposal does not exist on the current durable basis.
    ProposalMissing(RecordKey),
    /// No definitive current membership can be established.
    Incomplete(OutcomeIncomplete),
    /// Store lookup failed without laundering its refusal or internal defect.
    RecordRead(RecordReadError),
    /// A baseline or mutation scope is malformed.
    Revision(RevisionError),
    /// The returned physical record contradicts its numeric key: internal defect.
    RecordKindMismatch(RecordKey),
    /// Reads disagreed about publication identity: internal defect.
    BasisMismatch,
}

impl From<RevisionError> for ReviewError {
    fn from(error: RevisionError) -> Self {
        Self::Revision(error)
    }
}

/// The result of read-only review checks on a particular publication.
///
/// This is an assessment, not a transaction or a reusable authorization token.
/// A serial writer must assess again if it changes the current publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalAssessment<'a> {
    /// Verified reviewer identity, never the proposal's declared author.
    pub reviewer: &'a str,
    /// The exact root observed by every record lookup.
    pub basis: ViewBasis,
    /// Proposal revision that was assessed.
    pub proposal: RevPair,
    /// Current baseline or an object-specific conflict to persist atomically.
    pub verdict: BaselineVerdict,
}

/// Assess one pending stored proposal without changing any Store state.
///
/// `required` must be derived by the payload planner, not copied from an
/// untrusted request's claimed read set. It names existing dependencies; absence
/// assertions for creates are a separate lifecycle validation obligation.
/// Keeping a shared Store borrow makes all reads use the same publication.
///
/// # Errors
/// Authorization, missing/terminal proposal, incomplete authority, malformed
/// dependencies, or the caller's exact canonical work/stop refusal.
pub fn assess_approval<'a, F: FileSystem>(
    store: &Store<F>,
    grants: &Grants<'a>,
    proposal_key: RecordKey,
    required: &[RecordKey],
    ledger: &mut dyn SharedLedger,
) -> Result<ApprovalAssessment<'a>, ReviewError> {
    let reviewer = grants
        .authorize(store.kb_uuid(), Capability::Review, ledger)
        .map_err(ReviewError::Authorization)?;
    if proposal_key.kind() != RecordKind::Proposal {
        return Err(ReviewError::NotProposal(proposal_key));
    }
    let (proposal, basis) = match store
        .record(proposal_key, ledger)
        .map_err(ReviewError::RecordRead)?
    {
        RecordLookup::Found {
            record: FoldedRecord::Proposal(proposal),
            basis,
        } => (proposal, basis),
        RecordLookup::Found { .. } => return Err(ReviewError::RecordKindMismatch(proposal_key)),
        RecordLookup::Absent { .. } => return Err(ReviewError::ProposalMissing(proposal_key)),
        RecordLookup::Incomplete(reason) => return Err(ReviewError::Incomplete(reason)),
    };
    let verdict = check_approval_with(proposal, required, ledger, |key, ledger| {
        match store.record(key, ledger).map_err(ReviewError::RecordRead)? {
            RecordLookup::Found {
                record,
                basis: found,
            } => {
                if found != basis {
                    return Err(ReviewError::BasisMismatch);
                }
                Ok(Some(record.revision()))
            }
            RecordLookup::Absent { basis: found } => {
                if found != basis {
                    return Err(ReviewError::BasisMismatch);
                }
                Ok(None)
            }
            RecordLookup::Incomplete(reason) => Err(ReviewError::Incomplete(reason)),
        }
    })?;
    Ok(ApprovalAssessment {
        reviewer: reviewer.principal(),
        basis,
        proposal: RevPair {
            key: proposal_key,
            revision: proposal.revision,
        },
        verdict,
    })
}
