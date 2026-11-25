//! Read-only projections of the durable native proposal and frontier queues.

use std::fmt::{self, Display, Formatter};
use std::io;
use std::net::TcpStream;

use sage_db_kernel::{claims::CommandError, concept, relations, term_create};
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::{
    CommittedBlob, RecordImage, RecordImageLookup, RecordLookup, RecordReadError, RecordScanError,
    ViewBasis,
};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    FrontierEntryRecord, FrontierStatus, ProposalKind, ProposalRecord, ProposalStatus, RecordKey,
    RecordKind,
};
use serde::{Serialize, Serializer, ser::SerializeSeq};

use crate::activation::{
    Format, JSON, TEXT, format, limit, parameter, parts, push, render_error, reserve, sort, step,
};
use crate::http::{self, Deadline, Failure, Head, Route};
use crate::operator_read::{Decimal, Digest};

const MAX_ROWS: usize = 512;
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

pub(crate) fn serve(
    store: &mut Store<impl FileSystem>,
    basis: ViewBasis,
    head: &Head<'_>,
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    match head.route {
        Route::Frontier | Route::NativeFrontier => {
            frontier(store, basis, head, stream, deadline, ledger)
        }
        Route::Proposals | Route::KnowledgeProposals | Route::Proposal => {
            proposals(store, basis, head, stream, deadline, ledger)
        }
        _ => Err(Failure::internal()),
    }
}

fn bytes(
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
    mime: &'static str,
    write: impl FnOnce(&mut dyn io::Write) -> io::Result<()>,
) -> Result<(), Failure> {
    http::respond_with(stream, "200 OK", mime, deadline, ledger, write).map_err(render_error)
}

fn json(out: &mut dyn io::Write, value: &impl Serialize, pretty: bool) -> io::Result<()> {
    if pretty {
        serde_json::to_writer_pretty(&mut *out, value)?;
    } else {
        serde_json::to_writer(&mut *out, value)?;
    }
    out.write_all(b"\n")
}

fn scan_error(error: RecordScanError<Failure>) -> Failure {
    match error {
        RecordScanError::Visitor(error) => error,
        RecordScanError::Read(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
        RecordScanError::Incomplete(_) => Failure::unavailable("no complete readable publication"),
        RecordScanError::Read(_) => Failure::internal(),
    }
}

fn read_error(error: RecordReadError) -> Failure {
    match error {
        RecordReadError::Refused(refusal) => Failure::refused(refusal),
        _ => Failure::internal(),
    }
}

fn add<T>(items: &mut Vec<T>, item: T, ledger: &mut dyn SharedLedger) -> Result<(), Failure> {
    if items.len() == MAX_ROWS {
        return Err(limit("queue exceeds its row ceiling"));
    }
    push(items, item, ledger)
}

// Mapping stores the audit source id and the original pipe-separated row in
// separate authenticated values. Only the legacy concept table has the old
// four-column meaning. Neither the other imported tables nor free-form native
// frontier entries are silently recast as input to that older table.
fn legacy_ordinal(record: &FrontierEntryRecord) -> Option<u32> {
    let reason = record.common.reason.as_deref()?;
    if record.revision.get() != 1 {
        return None;
    }
    let imported = reason.strip_prefix("legacy import of 未决概念/0/")?;
    let (ordinal, _) = imported.split_once(" from frontier row ")?;
    ordinal.parse().ok()
}

#[derive(Serialize)]
struct LegacyRow<'a> {
    concept: &'a str,
    source: &'a str,
    reason: &'a str,
    priority: &'a str,
}

fn legacy_row(text: &str, ordinal: u32) -> Result<LegacyRow<'_>, Failure> {
    let (prefix, raw) = text.split_once(": ").ok_or_else(Failure::internal)?;
    let from_body = prefix
        .strip_prefix("未决概念/0/")
        .and_then(|v| v.parse::<u32>().ok());
    if from_body != Some(ordinal) || !raw.starts_with('|') {
        return Err(Failure::internal());
    }
    let mut cells = raw
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty());
    Ok(LegacyRow {
        concept: cells.next().ok_or_else(Failure::internal)?,
        source: cells.next().ok_or_else(Failure::internal)?,
        reason: cells.next().ok_or_else(Failure::internal)?,
        priority: cells.next().ok_or_else(Failure::internal)?,
    })
}

struct FrontierOwner {
    image: RecordImage,
    text: CommittedBlob,
    legacy_order: Option<u32>,
}

impl FrontierOwner {
    fn row(&self) -> Result<&FrontierEntryRecord, Failure> {
        match self.image.record() {
            FoldedRecord::FrontierEntry(record) => Ok(record),
            _ => Err(Failure::internal()),
        }
    }

    fn text(&self) -> Result<&str, Failure> {
        std::str::from_utf8(self.text.as_bytes()).map_err(|_| Failure::internal())
    }
}

fn frontier(
    store: &mut Store<impl FileSystem>,
    basis: ViewBasis,
    head: &Head<'_>,
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let legacy = matches!(head.route, Route::Frontier);
    let mut keys = reserve(MAX_ROWS, ledger)?;
    let observed = store
        .scan_current_records(ledger, |record, work| {
            if let FoldedRecord::FrontierEntry(row) = record {
                let order = legacy_ordinal(row);
                if !legacy || order.is_some() {
                    add(&mut keys, (row.key, order), work)?;
                }
            }
            Ok(())
        })
        .map_err(scan_error)?;
    if observed != basis {
        return Err(Failure::internal());
    }
    sort(&mut keys, |a, b| (a.1, a.0.id()) < (b.1, b.0.id()), ledger)?;
    let mut owners = reserve(keys.len(), ledger)?;
    let mut remaining = MAX_SOURCE_BYTES;
    let mut last_order = None;
    for (key, order) in keys {
        step(ledger)?;
        if order.is_some() && order == last_order {
            return Err(Failure::internal());
        }
        last_order = order;
        let RecordImageLookup::Found {
            image,
            basis: served,
        } = store.record_owned(key, ledger).map_err(read_error)?
        else {
            return Err(Failure::internal());
        };
        let FoldedRecord::FrontierEntry(row) = image.record() else {
            return Err(Failure::internal());
        };
        if served != basis {
            return Err(Failure::internal());
        }
        let blob = store
            .read_committed_blob(row.text_ref, remaining, ledger)
            .map_err(crate::runtime::blob_error)?;
        if blob.basis() != basis {
            return Err(Failure::internal());
        }
        remaining = remaining
            .checked_sub(u64::try_from(blob.len()).map_err(|_| Failure::internal())?)
            .ok_or_else(Failure::internal)?;
        push(
            &mut owners,
            FrontierOwner {
                image,
                text: blob,
                legacy_order: order,
            },
            ledger,
        )?;
    }
    let text = format(head) == Format::Text;
    if legacy {
        let mut rows = reserve(owners.len(), ledger)?;
        for owner in &owners {
            let ordinal = owner.legacy_order.ok_or_else(Failure::internal)?;
            let row = legacy_row(owner.text()?, ordinal)?;
            push(&mut rows, row, ledger)?;
        }
        return bytes(
            stream,
            deadline,
            ledger,
            if text { TEXT } else { JSON },
            |out| {
                if text && rows.is_empty() {
                    out.write_all(b"No frontier entries.\n")
                } else {
                    json(out, &rows, text)
                }
            },
        );
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Row<'a> {
        id: Decimal<u128>,
        revision: Decimal<u64>,
        status: &'static str,
        priority: &'static str,
        concept_refs: Keys<'a>,
        text: &'a str,
    }
    let mut rows = reserve(owners.len(), ledger)?;
    for owner in &owners {
        let row = owner.row()?;
        let status = match row.status {
            FrontierStatus::Open => "open",
            FrontierStatus::Resolved => "resolved",
        };
        let priority = ["high", "medium", "low"]
            .get(usize::from(row.priority))
            .ok_or_else(Failure::internal)?;
        push(
            &mut rows,
            Row {
                id: Decimal(row.key.id()),
                revision: Decimal(row.revision.get()),
                status,
                priority,
                concept_refs: Keys(&row.concept_refs),
                text: owner.text()?,
            },
            ledger,
        )?;
    }
    bytes(
        stream,
        deadline,
        ledger,
        if text { TEXT } else { JSON },
        |out| json(out, &rows, text),
    )
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
struct Baseline {
    key: Key,
    revision: Decimal<u64>,
}

struct Keys<'a>(&'a [RecordKey]);

impl Serialize for Keys<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for key in self.0 {
            seq.serialize_element(&Key::from(*key))?;
        }
        seq.end()
    }
}

pub(crate) fn status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Approved => "approved",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Conflicted => "conflicted",
    }
}

fn proposal_kind(kind: ProposalKind) -> &'static str {
    match kind {
        ProposalKind::ConceptMarkdown => "concept-markdown",
        ProposalKind::ClaimCreate => "claim-create",
        ProposalKind::ClaimEdit => "claim-edit",
        ProposalKind::EvidenceAttach => "evidence-attach",
        ProposalKind::RelationAssert => "relation-assert",
        ProposalKind::TermCreate => "term-create",
    }
}

struct ProposalId(u128);

impl Display for ProposalId {
    fn fmt(&self, out: &mut Formatter<'_>) -> fmt::Result {
        write!(out, "prp_{}", self.0)
    }
}

impl Serialize for ProposalId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

enum Subject<'a> {
    Term(&'a str),
    Claim(u128),
    ReservedTerm(u128),
}

impl Display for Subject<'_> {
    fn fmt(&self, out: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term(slug) => out.write_str(slug),
            Self::Claim(id) => write!(out, "claim:{id}"),
            Self::ReservedTerm(id) => write!(out, "term:{id}"),
        }
    }
}

impl Serialize for Subject<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Proposal<'a> {
    id: ProposalId,
    kind: &'static str,
    subject: Subject<'a>,
    subject_key: Key,
    revision: Decimal<u64>,
    status: &'static str,
    author: &'a str,
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_id: Option<&'a str>,
    review_note: Option<&'a str>,
}

fn proposal_row<'a>(
    store: &'a Store<impl FileSystem>,
    basis: ViewBasis,
    row: &'a ProposalRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<Proposal<'a>, Failure> {
    let subject = if row.kind == ProposalKind::TermCreate && row.status != ProposalStatus::Approved
    {
        Subject::ReservedTerm(row.subject.id())
    } else {
        match store.record(row.subject, ledger).map_err(read_error)? {
            RecordLookup::Found {
                record: FoldedRecord::Term(term),
                basis: served,
            } if served == basis => Subject::Term(&term.slug),
            RecordLookup::Found {
                record: FoldedRecord::Claim(_),
                basis: served,
            } if served == basis => Subject::Claim(row.subject.id()),
            _ => return Err(Failure::internal()),
        }
    };
    Ok(Proposal {
        id: ProposalId(row.key.id()),
        kind: proposal_kind(row.kind),
        subject,
        subject_key: row.subject.into(),
        revision: Decimal(row.revision.get()),
        status: status_name(row.status),
        author: &row.declared_author,
        reason: row.common.reason.as_deref(),
        legacy_id: row.legacy_id.as_deref(),
        review_note: row.review_note.as_deref(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposalPreview<'a> {
    kb_uuid: Decimal<sage_db_types::identity::KbUuid>,
    payload_sha256: Digest,
    proposal: Proposal<'a>,
    preview: Preview<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Preview<'a> {
    ConceptMarkdown {
        atom: Option<&'a str>,
        body: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
    },
    TermCreate {
        slug: &'a str,
        atom: &'a str,
        body: &'a str,
        layer: u8,
        domains: Vec<String>,
        confidence: &'static str,
        source: &'static str,
        triggers: Vec<String>,
        reason: &'a str,
    },
    RelationAssert {
        from: Key,
        target: Key,
        #[serde(rename = "targetSlug")]
        target_slug: &'a str,
        relation: &'static str,
        confidence: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        replaces: Option<Baseline>,
    },
}

fn proposal_preview<'a>(
    store: &'a Store<impl FileSystem>,
    basis: ViewBasis,
    row: &ProposalRecord,
    content: &'a CommittedBlob,
    ledger: &mut AttemptLedger<'_>,
) -> Result<Preview<'a>, Failure> {
    let unsupported = || {
        Failure::new(
            "422 Unprocessable Content",
            ErrorCode::Unsupported,
            "This proposal has no native review preview",
        )
    };
    if row.legacy_id.is_some() {
        return Err(unsupported());
    }
    let command_failure = |error| match error {
        CommandError::Refused(refusal) => Failure::refused(refusal),
        _ => Failure::internal(),
    };
    match row.kind {
        ProposalKind::ConceptMarkdown => {
            let edit = concept::decode_stored_intent(row, content.as_bytes(), ledger).map_err(
                |error| match error {
                    concept::ConceptTextError::Command(error) => command_failure(error),
                    _ => Failure::internal(),
                },
            )?;
            Ok(Preview::ConceptMarkdown {
                atom: edit.atom,
                body: edit.body,
                reason: edit.reason,
            })
        }
        ProposalKind::TermCreate => {
            let draft =
                term_create::decode_stored(row, content.as_bytes(), ledger).map_err(|error| {
                    match error {
                        term_create::CreateError::Command(error) => command_failure(error),
                        _ => Failure::internal(),
                    }
                })?;
            Ok(Preview::TermCreate {
                slug: draft.slug,
                atom: draft.atom,
                body: draft.body,
                layer: draft.layer.get(),
                domains: draft.domains,
                confidence: draft.confidence.as_str(),
                source: crate::activation::source_label(draft.source),
                triggers: draft.triggers,
                reason: draft.reason,
            })
        }
        ProposalKind::RelationAssert => {
            let edit = relations::decode_stored_intent(row, content.as_bytes(), ledger).map_err(
                |error| match error {
                    relations::RelationAssertError::Command(error) => command_failure(error),
                    _ => Failure::internal(),
                },
            )?;
            let target_slug = match store.record(edit.target, ledger).map_err(read_error)? {
                RecordLookup::Found {
                    record: FoldedRecord::Term(term),
                    basis: served,
                } if served == basis => term.slug.as_str(),
                _ => return Err(Failure::internal()),
            };
            Ok(Preview::RelationAssert {
                from: edit.from.into(),
                target: edit.target.into(),
                target_slug,
                relation: edit.relation.as_str(),
                confidence: edit.confidence.as_str(),
                replaces: row.base_revisions.get(2).map(|old| Baseline {
                    key: old.key.into(),
                    revision: Decimal(old.revision.get()),
                }),
            })
        }
        _ => Err(unsupported()),
    }
}

fn proposals(
    store: &mut Store<impl FileSystem>,
    basis: ViewBasis,
    head: &Head<'_>,
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let (path, query) = parts(head);
    if matches!(head.route, Route::Proposal) {
        let id = path
            .rsplit('/')
            .next()
            .and_then(|value| value.strip_prefix("prp_"))
            .and_then(|value| value.parse::<u128>().ok())
            .filter(|&value| value != 0)
            .ok_or_else(|| Failure::bad("Invalid native proposal id"))?;
        let key = RecordKey::new(RecordKind::Proposal, id);
        let row = match store.record(key, ledger).map_err(read_error)? {
            RecordLookup::Found {
                record: FoldedRecord::Proposal(row),
                basis: served,
            } if served == basis => row,
            RecordLookup::Absent { basis: served } if served == basis => {
                return Err(Failure::new(
                    "404 Not Found",
                    ErrorCode::InvalidArgument,
                    "Proposal not found",
                ));
            }
            RecordLookup::Incomplete(_) => {
                return Err(Failure::unavailable("no complete readable publication"));
            }
            _ => return Err(Failure::internal()),
        };
        if parameter(query, "preview") == Some("true") {
            let payload = row.payload_ref;
            let content = store
                .read_committed_blob(payload, sage_db_store::typed::MAX_FRAME_BYTES, ledger)
                .map_err(crate::runtime::blob_error)?;
            if content.basis() != basis {
                return Err(Failure::internal());
            }
            let row = match store.record(key, ledger).map_err(read_error)? {
                RecordLookup::Found {
                    record: FoldedRecord::Proposal(row),
                    basis: served,
                } if served == basis && row.payload_ref == payload => row,
                _ => return Err(Failure::internal()),
            };
            let preview = proposal_preview(store, basis, row, &content, ledger)?;
            let proposal = proposal_row(store, basis, row, ledger)?;
            return bytes(stream, deadline, ledger, JSON, |out| {
                json(
                    out,
                    &ProposalPreview {
                        kb_uuid: Decimal(basis.kb_uuid),
                        payload_sha256: Digest(payload),
                        proposal,
                        preview,
                    },
                    false,
                )
            });
        }
        let row = proposal_row(store, basis, row, ledger)?;
        return bytes(stream, deadline, ledger, JSON, |out| json(out, &row, false));
    }
    let status = parameter(query, "status");
    if status
        .is_some_and(|status| !matches!(status, "pending" | "approved" | "rejected" | "conflicted"))
    {
        return Err(Failure::bad("Invalid proposal status"));
    }
    let mut records = reserve(MAX_ROWS, ledger)?;
    let observed = store
        .scan_current_records(ledger, |record, work| {
            if let FoldedRecord::Proposal(row) = record
                && status.is_none_or(|want| want == status_name(row.status))
            {
                add(&mut records, row, work)?;
            }
            Ok(())
        })
        .map_err(scan_error)?;
    if observed != basis {
        return Err(Failure::internal());
    }
    sort(
        &mut records,
        |a, b| {
            a.common.created_at > b.common.created_at
                || (a.common.created_at == b.common.created_at && a.key.id() < b.key.id())
        },
        ledger,
    )?;
    let mut rows = reserve(records.len(), ledger)?;
    for record in records {
        let row = proposal_row(store, basis, record, ledger)?;
        push(&mut rows, row, ledger)?;
    }
    if format(head) == Format::Text {
        return bytes(stream, deadline, ledger, TEXT, |out| {
            if rows.is_empty() {
                return out.write_all(b"No knowledge proposals.\n");
            }
            for row in &rows {
                writeln!(
                    out,
                    "{:<10} {:<18} {}  [{}]",
                    row.status, row.kind, row.subject, row.id
                )?;
            }
            Ok(())
        });
    }
    if matches!(head.route, Route::Proposals) {
        #[derive(Serialize)]
        struct LegacyReply<'a> {
            authority: &'static str,
            proposals: &'a [Proposal<'a>],
            edges: [(); 0],
            atoms: [(); 0],
            bodies: [(); 0],
        }
        return bytes(stream, deadline, ledger, JSON, |out| {
            json(
                out,
                &LegacyReply {
                    authority: "structured",
                    proposals: &rows,
                    edges: [],
                    atoms: [],
                    bodies: [],
                },
                false,
            )
        });
    }
    #[derive(Serialize)]
    struct Reply<'a> {
        proposals: &'a [Proposal<'a>],
    }
    bytes(stream, deadline, ledger, JSON, |out| {
        json(out, &Reply { proposals: &rows }, false)
    })
}

#[cfg(test)]
mod tests {
    use super::legacy_row;

    #[test]
    fn legacy_row_requires_bound_source_ordinal_and_preserves_spelling() {
        let row = legacy_row("未决概念/0/3: | A | 来源 | 理由 | 高 |", 3)
            .ok()
            .unwrap();
        assert_eq!(
            (row.concept, row.source, row.reason, row.priority),
            ("A", "来源", "理由", "高")
        );
        assert!(legacy_row("未决概念/0/4: | A | 来源 | 理由 | 高 |", 3).is_err());
        assert!(legacy_row("概念内张力/0/3: | A | 来源 | 理由 | 高 |", 3).is_err());
    }
}
