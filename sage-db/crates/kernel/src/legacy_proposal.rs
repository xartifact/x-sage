//! Required compatibility metadata carried by a proposal's canonical request.

mod body_bundle;

pub use body_bundle::MAX_COMPAT_PROPOSED_BODY_BYTES;
pub(crate) use body_bundle::pack as pack_body_bundle;

use sage_db_store::{
    fs::FileSystem,
    handle::{HandleRefusal, Store},
};
use sage_db_types::ledger::SharedLedger;

use crate::claims::{CommandError, InputError, admitted_vec};
use crate::concept_intent::check_atom;

pub(crate) const SUBMIT_RELATION_VERSION: u16 = 2;
pub(crate) const SUBMIT_RELATION_PENDING_VERSION: u16 = 3;
pub(crate) const SUBMIT_CONCEPT_VERSION: u16 = 3;
pub(crate) const SUBMIT_REFERENCED_BODY_VERSION: u16 = 4;
pub(crate) const REVIEW_VERSION: u16 = 2;

// This is the maximum legacy basename representable in a file directory entry.
pub(crate) const MAX_FILENAME_BYTES: usize = 255;

/// The original Bun filename and comparison basis for a submitted proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyProposalFile<'a> {
    /// A day and authored edge identify the file. A later submission replaces its projection.
    Edge {
        /// The basename under proposals/edges.
        filename: &'a str,
    },
    /// Review originally compared only the quoted atom.
    Atom {
        /// The basename under proposals/atoms.
        filename: &'a str,
        /// The exact quoted atom from the original document.
        current_atom: &'a str,
    },
    /// Review originally compared the complete projected concept document.
    Body {
        /// The basename under proposals/bodies.
        filename: &'a str,
        /// The 32 bytes of the public revision hash.
        expected_revision: [u8; 32],
    },
}

impl<'a> LegacyProposalFile<'a> {
    /// Compatibility basename, with no path components.
    #[must_use]
    pub const fn filename(self) -> &'a str {
        match self {
            Self::Edge { filename } | Self::Atom { filename, .. } | Self::Body { filename, .. } => {
                filename
            }
        }
    }
}

/// The compatibility metadata and the version of the view used to author it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyProposal<'a> {
    /// Filename and per-family basis.
    pub file: LegacyProposalFile<'a>,
    /// Committed publication on which the caller obtained the legacy comparison basis.
    pub observed_version: u64,
}

fn malformed() -> CommandError {
    InputError::LegacyProposalMetadata.into()
}

pub(crate) fn check_publication(
    store: &Store<impl FileSystem>,
    expected: u64,
) -> Result<(), CommandError> {
    let found = store.head();
    if found != Some(expected) {
        return Err(CommandError::Store(HandleRefusal::ExpectVersionMismatch {
            expected,
            found,
        }));
    }
    Ok(())
}

fn valid_filename(directory: LegacyDirectory, name: &str) -> bool {
    let suffix = if directory == LegacyDirectory::Bodies {
        ".json"
    } else {
        ".md"
    };
    let Some(stem) = name.strip_suffix(suffix) else {
        return false;
    };
    name.len() <= MAX_FILENAME_BYTES
        && !stem.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
}

fn is_date(bytes: &[u8]) -> bool {
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(offset, byte)| {
            if matches!(offset, 4 | 7) {
                *byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

fn is_stamp(bytes: &[u8]) -> bool {
    bytes.len() == 24
        && is_date(&bytes[..10])
        && bytes[10..]
            .iter()
            .enumerate()
            .all(|(offset, byte)| match offset {
                0 => *byte == b'T',
                3 | 6 | 9 => *byte == b'-',
                13 => *byte == b'Z',
                _ => byte.is_ascii_digit(),
            })
}

pub(crate) fn check_edge_name(
    file: LegacyProposalFile<'_>,
    from: &str,
    relation: &str,
    target: &str,
) -> Result<(), CommandError> {
    let LegacyProposalFile::Edge { filename } = file else {
        return Err(malformed());
    };
    let Some(date) = filename.as_bytes().get(..10) else {
        return Err(malformed());
    };
    if !is_date(date) {
        return Err(malformed());
    }
    let tail = filename
        .get(10..)
        .and_then(|tail| tail.strip_prefix('-'))
        .and_then(|tail| tail.strip_prefix(from))
        .and_then(|tail| tail.strip_prefix('-'))
        .and_then(|tail| tail.strip_prefix(relation))
        .and_then(|tail| tail.strip_prefix('-'))
        .and_then(|tail| tail.strip_suffix(".md"));
    if tail != Some(target) {
        return Err(malformed());
    }
    Ok(())
}

pub(crate) fn check_text_name(
    file: LegacyProposalFile<'_>,
    slug: &str,
) -> Result<(), CommandError> {
    let (marker, suffix) = match file {
        LegacyProposalFile::Atom { .. } => ("-", ".md"),
        LegacyProposalFile::Body { .. } => ("-body-", ".json"),
        LegacyProposalFile::Edge { .. } => return Err(malformed()),
    };
    let filename = file.filename();
    if !filename.as_bytes().get(..24).is_some_and(is_stamp) {
        return Err(malformed());
    }
    let suffix_hex = filename
        .get(24..)
        .and_then(|tail| tail.strip_prefix('-'))
        .and_then(|tail| tail.strip_prefix(slug))
        .and_then(|tail| tail.strip_prefix(marker))
        .and_then(|tail| tail.strip_suffix(suffix));
    if !suffix_hex.is_some_and(|value| {
        value.len() == 6
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(malformed());
    }
    Ok(())
}

// Payload companion: kind:u8, filename (u16 length and bytes), family basis,
// then the unchanged canonical bytes of the native submit. The frame's version
// makes this required companion distinguishable from old proposals.
pub(crate) fn encode(
    file: LegacyProposalFile<'_>,
    payload: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CommandError> {
    ledger.checkpoint()?;
    if !valid_filename(file.directory(), file.filename()) {
        return Err(malformed());
    }
    let (kind, basis_size) = match file {
        LegacyProposalFile::Edge { .. } => (0, 0),
        LegacyProposalFile::Atom { current_atom, .. } => {
            check_atom(current_atom).map_err(|_| malformed())?;
            (1, 2 + current_atom.len())
        }
        LegacyProposalFile::Body { .. } => (2, 32),
    };
    let filename = file.filename().as_bytes();
    let total = 3usize
        .checked_add(filename.len())
        .and_then(|total| total.checked_add(basis_size))
        .and_then(|total| total.checked_add(payload.len()))
        .ok_or_else(malformed)?;
    if crate::claims::extent(total)? > u64::from(sage_db_store::typed::MAX_PAYLOAD) {
        return Err(malformed());
    }
    let mut out = admitted_vec(total, ledger)?;
    out.push(kind);
    write_text(&mut out, filename)?;
    match file {
        LegacyProposalFile::Edge { .. } => {}
        LegacyProposalFile::Atom { current_atom, .. } => {
            write_text(&mut out, current_atom.as_bytes())?
        }
        LegacyProposalFile::Body {
            expected_revision, ..
        } => out.extend_from_slice(&expected_revision),
    }
    out.extend_from_slice(payload);
    Ok(out)
}

fn write_text(out: &mut Vec<u8>, text: &[u8]) -> Result<(), CommandError> {
    let len = u16::try_from(text.len()).map_err(|_| malformed())?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(text);
    Ok(())
}

pub(crate) fn encode_concept_submit(
    file: LegacyProposalFile<'_>,
    edit: &crate::concept_intent::ConceptTextEdit<'_>,
    reason: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<(u16, Vec<u8>), crate::concept::ConceptTextError> {
    use crate::concept_intent::{
        ConceptTextEdit, ConceptTextPayloadError, encode_submit_with_reason,
    };
    check_text_family(file, edit.field_mask, Some(reason), edit.body, ledger)?;
    let LegacyProposalFile::Body { filename, .. } = file else {
        let payload = encode_submit_with_reason(edit, reason, ledger)?;
        return Ok((SUBMIT_CONCEPT_VERSION, encode(file, &payload, ledger)?));
    };
    // Preserve the old request identity for every inline frame that can fit.
    match encode_submit_with_reason(edit, reason, ledger) {
        Ok(payload) if extent_fits_body_companion(payload.len(), filename.len()) => {
            return Ok((SUBMIT_CONCEPT_VERSION, encode(file, &payload, ledger)?));
        }
        Ok(_) | Err(ConceptTextPayloadError::TooLarge { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    // A v4 canonical body is encoded once in the proposal owner, independently
    // of the fixed-width signed body reference in the compact operation frame.
    let body = edit.body.ok_or_else(malformed)?;
    let borrowed = ConceptTextEdit {
        body: Some(""),
        ..*edit
    };
    let metadata = encode_submit_with_reason(&borrowed, reason, ledger)?;
    let op = body_bundle::bind(body, &metadata, ledger)?;
    Ok((SUBMIT_REFERENCED_BODY_VERSION, encode(file, &op, ledger)?))
}

fn extent_fits_body_companion(inner: usize, filename: usize) -> bool {
    inner
        .checked_add(filename)
        .and_then(|n| n.checked_add(3 + 32))
        .is_some_and(|n| n <= sage_db_store::typed::MAX_PAYLOAD as usize)
}

fn read_text(bytes: &[u8]) -> Result<(&str, &[u8]), CommandError> {
    let Some((width, rest)) = bytes.split_first_chunk::<2>() else {
        return Err(malformed());
    };
    let len = usize::from(u16::from_le_bytes(*width));
    let (text, rest) = rest.split_at_checked(len).ok_or_else(malformed)?;
    let text = std::str::from_utf8(text).map_err(|_| malformed())?;
    Ok((text, rest))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<(LegacyProposalFile<'_>, &[u8]), CommandError> {
    let (&kind, rest) = bytes.split_first().ok_or_else(malformed)?;
    let (filename, rest) = read_text(rest)?;
    let (file, payload) = match kind {
        0 => (LegacyProposalFile::Edge { filename }, rest),
        1 => {
            let (current_atom, payload) = read_text(rest)?;
            check_atom(current_atom).map_err(|_| malformed())?;
            (
                LegacyProposalFile::Atom {
                    filename,
                    current_atom,
                },
                payload,
            )
        }
        2 => {
            let (expected_revision, payload) =
                rest.split_first_chunk::<32>().ok_or_else(malformed)?;
            (
                LegacyProposalFile::Body {
                    filename,
                    expected_revision: *expected_revision,
                },
                payload,
            )
        }
        _ => return Err(malformed()),
    };
    if !valid_filename(file.directory(), file.filename()) || payload.is_empty() {
        return Err(malformed());
    }
    Ok((file, payload))
}

pub(crate) fn check_text_family(
    file: LegacyProposalFile<'_>,
    field_mask: u8,
    reason: Option<&str>,
    body: Option<&str>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CommandError> {
    use crate::concept_intent::{MASK_ATOM, MASK_BODY};
    match file {
        LegacyProposalFile::Atom { .. } if field_mask == MASK_ATOM => Ok(()),
        LegacyProposalFile::Body { .. } if field_mask == MASK_BODY => {
            let reason = reason.ok_or_else(malformed)?;
            let body = body.ok_or_else(malformed)?;
            if !has_non_whitespace(reason, ledger)?
                || !body.strip_suffix('\n').is_some_and(|text| {
                    text.chars()
                        .next_back()
                        .is_some_and(|last| !js_whitespace(last))
                })
            {
                return Err(malformed());
            }
            Ok(())
        }
        _ => Err(malformed()),
    }
}

fn has_non_whitespace(text: &str, ledger: &mut dyn SharedLedger) -> Result<bool, CommandError> {
    ledger.checkpoint()?;
    for (offset, character) in text.chars().enumerate() {
        ledger.try_charge_entries(1)?;
        if offset.is_multiple_of(1024) {
            ledger.checkpoint()?;
        }
        if !js_whitespace(character) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The legacy proposal directory fixes which filenames can collide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyDirectory {
    /// Authored relations under proposals/edges.
    Edges,
    /// Atom refinements under proposals/atoms.
    Atoms,
    /// Body replacements under proposals/bodies.
    Bodies,
}

impl LegacyDirectory {
    fn prefix(self) -> &'static str {
        match self {
            Self::Edges => "proposals/edges/",
            Self::Atoms => "proposals/atoms/",
            Self::Bodies => "proposals/bodies/",
        }
    }
}

impl LegacyProposalFile<'_> {
    /// The directory containing the committed filename.
    #[must_use]
    pub const fn directory(self) -> LegacyDirectory {
        match self {
            Self::Edge { .. } => LegacyDirectory::Edges,
            Self::Atom { .. } => LegacyDirectory::Atoms,
            Self::Body { .. } => LegacyDirectory::Bodies,
        }
    }
}

/// The canonical intent underneath the compatibility metadata.
#[derive(Debug)]
pub enum LegacyIntent<'a> {
    /// Authored assertion.
    Edge(crate::relations::DecodedSubmit<'a>),
    /// Authored assertion with a target slug whose Term was absent at submission.
    PendingEdge(crate::relations::DecodedPendingRelation<'a>),
    /// Concept atom or body edit.
    Text(crate::concept_intent::DecodedSubmit<'a>),
}

/// The legacy identity and independently decodable native intent.
#[derive(Debug)]
pub struct DecodedLegacyProposal<'a> {
    /// Committed filename, comparison basis and observed publication.
    pub compatibility: LegacyProposal<'a>,
    /// Native candidate payload.
    pub intent: LegacyIntent<'a>,
    /// Body SHA-256 already verified against the v4 frame and its owning blob.
    pub body_sha256: Option<[u8; 32]>,
}

/// Decode a hash-verified proposal blob on the same basis as its record.
///
/// An import retains the original file with its import identity, and an ordinary
/// native submission has no public filename. Both return `Ok(None)` here.
///
/// # Errors
/// A recognized compatibility operation was malformed or disagreed with its record.
pub fn decode_stored<'a>(
    record: &sage_db_types::persist::ProposalRecord,
    bytes: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Option<DecodedLegacyProposal<'a>>, CommandError> {
    use crate::{concept_intent, relations};
    use sage_db_store::typed::{self, RequestOutcome};
    use sage_db_types::persist::{ProposalKind, ProposalStatus};
    if record.legacy_id.is_some()
        || !matches!(
            record.kind,
            ProposalKind::RelationAssert | ProposalKind::ConceptMarkdown
        )
    {
        return Ok(None);
    }
    let (frame_bytes, raw_body) = if record.kind == ProposalKind::ConceptMarkdown {
        body_bundle::split(bytes)?
    } else {
        (bytes, None)
    };
    let frame = match typed::decode_request(frame_bytes, ledger) {
        RequestOutcome::Ok(value) => value,
        RequestOutcome::Rejected(error) => return Err(InputError::PersistedFrame(error).into()),
        RequestOutcome::Refused(error) => return Err(error.into()),
    };
    let [op] = frame.ops.as_slice() else {
        return Err(InputError::StoredOperationCount {
            found: frame.ops.len(),
        }
        .into());
    };
    let operation = (op.kind, op.version);
    let is_keyed_edge = operation == (relations::SUBMIT_RELATION_ASSERT, SUBMIT_RELATION_VERSION);
    let is_pending_edge = operation
        == (
            relations::SUBMIT_RELATION_ASSERT,
            SUBMIT_RELATION_PENDING_VERSION,
        );
    let referenced_body = operation
        == (
            concept_intent::SUBMIT_CONCEPT_TEXT,
            SUBMIT_REFERENCED_BODY_VERSION,
        );
    if referenced_body != raw_body.is_some() {
        return Err(malformed());
    }
    if !is_keyed_edge
        && !is_pending_edge
        && !referenced_body
        && operation != (concept_intent::SUBMIT_CONCEPT_TEXT, SUBMIT_CONCEPT_VERSION)
    {
        return Ok(None);
    }
    let observed_version = frame.expected_version.ok_or_else(malformed)?;
    if frame.read_set != record.base_revisions {
        return Err(malformed());
    }
    let (file, payload) = decode(op.payload)?;
    let pending = record.status == ProposalStatus::Pending;
    let mut body_sha256 = None;
    let intent = if is_pending_edge {
        if record.kind != ProposalKind::RelationAssert
            || !matches!(file, LegacyProposalFile::Edge { .. })
        {
            return Err(malformed());
        }
        let decoded =
            relations::decode_pending_submit(payload, ledger).map_err(|error| match error {
                relations::RelationAssertPayloadError::Refused(refusal) => refusal.into(),
                _ => malformed(),
            })?;
        let [source] = frame.read_set.as_slice() else {
            return Err(malformed());
        };
        if decoded.from != record.subject
            || decoded.from != source.key
            || decoded.declared_author != record.declared_author
            || (pending && record.common.reason.as_deref() != Some(decoded.reason))
        {
            return Err(malformed());
        }
        LegacyIntent::PendingEdge(decoded)
    } else if is_keyed_edge {
        if record.kind != ProposalKind::RelationAssert
            || !matches!(file, LegacyProposalFile::Edge { .. })
        {
            return Err(malformed());
        }
        let decoded = relations::decode_submit(payload, ledger).map_err(|error| match error {
            relations::RelationAssertPayloadError::Refused(refusal) => refusal.into(),
            _ => malformed(),
        })?;
        let [a, b] = frame.read_set.as_slice() else {
            return Err(malformed());
        };
        if decoded.from != record.subject
            || decoded.from.min(decoded.target) != a.key
            || decoded.from.max(decoded.target) != b.key
            || decoded.declared_author != record.declared_author
            || (pending && record.common.reason.as_deref() != Some(decoded.reason))
        {
            return Err(malformed());
        }
        LegacyIntent::Edge(decoded)
    } else {
        if record.kind != ProposalKind::ConceptMarkdown {
            return Err(malformed());
        }
        let (payload, body) = if let Some(raw_body) = raw_body {
            if !matches!(file, LegacyProposalFile::Body { .. }) {
                return Err(malformed());
            }
            let (metadata, body, sha) = body_bundle::open(payload, raw_body, ledger)?;
            body_sha256 = Some(sha);
            (metadata, Some(body))
        } else {
            (payload, None)
        };
        let mut decoded = concept_intent::decode_submit_with_reason(payload, ledger).map_err(
            |error| match error {
                concept_intent::ConceptTextPayloadError::Refused(refusal) => refusal.into(),
                _ => malformed(),
            },
        )?;
        if let Some(body) = body {
            if decoded.body != Some("") {
                return Err(malformed());
            }
            decoded.body = Some(body);
        }
        let [only] = frame.read_set.as_slice() else {
            return Err(malformed());
        };
        if decoded.subject != record.subject
            || decoded.subject != only.key
            || decoded.declared_author != record.declared_author
            || (pending && record.common.reason.as_deref() != decoded.reason)
        {
            return Err(malformed());
        }
        check_text_family(
            file,
            decoded.field_mask,
            decoded.reason,
            decoded.body,
            ledger,
        )?;
        LegacyIntent::Text(decoded)
    };
    Ok(Some(DecodedLegacyProposal {
        compatibility: LegacyProposal {
            file,
            observed_version,
        },
        intent,
        body_sha256,
    }))
}

/// The latest durable record occupying a compatibility filename.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AliasMatch {
    /// The publication on which the selection is complete.
    pub basis: sage_db_store::view::ViewBasis,
    /// Proposal identity and exact revision for a prospective review.
    pub proposal: sage_db_types::persist::RevPair,
    /// Current durable lifecycle value.
    pub status: sage_db_types::persist::ProposalStatus,
}

#[derive(Clone, Copy)]
struct Candidate {
    pair: sage_db_types::persist::RevPair,
    status: sage_db_types::persist::ProposalStatus,
    payload_ref: Option<[u8; 32]>,
}

fn scan_error(error: sage_db_store::view::RecordScanError<CommandError>) -> CommandError {
    use sage_db_store::view::RecordScanError;
    match error {
        RecordScanError::Visitor(error) => error,
        RecordScanError::Incomplete(error) => CommandError::Incomplete(error),
        RecordScanError::Read(error) => error.into(),
    }
}

/// Find a filename on the complete current basis, including imported proposals.
///
/// Same-filename writes each leave their own immutable request receipt. The latest
/// matching allocation wins even if it is terminal. A resolver never revives an
/// older candidate for an approved or rejected name.
///
/// # Errors
/// Refuses unauthorized reads, malformed filenames, incomplete views and unreadable payloads.
pub fn lookup_alias(
    store: &mut Store<impl FileSystem>,
    grants: &crate::auth::Grants<'_>,
    directory: LegacyDirectory,
    filename: &str,
    ledger: &mut dyn sage_db_store::lease::LeaseLedger,
) -> Result<Option<AliasMatch>, CommandError> {
    grants.authorize(store.kb_uuid(), crate::auth::Capability::Read, ledger)?;
    lookup_current(store, directory, filename, ledger)
}

fn lookup_current(
    store: &mut Store<impl FileSystem>,
    directory: LegacyDirectory,
    filename: &str,
    ledger: &mut dyn sage_db_store::lease::LeaseLedger,
) -> Result<Option<AliasMatch>, CommandError> {
    use crate::claims::{InternalDefect, ensure_import_ready, folded_current};
    use sage_db_store::{fold::FoldedRecord, typed};
    use sage_db_types::persist::{ProposalKind, RevPair};

    if !valid_filename(directory, filename) {
        return Err(malformed());
    }
    ensure_import_ready(store, ledger)?;
    let classify = |row: &FoldedRecord,
                    work: &mut dyn SharedLedger|
     -> Result<Option<Candidate>, CommandError> {
        let FoldedRecord::Proposal(row) = row else {
            return Ok(None);
        };
        work.checkpoint()?;
        let payload_ref = if let Some(path) = &row.legacy_id {
            work.try_charge_entries(crate::claims::extent(path.len())?)?;
            if path.strip_prefix(directory.prefix()) != Some(filename) {
                return Ok(None);
            }
            None
        } else {
            let compatible_kind = match directory {
                LegacyDirectory::Edges => ProposalKind::RelationAssert,
                LegacyDirectory::Atoms | LegacyDirectory::Bodies => ProposalKind::ConceptMarkdown,
            };
            if row.kind != compatible_kind {
                return Ok(None);
            }
            Some(row.payload_ref)
        };
        Ok(Some(Candidate {
            pair: RevPair {
                key: row.key,
                revision: row.revision,
            },
            status: row.status,
            payload_ref,
        }))
    };
    let mut count = 0usize;
    let basis = store
        .scan_identified_records(ledger, |row, work| {
            if classify(row, work)?.is_some() {
                count = count
                    .checked_add(1)
                    .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?;
            }
            Ok(())
        })
        .map_err(scan_error)?;
    let mut candidates = admitted_vec(count, ledger)?;
    let filled = store
        .scan_identified_records(ledger, |row, work| {
            if let Some(candidate) = classify(row, work)? {
                if candidates.len() == count {
                    return Err(CommandError::BasisDivergence);
                }
                candidates.push(candidate);
            }
            Ok(())
        })
        .map_err(scan_error)?;
    if filled != basis || candidates.len() != count {
        return Err(CommandError::BasisDivergence);
    }

    let mut newest: Option<AliasMatch> = None;
    for candidate in candidates.into_iter().rev() {
        ledger.checkpoint()?;
        if newest
            .as_ref()
            .is_some_and(|found| found.proposal.key >= candidate.pair.key)
        {
            continue;
        }
        if let Some(payload_ref) = candidate.payload_ref {
            let payload = store.read_committed_blob(payload_ref, typed::MAX_FRAME_BYTES, ledger)?;
            if payload.basis() != basis {
                return Err(CommandError::BasisDivergence);
            }
            let Some(FoldedRecord::Proposal(record)) =
                folded_current(store, candidate.pair.key, ledger)?
            else {
                return Err(CommandError::BasisDivergence);
            };
            let Some(file) = decode_stored(record, payload.as_bytes(), ledger)?
                .map(|decoded| decoded.compatibility.file)
            else {
                continue;
            };
            ledger.try_charge_entries(crate::claims::extent(file.filename().len())?)?;
            if file.directory() != directory || file.filename() != filename {
                continue;
            }
        }
        newest = Some(AliasMatch {
            basis,
            proposal: candidate.pair,
            status: candidate.status,
        });
    }
    Ok(newest)
}

/// Exact filename and committed version against which a merge was prepared.
#[derive(Clone, Copy, Debug)]
pub struct LegacyReview<'a> {
    /// Family of the selected file.
    pub directory: LegacyDirectory,
    /// Requested basename in that family.
    pub filename: &'a str,
    /// The publication the caller observed for this merge.
    pub observed_version: u64,
}

/// Public revision measured by the Bun projection on the review's exact publication.
#[derive(Clone, Copy, Debug)]
pub struct BodyObservation {
    /// Legacy public revision digest, including authored outgoing relations.
    pub public_revision: [u8; 32],
}

pub(crate) fn encode_review(
    review: LegacyReview<'_>,
    observation: Option<BodyObservation>,
    payload: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CommandError> {
    ledger.checkpoint()?;
    if !valid_filename(review.directory, review.filename)
        || (review.directory == LegacyDirectory::Bodies) != observation.is_some()
    {
        return Err(malformed());
    }
    let total = 3usize
        .checked_add(review.filename.len())
        .and_then(|sum| sum.checked_add(if observation.is_some() { 32 } else { 0 }))
        .and_then(|sum| sum.checked_add(payload.len()))
        .ok_or_else(malformed)?;
    if crate::claims::extent(total)? > u64::from(sage_db_store::typed::MAX_PAYLOAD) {
        return Err(malformed());
    }
    let mut out = admitted_vec(total, ledger)?;
    out.push(match review.directory {
        LegacyDirectory::Edges => 0,
        LegacyDirectory::Atoms => 1,
        LegacyDirectory::Bodies => 2,
    });
    write_text(&mut out, review.filename.as_bytes())?;
    if let Some(observation) = observation {
        out.extend_from_slice(&observation.public_revision);
    }
    out.extend_from_slice(payload);
    Ok(out)
}

pub(crate) fn require_current_alias(
    store: &mut Store<impl FileSystem>,
    review: LegacyReview<'_>,
    target: sage_db_types::persist::RevPair,
    ledger: &mut dyn sage_db_store::lease::LeaseLedger,
) -> Result<(), CommandError> {
    check_publication(store, review.observed_version)?;
    let found = lookup_current(store, review.directory, review.filename, ledger)?;
    if !found.is_some_and(|found| found.proposal == target) {
        return Err(InputError::LegacyProposalAliasMoved.into());
    }
    Ok(())
}

pub(crate) fn js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}
