//! Current legacy proposal identities and their imported or typed payloads.

use std::net::TcpStream;

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::legacy_proposal::{self, LegacyIntent, LegacyProposalFile};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::{
    ImportReadiness, RecordImage, RecordImageLookup, RecordLookup, RecordReadError,
    RecordScanError, ViewBasis,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{ProposalKind, ProposalRecord, RecordKey};
use serde::Serialize;

use crate::activation::{limit, push, render_error, reserve, step};
use crate::http::{self, Deadline, Failure};
use crate::import_source::{self, Example};
use crate::operator_read::{Decimal, Digest};
use crate::queue_reads::status_name;

const MAX_ROWS: usize = 512;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_RAW_BODIES: u64 = 2 * 1024 * 1024;

pub(crate) fn serve(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    raw_bodies: bool,
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(crate::runtime::auth_error)?;
    let ImportReadiness::Ready { basis } = store
        .import_readiness(ledger)
        .map_err(|error| crate::runtime::kernel_error(error.into()))?
    else {
        return Err(Failure::unavailable("no complete readable publication"));
    };
    let mut keys = reserve(MAX_ROWS, ledger)?;
    let observed = store
        .scan_current_records(ledger, |record, work| {
            if let FoldedRecord::Proposal(row) = record
                && (path(row)?.is_some()
                    || matches!(
                        row.kind,
                        ProposalKind::RelationAssert | ProposalKind::ConceptMarkdown
                    ))
            {
                if keys.len() == MAX_ROWS {
                    return Err(limit("legacy proposal listing exceeds its row ceiling"));
                }
                push(&mut keys, row.key, work)?;
            }
            Ok(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(failure) => failure,
            RecordScanError::Read(read) => read_error(read),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
        })?;
    if observed != basis {
        return Err(Failure::internal());
    }
    let mut owners = reserve(keys.len(), ledger)?;
    let mut remaining = http::MAX_RESPONSE as u64 + if raw_bodies { MAX_RAW_BODIES } else { 0 };
    for key in keys {
        step(ledger)?;
        let RecordImageLookup::Found {
            image,
            basis: read_basis,
        } = store.record_owned(key, ledger).map_err(read_error)?
        else {
            return Err(Failure::internal());
        };
        let record = proposal(&image)?;
        if read_basis != basis {
            return Err(Failure::internal());
        }
        let per_file = if record.legacy_id.is_some() {
            MAX_FILE_BYTES
        } else {
            sage_db_store::typed::MAX_FRAME_BYTES
        };
        let blob = store
            .read_committed_blob(record.payload_ref, remaining.min(per_file), ledger)
            .map_err(crate::runtime::blob_error)?;
        if blob.basis() != basis {
            return Err(Failure::internal());
        }
        remaining = remaining
            .checked_sub(u64::try_from(blob.len()).map_err(|_| Failure::internal())?)
            .ok_or_else(Failure::internal)?;
        push(&mut owners, (image, blob), ledger)?;
    }
    let archive = import_source::import_package(store, basis, ledger)?;
    let examples = archive
        .as_ref()
        .map(|package| import_source::examples(package, ledger))
        .transpose()?
        .unwrap_or_default();
    let mut files = reserve(owners.len(), ledger)?;
    let mut native_files = reserve(owners.len(), ledger)?;
    for (image, blob) in &owners {
        let row = proposal(image)?;
        if let Some(path) = path(row)? {
            push(
                &mut files,
                File {
                    path,
                    status: status_name(row.status),
                    payload: std::str::from_utf8(blob.as_bytes())
                        .map_err(|_| Failure::internal())?,
                    key: Key::from(row.key),
                    revision: Decimal(row.revision.get()),
                },
                ledger,
            )?;
        } else if let Some(file) = legacy_proposal::decode_stored(row, blob.as_bytes(), ledger)
            .map_err(crate::runtime::kernel_error)?
        {
            push(
                &mut native_files,
                native_file(store, row, file, basis, raw_bodies, ledger)?,
                ledger,
            )?;
        }
    }
    let reply = Reply {
        basis: Basis {
            kb_uuid: Decimal(basis.kb_uuid),
            version: Decimal(basis.version.get()),
            manifest_sha256: Digest(basis.manifest_sha256),
        },
        files,
        examples,
        native_files,
    };
    if !raw_bodies
        || !reply
            .native_files
            .iter()
            .any(|file| file.body.as_ref().is_some_and(|body| body.tail.is_some()))
    {
        return http::respond(stream, &reply, deadline, ledger).map_err(render_error);
    }
    respond_with_raw_bodies(stream, &reply, deadline, ledger)
}

fn respond_with_raw_bodies(
    stream: &TcpStream,
    reply: &Reply<'_>,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let mut scratch = [0u8; http::MAX_RESPONSE];
    let metadata = http::render(&mut scratch, ledger, |out| {
        serde_json::to_writer(out, reply).map_err(std::io::Error::from)
    })
    .map_err(render_error)?;
    let metadata_width = u32::try_from(metadata.len())
        .map_err(|_| Failure::internal())?
        .to_le_bytes();
    let mut parts = reserve(reply.native_files.len() + 2, ledger)?;
    push(&mut parts, &metadata_width[..], ledger)?;
    push(&mut parts, metadata, ledger)?;
    let mut remaining = MAX_RAW_BODIES;
    for file in &reply.native_files {
        step(ledger)?;
        let Some(tail) = file.body.as_ref().and_then(|body| body.tail) else {
            continue;
        };
        let len = u64::try_from(tail.len()).map_err(|_| Failure::internal())?;
        remaining = remaining
            .checked_sub(len)
            .ok_or_else(|| limit("legacy proposal bodies exceed their byte ceiling"))?;
        ledger
            .try_charge(len)
            .map_err(|error| Failure::refused(sage_db_types::framing::Refusal::Budget(error)))?;
        push(&mut parts, tail.as_bytes(), ledger)?;
    }
    http::respond_octets(stream, "200 OK", &parts, deadline);
    Ok(())
}

fn term_slug<'a>(
    store: &'a Store<impl FileSystem>,
    key: RecordKey,
    basis: ViewBasis,
    ledger: &mut AttemptLedger<'_>,
) -> Result<&'a str, Failure> {
    match store.record(key, ledger).map_err(read_error)? {
        RecordLookup::Found {
            record: FoldedRecord::Term(term),
            basis: actual,
        } if actual == basis => Ok(&term.slug),
        _ => Err(Failure::internal()),
    }
}

fn native_file<'a>(
    store: &'a Store<impl FileSystem>,
    row: &ProposalRecord,
    decoded: legacy_proposal::DecodedLegacyProposal<'a>,
    basis: ViewBasis,
    raw_bodies: bool,
    ledger: &mut AttemptLedger<'_>,
) -> Result<NativeFile<'a>, Failure> {
    let mut view = NativeFile {
        directory: match decoded.compatibility.file {
            LegacyProposalFile::Edge { .. } => "edges",
            LegacyProposalFile::Atom { .. } => "atoms",
            LegacyProposalFile::Body { .. } => "bodies",
        },
        filename: decoded.compatibility.file.filename(),
        status: status_name(row.status),
        key: Key::from(row.key),
        revision: Decimal(row.revision.get()),
        subject_slug: term_slug(store, row.subject, basis, ledger)?,
        edge: None,
        atom: None,
        body: None,
    };
    match (decoded.compatibility.file, decoded.intent) {
        (LegacyProposalFile::Edge { .. }, LegacyIntent::Edge(edge)) => {
            view.edge = Some(Edge {
                target_slug: term_slug(store, edge.target, basis, ledger)?,
                relation: edge.relation.as_str(),
                confidence: edge.confidence.as_str(),
            });
        }
        (LegacyProposalFile::Edge { .. }, LegacyIntent::PendingEdge(edge)) => {
            view.edge = Some(Edge {
                target_slug: edge.target_slug,
                relation: edge.relation.as_str(),
                confidence: edge.confidence.as_str(),
            });
        }
        (LegacyProposalFile::Atom { current_atom, .. }, LegacyIntent::Text(edit)) => {
            view.atom = Some(Atom {
                current_atom,
                proposed_atom: edit.atom.ok_or_else(Failure::internal)?,
            });
        }
        (
            LegacyProposalFile::Body {
                expected_revision, ..
            },
            LegacyIntent::Text(edit),
        ) => {
            let body = edit.body.ok_or_else(Failure::internal)?;
            // The raw response uses one tail for each verified large body, in
            // this metadata's native-file order. Inline old bodies retain JSON.
            let external = raw_bodies && decoded.body_sha256.is_some();
            view.body = Some(Body {
                expected_revision: Digest(expected_revision),
                proposed_body: (!external).then_some(body),
                raw_bytes: external
                    .then(|| u64::try_from(body.len()).map(Decimal))
                    .transpose()
                    .map_err(|_| Failure::internal())?,
                tail: external.then_some(body),
                reason: edit.reason.ok_or_else(Failure::internal)?,
            });
        }
        _ => return Err(Failure::internal()),
    }
    Ok(view)
}

fn proposal(image: &RecordImage) -> Result<&ProposalRecord, Failure> {
    match image.record() {
        FoldedRecord::Proposal(row) => Ok(row),
        _ => Err(Failure::internal()),
    }
}

fn read_error(error: RecordReadError) -> Failure {
    match error {
        RecordReadError::Refused(refusal) => Failure::refused(refusal),
        _ => Failure::internal(),
    }
}

fn path(row: &ProposalRecord) -> Result<Option<&str>, Failure> {
    let Some(path) = row.legacy_id.as_deref() else {
        return Ok(None);
    };
    let (family, file) = path
        .strip_prefix("proposals/")
        .and_then(|relative| relative.split_once('/'))
        .ok_or_else(Failure::internal)?;
    let (kind, extension) = match family {
        "edges" => (ProposalKind::RelationAssert, ".md"),
        "atoms" => (ProposalKind::ConceptMarkdown, ".md"),
        "bodies" => (ProposalKind::ConceptMarkdown, ".json"),
        _ => return Err(Failure::internal()),
    };
    if row.kind != kind
        || file.len() > 255
        || file.starts_with('.')
        || file.contains("..")
        || !file.ends_with(extension)
        || !file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Failure::internal());
    }
    Ok(Some(path))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Basis {
    kb_uuid: Decimal<KbUuid>,
    version: Decimal<u64>,
    manifest_sha256: Digest,
}

#[derive(Serialize)]
struct File<'a> {
    path: &'a str,
    status: &'static str,
    payload: &'a str,
    key: Key,
    revision: Decimal<u64>,
}

#[derive(Serialize)]
struct Key {
    kind: u16,
    id: Decimal<u128>,
}

impl From<RecordKey> for Key {
    fn from(key: RecordKey) -> Self {
        Self {
            kind: key.kind().as_u16(),
            id: Decimal(key.id()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeFile<'a> {
    directory: &'static str,
    filename: &'a str,
    status: &'static str,
    key: Key,
    revision: Decimal<u64>,
    subject_slug: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    edge: Option<Edge<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    atom: Option<Atom<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Body<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Edge<'a> {
    target_slug: &'a str,
    relation: &'static str,
    confidence: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Atom<'a> {
    current_atom: &'a str,
    proposed_atom: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Body<'a> {
    expected_revision: Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    proposed_body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_bytes: Option<Decimal<u64>>,
    #[serde(skip)]
    tail: Option<&'a str>,
    reason: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Reply<'a> {
    basis: Basis,
    files: Vec<File<'a>>,
    examples: Vec<Example<'a>>,
    native_files: Vec<NativeFile<'a>>,
}
