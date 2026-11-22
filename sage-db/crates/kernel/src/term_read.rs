//! Authorized current Term and body reads, bound to one durable publication.
//!
//! This is the content-read seam for a later document query. It does not construct an
//! editable projection, relationship baseline, search result or historical query. The
//! current after-image (including a retired flag) is kept behind its existing owner;
//! body bytes have a separate leased owner. No payload is copied out of either lease.
use crate::{
    auth::{Capability, Grants},
    claims::{CommandError, InputError, ensure_import_ready},
};
use sage_db_store::{
    fold::FoldedRecord,
    fs::FileSystem,
    handle::Store,
    lease::LeaseLedger,
    view::{CommittedBlob, RecordImage, RecordImageLookup, ViewBasis},
};
use sage_db_types::persist::{RecordKey, RecordKind};

/// A complete current after-image and its verified content, or confirmed absence.
#[derive(Debug)]
pub enum TermRead {
    /// Both retained owners refer to the same CURRENT-named manifest file hash.
    Found {
        /// Existing full typed record owner; not a fresh owning copy.
        record: RecordImage,
        /// Actual length/hash-verified body bytes and their retained reservation.
        body: CommittedBlob,
    },
    /// Complete durable coverage proves this numeric key has no current after-image.
    Absent {
        /// The publication which established absence.
        basis: ViewBasis,
    },
}

/// A read failed; errors retain their actual operation and remedy.
#[derive(Debug)]
pub enum TermReadError {
    /// Exact existing capability, import, record, blob, ledger or basis failure.
    Command(CommandError),
    /// A Term key named a retained owner of another family, an internal index defect.
    WrongRetainedFamily {
        /// Numeric identity that contradicted the retained owner.
        key: RecordKey,
    },
}
impl From<CommandError> for TermReadError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Read a current Term after-image and the body its own digest references.
///
/// `read` is required explicitly. The whole synchronous operation holds `&mut Store`,
/// so no Store publication can interleave. Both underlying reads independently require
/// durable coverage and their complete `ViewBasis` values are compared. Missing content,
/// a configured ceiling, actual filesystem error/effect, corruption and caller refusal
/// remain the Store's own typed classes. Body bytes are not decoded or normalized.
///
/// # Errors
/// [`TermReadError`], never a successful partial read or fabricated absence.
pub fn read_term<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    key: RecordKey,
    max_body_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<TermRead, TermReadError> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(CommandError::from)?;
    ensure_import_ready(store, ledger)?;
    if key.kind() != RecordKind::Term {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: RecordKind::Term,
            found: key.kind(),
        })
        .into());
    }
    let (record, basis) = match store
        .record_owned(key, ledger)
        .map_err(CommandError::from)?
    {
        RecordImageLookup::Found { image, basis } => (image, basis),
        RecordImageLookup::Absent { basis } => return Ok(TermRead::Absent { basis }),
        RecordImageLookup::Incomplete(reason) => {
            return Err(CommandError::Incomplete(reason).into());
        }
    };
    let FoldedRecord::Term(term) = record.record() else {
        return Err(TermReadError::WrongRetainedFamily { key });
    };
    let body = store
        .read_committed_blob(term.body_ref, max_body_len, ledger)
        .map_err(CommandError::from)?;
    if body.basis() != basis {
        return Err(CommandError::BasisDivergence.into());
    }
    Ok(TermRead::Found { record, body })
}
