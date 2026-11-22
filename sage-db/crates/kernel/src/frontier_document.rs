//! Versioned raw Frontier document storage for the Bun compatibility adapter.
//!
//! The database commits the exact bytes as a canonical KB metadata reference.
//! The caller interprets the document and decides whether an append is a duplicate.

use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store::view::{CommittedBlob, KbMetadataReadError, ViewBasis};
use sage_db_types::framing::Limits;
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{ActorOrigin, Extensions, IdSpace, KbMetadataRecord};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, PreparedTransaction};

use crate::auth::{Capability, Grants};
use crate::claims::{
    CommandError, InternalDefect, RequestMeta, admitted_bytes, admitted_string, admitted_vec,
    check_meta, ensure_import_ready, extent, frame_and_digest, map_format, outcome_event,
    retry_outcome, revision_next, version_next,
};

/// Replace the exact bytes of a legacy Frontier document.
pub const REPLACE_FRONTIER_DOCUMENT: u16 = 15;
/// Operation v1 is an expected revision (8-byte LE) and the new document's raw SHA-256.
pub const OPERATION_VERSION: u16 = 1;

/// A read bears the metadata revision even if the document has not been seeded.
pub struct FrontierDocument {
    /// Exact durable metadata revision used for compare-and-swap.
    pub revision: RecordRevision,
    /// Durable publication from which both fields were read.
    pub basis: ViewBasis,
    /// Hash-verified bytes with the retained reservation, if established.
    pub raw: Option<CommittedBlob>,
}

/// Failure to obtain or replace an exact Frontier document.
#[derive(Debug)]
pub enum FrontierDocumentError {
    /// Shared authorization, resource, authority, retry or commit refusal.
    Command(CommandError),
    /// Canonical metadata could not be read.
    Metadata(KbMetadataReadError),
    /// The document's metadata revision changed since the caller read it.
    Stale {
        /// The caller's previous revision.
        expected: RecordRevision,
        /// The definitive current metadata revision.
        actual: RecordRevision,
    },
}

impl From<CommandError> for FrontierDocumentError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Read the exact current document on one durable publication.
///
/// # Errors
///
/// Authorization, incomplete import, invalid metadata, a caller ceiling or
/// ledger refusal, or confirmed damage to the committed blob.
pub fn read_frontier_document(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    max_document_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<FrontierDocument, FrontierDocumentError> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(CommandError::from)?;
    ensure_import_ready(store, ledger)?;
    let (metadata, basis) = store
        .kb_metadata(ledger)
        .map_err(FrontierDocumentError::Metadata)?;
    let revision = metadata.revision;
    let digest = metadata.legacy_frontier_ref;
    let raw = match digest {
        None => None,
        Some(digest) => {
            let blob = store
                .read_committed_blob(digest, max_document_len, ledger)
                .map_err(CommandError::from)?;
            if blob.basis() != basis {
                return Err(CommandError::BasisDivergence.into());
            }
            Some(blob)
        }
    };
    Ok(FrontierDocument {
        revision,
        basis,
        raw,
    })
}

/// Publish exact bytes with a metadata compare-and-swap and a durable request outcome.
///
/// Exact retries return their original outcome before the write fence and CAS.
/// The new document, metadata and outcome publish in one transaction. The caller
/// owns document parsing, legacy append semantics and its transport size policy.
///
/// # Errors
///
/// Authorization, an incomplete import, an idempotency mismatch, stale metadata,
/// a caller refusal or a definite store commit refusal.
pub fn replace_frontier_document(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    expected: RecordRevision,
    document: &[u8],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, FrontierDocumentError> {
    let authorized = grants
        .authorize(store.kb_uuid(), Capability::WriteAux, ledger)
        .map_err(CommandError::from)?;
    let principal = authorized.principal();
    check_meta(&meta, principal).map_err(CommandError::from)?;
    let document_ref = map_format(typed::raw_hash(document, ledger))?;
    let mut payload = [0u8; 40];
    payload[..8].copy_from_slice(&expected.get().to_le_bytes());
    payload[8..].copy_from_slice(&document_ref);
    let (_, digest) = frame_and_digest(
        REPLACE_FRONTIER_DOCUMENT,
        OPERATION_VERSION,
        &payload,
        &[],
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, principal, meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;

    let (current, publication) = store
        .kb_metadata(ledger)
        .map_err(FrontierDocumentError::Metadata)?;
    if current.revision != expected {
        return Err(FrontierDocumentError::Stale {
            expected,
            actual: current.revision,
        });
    }
    // Existing commit-basis lookup supplies the rules. This command allocates no ID.
    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, ledger)
        .map_err(CommandError::from)?;
    if publication != basis.basis {
        return Err(CommandError::BasisDivergence.into());
    }

    let version = version_next(store.head())?;
    let next = next_metadata(current, document_ref, principal, meta.created_at, ledger)?;
    let result = typed::result_payload(version, &[], &[], ledger).map_err(CommandError::from)?;
    let result_ref = map_format(typed::raw_hash(&result, ledger))?;
    let outcome = outcome_event(
        version,
        principal,
        meta.request_id,
        digest,
        result_ref,
        meta.created_at,
        ledger,
    )?;
    let mut events = admitted_vec(2, ledger)?;
    events.push(map_format(typed::prepare_event(
        EventInput::KbMetadata(&next),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);
    let mut blobs = admitted_vec(3, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    for (sha256, bytes) in [(document_ref, document), (result_ref, result.as_slice())] {
        // The same physical address may already be declared, even as a rules blob.
        if blobs.iter().all(|blob| blob.sha256 != sha256) {
            blobs.push(BlobDeclaration {
                sha256,
                declared_len: extent(bytes.len())?,
                source: BlobSource::Provide(admitted_bytes(bytes, ledger)?),
            });
        }
    }
    let prepared = PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: meta.tx_id,
        kb_uuid: publication.kb_uuid,
        created_at: meta.created_at,
        principal_id: admitted_string(principal, ledger)?,
        request_id: admitted_string(meta.request_id, ledger)?,
        payload_digest: digest,
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, limits, ledger) {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        result => Ok(result),
    }
}

fn next_metadata(
    current: &KbMetadataRecord,
    document_ref: [u8; 32],
    principal: &str,
    created_at: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<KbMetadataRecord, CommandError> {
    let mut domains = admitted_vec(current.domains.len(), ledger)?;
    for domain in &current.domains {
        domains.push(admitted_string(domain, ledger)?);
    }
    let current_extensions = current.extensions.entries();
    let mut extensions = admitted_vec(current_extensions.len(), ledger)?;
    for (key, value) in current_extensions {
        extensions.push((
            admitted_string(key, ledger)?,
            admitted_bytes(value, ledger)?,
        ));
    }
    let extensions = Extensions::from_canonical(extensions)
        .map_err(|_| CommandError::Internal(InternalDefect::EncodeDecodeDivergence))?;
    Ok(KbMetadataRecord {
        revision: revision_next(current.revision)?,
        kb_uuid: current.kb_uuid,
        slug: admitted_string(&current.slug, ledger)?,
        name: admitted_string(&current.name, ledger)?,
        description: admitted_string(&current.description, ledger)?,
        domains,
        legacy_frontier_ref: Some(document_ref),
        audit: sage_db_types::persist::OptionalActorAudit {
            actor: Some(ActorOrigin::Authenticated),
            recorded_by: Some(admitted_string(principal, ledger)?),
            reason: None,
            created_at: current.audit.created_at,
            updated_at: Some(created_at),
        },
        extensions,
        changed_fields: None,
    })
}
