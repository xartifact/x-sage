//! The legacy concept activation projection over one native publication.

use std::io::{self, Write};
use std::net::TcpStream;

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::catalog::{CatalogError, SlugResolution, SlugScanError, resolve_term_slug};
use sage_db_kernel::snapshot::{Expectation, Snapshot, SnapshotBuildError};
use sage_db_kernel::term_read::{TermRead, TermReadError, read_term};
use sage_db_protocol::generated::ErrorCode;
use sage_db_query::{Answer, FactQuery, FactQueryError, answer_relation};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::typed::DigestAssemblyError;
use sage_db_store::view::{
    CommitReadError, RecordLookup, RecordReadError, RecordScanError, ViewBasis,
};
use sage_db_types::budget::Budget;
use sage_db_types::framing::Refusal;
use sage_db_types::identity::TermId;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{AssertionRecordFull, RecordKey, TermRecord, TermSource};
use sage_db_types::query::QueryError;
use sage_db_types::relation::RelationId;
use sage_db_types::snapshot::{SnapshotOpenError, SnapshotView};
use serde::{Serialize, Serializer};

use crate::http::{self, Deadline, Failure, Head, Route};
use crate::operator_read::{Decimal, Digest};

pub(crate) const MAX_TERMS: usize = 512;
const MAX_ASSERTIONS: usize = 8192;
const MAX_DEPTH: u32 = 64;
pub(crate) const JSON: &str = "application/json; charset=utf-8";
pub(crate) const TEXT: &str = "text/plain; charset=utf-8";
pub(crate) const MARKDOWN: &str = "text/markdown; charset=utf-8";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Json,
    Text,
    Markdown,
}

pub(crate) fn parts<'a>(head: &Head<'a>) -> (&'a str, &'a str) {
    head.target.split_once('?').unwrap_or((head.target, ""))
}

pub(crate) fn parameter<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|segment| segment.split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

pub(crate) fn format(head: &Head<'_>) -> Format {
    match parameter(parts(head).1, "format") {
        Some("markdown") => Format::Markdown,
        Some("text" | "ai" | "human") => Format::Text,
        Some("json" | "manifest") => Format::Json,
        _ => {
            let accept = head
                .accept
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or("");
            if accept.contains("text/markdown")
                || accept.contains("text/plain") && !accept.contains("json")
            {
                Format::Text
            } else {
                Format::Json
            }
        }
    }
}

pub(crate) fn wants_text(head: &Head<'_>) -> bool {
    format(head) != Format::Json || parameter(parts(head).1, "format") == Some("manifest")
}

#[derive(Clone, Copy)]
enum Strategy {
    Exploration,
    Validation,
    Generation,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Self::Exploration => "exploration",
            Self::Validation => "validation",
            Self::Generation => "generation",
        }
    }

    fn follows(self, relation: RelationId) -> bool {
        matches!(relation, RelationId::DependsOn | RelationId::Enables)
            || match self {
                Self::Exploration => {
                    matches!(relation, RelationId::Contradicts | RelationId::AnalogousTo)
                }
                Self::Validation => relation == RelationId::Contradicts,
                Self::Generation => relation == RelationId::Instantiates,
            }
    }
}

struct Options<'a> {
    format: Format,
    kb: Option<&'a str>,
    strategy: Option<Strategy>,
    depth: u32,
}

fn options<'a>(head: &'a Head<'_>) -> Result<Options<'a>, Failure> {
    let query = parts(head).1;
    let strategy = match parameter(query, "strategy") {
        None => None,
        Some("exploration") => Some(Strategy::Exploration),
        Some("validation") => Some(Strategy::Validation),
        Some("generation") => Some(Strategy::Generation),
        Some(_) => return Err(Failure::bad("invalid activation strategy")),
    };
    let depth = match parameter(query, "depth") {
        None => {
            if strategy.is_some() {
                2
            } else {
                1
            }
        }
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|depth| (1..=MAX_DEPTH).contains(depth))
            .ok_or_else(|| Failure::bad("activation depth must be in 1..=64"))?,
    };
    Ok(Options {
        format: format(head),
        kb: parameter(query, "kb"),
        strategy,
        depth,
    })
}

pub(crate) fn decode_component<'a>(
    raw: &str,
    buffer: &'a mut [u8],
    form_value: bool,
) -> Result<&'a str, Failure> {
    let bad = || Failure::bad("invalid URL component");
    let mut input = raw.bytes();
    let mut written = 0;
    while let Some(byte) = input.next() {
        let value = match byte {
            b'%' => {
                let high = input.next().and_then(hex_digit).ok_or_else(bad)?;
                let low = input.next().and_then(hex_digit).ok_or_else(bad)?;
                high * 16 + low
            }
            b'+' if form_value => b' ',
            _ => byte,
        };
        *buffer.get_mut(written).ok_or_else(bad)? = value;
        written += 1;
    }
    std::str::from_utf8(&buffer[..written]).map_err(|_| bad())
}

pub(crate) fn active_slug<F: FileSystem>(
    store: &Store<F>,
    grants: &Grants<'_>,
    slug: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<Option<(RecordKey, ViewBasis)>, Failure> {
    match resolve_term_slug(store, grants, slug, ledger).map_err(catalog_error)? {
        SlugResolution::Found { term, basis } if !term.retired => Ok(Some((term.key, basis))),
        SlugResolution::Absent { .. } | SlugResolution::Found { .. } => Ok(None),
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn serve<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    head: &Head<'_>,
    socket: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(crate::runtime::auth_error)?;
    let options = options(head)?;
    let mut slug_buffer = [0u8; 64];
    let raw_slug = parts(head)
        .0
        .strip_prefix("/api/concepts/")
        .ok_or_else(Failure::internal)?;
    let slug = decode_component(raw_slug, &mut slug_buffer, false)?;
    let Some((key, basis)) = active_slug(store, grants, slug, ledger)? else {
        missing(socket, slug, wants_text(head));
        return Ok(());
    };
    let TermRead::Found { record, body } =
        read_term(store, grants, key, http::MAX_BODY as u64, ledger).map_err(term_error)?
    else {
        return Err(Failure::internal());
    };
    if body.basis() != basis {
        return Err(Failure::internal());
    }
    let FoldedRecord::Term(term) = record.record() else {
        return Err(Failure::internal());
    };
    let body = std::str::from_utf8(body.as_bytes()).map_err(|_| Failure::internal())?;
    let snapshot = Snapshot::open(store, Expectation::at(basis.kb_uuid, basis.version), ledger)
        .map_err(snapshot_error)?;
    if snapshot
        .store()
        .view()
        .is_none_or(|view| view.basis() != basis)
    {
        return Err(Failure::internal());
    }
    let mut graph = Projection::collect(&snapshot, ledger)?;
    graph.check_kb(options.kb)?;
    let origin = graph.index(key, ledger)?;
    let mime = match options.format {
        Format::Json => JSON,
        Format::Text => TEXT,
        Format::Markdown => MARKDOWN,
    };
    if options.format == Format::Markdown {
        return http::respond_with(socket, "200 OK", mime, control, ledger, |out| {
            markdown(out, term, body, &graph, origin)?;
            out.write_all(b"\n")
        })
        .map_err(render_error);
    }

    let mut query_budget = Budget::DEFAULT;
    if options.strategy.is_some() {
        for from in 0..graph.nodes.len() {
            graph.evaluate(&snapshot, from, &mut query_budget, control, ledger)?;
        }
    }
    let visited = graph.visit(origin, &options, ledger)?;
    if options.strategy.is_none() {
        for &from in &visited {
            graph.evaluate(&snapshot, from, &mut query_budget, control, ledger)?;
        }
    }
    let mut markdown_buffer = [0; http::MAX_RESPONSE];
    let markdown = if options.format == Format::Json {
        let raw = http::render(&mut markdown_buffer, ledger, |out| {
            markdown(out, term, body, &graph, origin)
        })
        .map_err(render_error)?;
        Some(std::str::from_utf8(raw).map_err(|_| Failure::internal())?)
    } else {
        None
    };
    let dto = graph.response(origin, &visited, body, markdown, &options, ledger)?;
    http::respond_with(socket, "200 OK", mime, control, ledger, |out| {
        if options.format == Format::Json {
            serde_json::to_writer(out, &dto).map_err(io::Error::from)
        } else {
            text(out, &dto)
        }
    })
    .map_err(render_error)
}

/// Serialize the database record without constructing the product document or a graph.
pub(crate) fn serve_term_text<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    slug: &str,
    head: &Head<'_>,
    socket: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let mode = head.route;
    let raw = head.accept == Some(b"application/octet-stream");
    if slug.is_empty() || slug.len() > sage_db_store::typed::MAX_SLUG_BYTES as usize {
        return Err(Failure::bad("invalid term slug length"));
    }
    let (key, basis) = active_slug(store, grants, slug, ledger)?.ok_or(Failure::new(
        "404 Not Found",
        ErrorCode::InvalidArgument,
        "Term text not found",
    ))?;
    let TermRead::Found { record, body } =
        read_term(store, grants, key, http::MAX_BODY as u64, ledger).map_err(term_error)?
    else {
        return Err(Failure::internal());
    };
    if body.basis() != basis {
        return Err(Failure::internal());
    }
    let FoldedRecord::Term(term) = record.record() else {
        return Err(Failure::internal());
    };
    let body = std::str::from_utf8(body.as_bytes()).map_err(|_| Failure::internal())?;

    // Borrow the canonical record and blob until the response has been metered and encoded.
    // The generated response is exercised at the HTTP boundary.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Basis {
        kb_uuid: Decimal<sage_db_types::identity::KbUuid>,
        version: Decimal<u64>,
        manifest_sha256: Digest,
    }
    #[derive(Serialize)]
    struct Key {
        kind: u16,
        id: Decimal<u128>,
    }
    #[derive(Serialize)]
    struct Reply<'a> {
        basis: Basis,
        key: Key,
        revision: Decimal<u64>,
        slug: &'a str,
        atom: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<&'a str>,
    }
    let reply = Reply {
        basis: Basis {
            kb_uuid: Decimal(basis.kb_uuid),
            version: Decimal(basis.version.get()),
            manifest_sha256: Digest(basis.manifest_sha256),
        },
        key: Key {
            kind: term.key.kind() as u16,
            id: Decimal(term.key.id()),
        },
        revision: Decimal(term.revision.get()),
        slug: &term.slug,
        atom: &term.atom,
        body: (!raw).then_some(body),
    };
    let raw_body = raw.then_some(body.as_bytes());
    if matches!(mode, Route::ReadTermText) {
        return respond_term(socket, &reply, raw_body, control, ledger);
    }
    if matches!(mode, Route::ReadTermActivation) {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Activation<'a> {
            text: Reply<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            original: Option<crate::import_source::Original<'a>>,
        }
        let package = crate::import_source::package(store, term, basis, ledger)?;
        let original = package
            .as_ref()
            .map(|package| crate::import_source::extract(package, term, ledger))
            .transpose()?;
        return respond_term(
            socket,
            &Activation {
                text: reply,
                original,
            },
            raw_body,
            control,
            ledger,
        );
    }

    #[derive(Serialize)]
    struct Document<'a> {
        text: Reply<'a>,
        layer: u8,
        domains: &'a [String],
        confidence: &'static str,
        source: &'static str,
        triggers: &'a [String],
        assertions: Vec<DocumentAssertion<'a>>,
    }
    let assertions = document_assertions(store, key, basis, ledger)?;
    respond_term(
        socket,
        &Document {
            text: reply,
            layer: term.layer.get(),
            domains: &term.domains,
            confidence: term.confidence.as_str(),
            source: source_label(term.source),
            triggers: &term.triggers,
            assertions,
        },
        raw_body,
        control,
        ledger,
    )
}

fn respond_term(
    socket: &TcpStream,
    reply: &impl Serialize,
    raw: Option<&[u8]>,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    let Some(raw) = raw else {
        return http::respond(socket, reply, control, ledger).map_err(render_error);
    };
    let mut scratch = [0u8; http::MAX_RESPONSE];
    let metadata = http::render(&mut scratch, ledger, |out| {
        serde_json::to_writer(out, reply).map_err(io::Error::from)
    })
    .map_err(render_error)?;
    let size = u32::try_from(metadata.len()).map_err(|_| Failure::internal())?;
    ledger
        .try_charge(u64::try_from(raw.len()).map_err(|_| Failure::internal())?)
        .map_err(|error| Failure::refused(sage_db_types::framing::Refusal::Budget(error)))?;
    step(ledger)?;
    http::respond_octets(
        socket,
        "200 OK",
        &[&size.to_le_bytes(), metadata, raw],
        control,
    );
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentAssertion<'a> {
    key: DocumentAssertionKey,
    revision: Decimal<u64>,
    relation: &'static str,
    target_slug: &'a str,
    confidence: &'static str,
    declaration_order: u32,
}

#[derive(Serialize)]
struct DocumentAssertionKey {
    kind: u16,
    id: Decimal<u128>,
}

fn document_assertions<'a, F: FileSystem>(
    store: &'a Store<F>,
    from: RecordKey,
    expected: ViewBasis,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<DocumentAssertion<'a>>, Failure> {
    let mut assertions = Vec::new();
    let basis = store
        .scan_identified_records(ledger, |row, work| {
            let FoldedRecord::Assertion(assertion) = row else {
                return Ok(());
            };
            if assertion.from != from || assertion.retracted {
                return Ok(());
            }
            if assertions.len() == MAX_ASSERTIONS {
                return Err(limit("document assertions exceed the response ceiling"));
            }
            let target = store
                .record(assertion.target, work)
                .map_err(|error| match error {
                    RecordReadError::Refused(refusal) => Failure::refused(refusal),
                    _ => Failure::internal(),
                })?;
            let RecordLookup::Found {
                record: FoldedRecord::Term(term),
                basis,
            } = target
            else {
                return Err(Failure::internal());
            };
            if basis != expected || term.retired {
                return Err(Failure::internal());
            }

            // Retain only actual outgoing rows. Admit the replacement table
            // before allocating it and charge each move of an existing slot.
            if assertions.len() == assertions.capacity() {
                let next = (2 * assertions.capacity().max(2)).min(MAX_ASSERTIONS);
                let mut grown = reserve(next, work)?;
                for row in std::mem::take(&mut assertions) {
                    push(&mut grown, row, work)?;
                }
                assertions = grown;
            }
            push(
                &mut assertions,
                DocumentAssertion {
                    key: DocumentAssertionKey {
                        kind: assertion.key.kind() as u16,
                        id: Decimal(assertion.key.id()),
                    },
                    revision: Decimal(assertion.revision.get()),
                    relation: assertion.relation.as_str(),
                    target_slug: &term.slug,
                    confidence: assertion.confidence.as_str(),
                    declaration_order: assertion.declaration_order,
                },
                work,
            )
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(failure) => failure,
            RecordScanError::Read(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
            RecordScanError::Read(_) => Failure::internal(),
        })?;
    if basis != expected {
        return Err(Failure::internal());
    }
    sort(
        &mut assertions,
        |a, b| (a.declaration_order, a.key.id.0) < (b.declaration_order, b.key.id.0),
        ledger,
    )?;
    Ok(assertions)
}

pub(crate) fn serve_body<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    basis: ViewBasis,
    head: &Head<'_>,
    socket: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let raw_slug = parts(head)
        .0
        .strip_prefix("/api/concepts/")
        .and_then(|path| path.strip_suffix("/body"))
        .ok_or_else(Failure::internal)?;
    let mut slug_buffer = [0; 64];
    let slug = decode_component(raw_slug, &mut slug_buffer, false)?;
    let format = format(head);
    let Some((key, served)) = active_slug(store, grants, slug, ledger)? else {
        missing(socket, slug, format != Format::Json);
        return Ok(());
    };
    if served != basis {
        return Err(Failure::internal());
    }
    let TermRead::Found { record, body } =
        read_term(store, grants, key, http::MAX_BODY as u64, ledger).map_err(term_error)?
    else {
        return Err(Failure::internal());
    };
    if body.basis() != basis {
        return Err(Failure::internal());
    }
    if format != Format::Json {
        return http::respond_with(socket, "200 OK", MARKDOWN, control, ledger, |out| {
            out.write_all(body.as_bytes())
        })
        .map_err(render_error);
    }

    let FoldedRecord::Term(term) = record.record() else {
        return Err(Failure::internal());
    };
    let body = std::str::from_utf8(body.as_bytes()).map_err(|_| Failure::internal())?;
    let snapshot = Snapshot::open(store, Expectation::at(basis.kb_uuid, basis.version), ledger)
        .map_err(snapshot_error)?;
    if snapshot
        .store()
        .view()
        .is_none_or(|view| view.basis() != basis)
    {
        return Err(Failure::internal());
    }
    let graph = Projection::collect(&snapshot, ledger)?;
    let origin = graph.index(key, ledger)?;
    let mut buffer = [0; http::MAX_RESPONSE];
    let document = http::render(&mut buffer, ledger, |out| {
        markdown(out, term, body, &graph, origin)
    })
    .map_err(render_error)?;
    let document = std::str::from_utf8(document).map_err(|_| Failure::internal())?;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Body<'a> {
        concept_id: &'a str,
        kb_uuid: Decimal<sage_db_types::identity::KbUuid>,
        term_id: Decimal<u128>,
        revision: Decimal<u64>,
        version: Decimal<u64>,
        atom: &'a str,
        body: &'a str,
        document: &'a str,
        structured: bool,
    }
    let result = Body {
        concept_id: &term.slug,
        kb_uuid: Decimal(basis.kb_uuid),
        term_id: Decimal(term.key.id()),
        revision: Decimal(term.revision.get()),
        version: Decimal(basis.version.get()),
        atom: &term.atom,
        body,
        document,
        structured: true,
    };
    http::respond_with(socket, "200 OK", JSON, control, ledger, |out| {
        serde_json::to_writer(&mut *out, &result)?;
        out.write_all(b"\n")
    })
    .map_err(render_error)
}

pub(crate) fn missing(socket: &TcpStream, slug: &str, text: bool) {
    let failure = Failure::new(
        "404 Not Found",
        ErrorCode::InvalidArgument,
        "Concept not found",
    );
    let mut buffer = [0; 96];
    let mut out = io::Cursor::new(buffer.as_mut_slice());
    if write!(out, "Concept not found: {slug}").is_ok() {
        let n = out.position() as usize;
        let message = std::str::from_utf8(&buffer[..n]).unwrap_or(failure.message());
        http::reply_legacy_error(socket, failure, message, text);
    }
}

fn catalog_error(error: CatalogError<SlugScanError>) -> Failure {
    match error {
        CatalogError::Command(error) => crate::runtime::kernel_error(error),
        CatalogError::Visitor(SlugScanError::Refused(refusal)) => Failure::refused(refusal),
        _ => Failure::internal(),
    }
}

fn term_error(error: TermReadError) -> Failure {
    match error {
        TermReadError::Command(error) => crate::runtime::kernel_error(error),
        TermReadError::WrongRetainedFamily { .. } => Failure::internal(),
    }
}

pub(crate) fn snapshot_error(error: SnapshotBuildError) -> Failure {
    match error {
        SnapshotBuildError::View(SnapshotOpenError::Refused(refusal))
        | SnapshotBuildError::Metadata(CommitReadError::Refused(refusal))
        | SnapshotBuildError::Record(RecordReadError::Refused(refusal)) => {
            Failure::refused(refusal)
        }
        SnapshotBuildError::GraphDigest(DigestAssemblyError::Budget(error)) => {
            Failure::refused(Refusal::Budget(error))
        }
        SnapshotBuildError::GraphDigest(DigestAssemblyError::Stopped(stop)) => {
            Failure::refused(Refusal::Stopped(stop))
        }
        SnapshotBuildError::Blob(error) => crate::runtime::blob_error(error),
        SnapshotBuildError::View(
            SnapshotOpenError::NotDurable | SnapshotOpenError::ImportIncomplete,
        ) => Failure::unavailable("no complete readable publication"),
        _ => Failure::internal(),
    }
}

pub(crate) fn limit(message: &'static str) -> Failure {
    Failure::new("429 Too Many Requests", ErrorCode::Budget, message)
}

pub(crate) fn render_error(error: io::Error) -> Failure {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::refused(Refusal::Stopped(
            sage_db_types::query::ControlStop::DeadlineExceeded,
        )),
        io::ErrorKind::WriteZero | io::ErrorKind::OutOfMemory => {
            limit("activation response exceeds its allowance")
        }
        _ => Failure::internal(),
    }
}

pub(crate) fn facts(
    view: &impl SnapshotView,
    source: TermId,
    relation: RelationId,
    budget: &mut Budget,
    control: &Deadline,
) -> Result<Answer, Failure> {
    facts_for_display(view, source, relation, false, budget, control)
}

pub(crate) fn facts_for_display(
    view: &impl SnapshotView,
    source: TermId,
    relation: RelationId,
    with_proof: bool,
    budget: &mut Budget,
    control: &Deadline,
) -> Result<Answer, Failure> {
    let answer = answer_relation(
        view,
        FactQuery {
            snapshot: view.basis(),
            source,
            relation,
            with_proof,
            budget: *budget,
        },
        control,
    )
    .map_err(|err| match err {
        FactQueryError::Executor(QueryError::BudgetExceeded(_)) => {
            limit("evaluation budget exhausted")
        }
        FactQueryError::Executor(QueryError::Stopped(stop)) => {
            Failure::refused(Refusal::Stopped(stop))
        }
        _ => Failure::internal(),
    })?;
    let cost = answer.stats();
    budget.max_steps = budget
        .max_steps
        .checked_sub(cost.steps)
        .ok_or_else(Failure::internal)?;
    budget.max_work_rows = budget
        .max_work_rows
        .checked_sub(cost.work_rows)
        .ok_or_else(Failure::internal)?;
    budget.max_work_bytes = budget
        .max_work_bytes
        .checked_sub(cost.work_bytes)
        .ok_or_else(Failure::internal)?;
    budget.max_proof_nodes = budget
        .max_proof_nodes
        .checked_sub(cost.proof_nodes)
        .ok_or_else(Failure::internal)?;
    Ok(answer)
}

pub(crate) fn step(ledger: &mut dyn SharedLedger) -> Result<(), Failure> {
    ledger
        .checkpoint()
        .map_err(|s| Failure::refused(Refusal::Stopped(s)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|e| Failure::refused(Refusal::Budget(e)))
}

pub(crate) fn reserve<T>(slots: usize, ledger: &mut dyn SharedLedger) -> Result<Vec<T>, Failure> {
    let bytes = slots
        .checked_mul(size_of::<T>())
        .ok_or_else(Failure::internal)?;
    ledger
        .try_charge(bytes as u64)
        .map_err(|e| Failure::refused(Refusal::Budget(e)))?;
    let mut v = Vec::new();
    v.try_reserve_exact(slots)
        .map_err(|_| Failure::internal())?;
    if v.capacity() != slots {
        return Err(Failure::internal());
    }
    Ok(v)
}

pub(crate) fn push<T>(
    v: &mut Vec<T>,
    value: T,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    step(ledger)?;
    if v.len() == v.capacity() {
        return Err(Failure::internal());
    }
    v.push(value);
    Ok(())
}

pub(crate) fn sort<T>(
    data: &mut [T],
    mut before: impl FnMut(&T, &T) -> bool,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    try_sort(data, |a, b, _| Ok(before(a, b)), ledger)
}

pub(crate) fn try_sort<T>(
    data: &mut [T],
    mut before: impl FnMut(&T, &T, &mut dyn SharedLedger) -> Result<bool, Failure>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    for i in 1..data.len() {
        let mut j = i;
        while j > 0 {
            step(ledger)?;
            if !before(&data[j], &data[j - 1], ledger)? {
                break;
            }
            data.swap(j, j - 1);
            j -= 1;
        }
    }
    Ok(())
}

pub(crate) struct Vertex<'a> {
    pub(crate) term: &'a TermRecord,
    requires: Vec<usize>,
    direct_count: usize,
    outgoing: std::ops::Range<usize>,
}

pub(crate) struct Link<'a> {
    pub(crate) record: &'a AssertionRecordFull,
    pub(crate) from: usize,
    pub(crate) target: usize,
}

pub(crate) struct Projection<'a> {
    pub(crate) nodes: Vec<Vertex<'a>>,
    pub(crate) edges: Vec<Link<'a>>,
    kb_slug: Option<&'a str>,
}

pub(crate) fn file_order(term: &TermRecord) -> &str {
    term.legacy_source_path
        .as_deref()
        .map_or(&term.slug, |path| path.rsplit('/').next().unwrap_or(path))
}

impl<'a> Projection<'a> {
    pub(crate) fn check_kb(&self, requested: Option<&str>) -> Result<(), Failure> {
        if requested.is_some_and(|value| Some(value) != self.kb_slug) {
            return Err(Failure::new(
                "404 Not Found",
                ErrorCode::InvalidArgument,
                "Unknown knowledge base",
            ));
        }
        Ok(())
    }

    pub(crate) fn collect<F: FileSystem>(
        snapshot: &'a Snapshot<'_, F>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, Failure> {
        let graph = snapshot.graph();
        let terms = graph.terms().len();
        let assertions = graph.assertions().len();
        if terms > MAX_TERMS || assertions > MAX_ASSERTIONS {
            return Err(limit("activation graph exceeds its ceiling"));
        }
        let mut projection = Self {
            nodes: reserve(terms, ledger)?,
            edges: reserve(assertions, ledger)?,
            kb_slug: None,
        };
        let mut rows = reserve(assertions, ledger)?;
        snapshot
            .store()
            .scan_current_records(ledger, |record, work| {
                match record {
                    FoldedRecord::Term(term) if !term.retired => push(
                        &mut projection.nodes,
                        Vertex {
                            term,
                            requires: Vec::new(),
                            direct_count: 0,
                            outgoing: 0..0,
                        },
                        work,
                    )?,
                    FoldedRecord::Assertion(row) if !row.retracted => push(&mut rows, row, work)?,
                    FoldedRecord::KbMetadata(meta) => projection.kb_slug = Some(&meta.slug),
                    _ => {}
                }
                Ok(())
            })
            .map_err(|err| match err {
                RecordScanError::Visitor(failure) => failure,
                RecordScanError::Read(RecordReadError::Refused(refusal)) => {
                    Failure::refused(refusal)
                }
                RecordScanError::Incomplete(_) => {
                    Failure::unavailable("no complete readable publication")
                }
                RecordScanError::Read(_) => Failure::internal(),
            })?;
        if projection.nodes.len() != terms || rows.len() != assertions {
            return Err(Failure::internal());
        }
        sort(
            &mut projection.nodes,
            |a, b| {
                a.term
                    .layer
                    .get()
                    .cmp(&b.term.layer.get())
                    .then_with(|| {
                        file_order(a.term)
                            .encode_utf16()
                            .cmp(file_order(b.term).encode_utf16())
                    })
                    .then_with(|| a.term.key.id().cmp(&b.term.key.id()))
                    .is_lt()
            },
            ledger,
        )?;
        for record in rows {
            let from = projection.index(record.from, ledger)?;
            let target = projection.index(record.target, ledger)?;
            push(
                &mut projection.edges,
                Link {
                    record,
                    from,
                    target,
                },
                ledger,
            )?;
        }
        sort(
            &mut projection.edges,
            |a, b| {
                (a.from, a.record.declaration_order, a.record.key.id())
                    < (b.from, b.record.declaration_order, b.record.key.id())
            },
            ledger,
        )?;
        let mut consumed = 0;
        for (index, node) in projection.nodes.iter_mut().enumerate() {
            step(ledger)?;
            let start = consumed;
            while projection
                .edges
                .get(consumed)
                .is_some_and(|edge| edge.from == index)
            {
                step(ledger)?;
                consumed += 1;
            }
            node.outgoing = start..consumed;
        }
        Ok(projection)
    }

    pub(crate) fn outgoing(&self, from: usize) -> &[Link<'a>] {
        &self.edges[self.nodes[from].outgoing.clone()]
    }

    pub(crate) fn index(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<usize, Failure> {
        for (i, node) in self.nodes.iter().enumerate() {
            step(ledger)?;
            if node.term.key == key {
                return Ok(i);
            }
        }
        Err(Failure::internal())
    }

    fn evaluate<F: FileSystem>(
        &mut self,
        snapshot: &Snapshot<'_, F>,
        from: usize,
        budget: &mut Budget,
        control: &Deadline,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Failure> {
        let source = TermId::new(
            u32::try_from(self.nodes[from].term.key.id()).map_err(|_| Failure::internal())?,
        );
        let answer = facts(snapshot, source, RelationId::DependsOn, budget, control)?;

        // The engine supplies the exact membership set. This walk only
        // orders those members using the legacy display's authored-first BFS.
        let mut pending = reserve(self.nodes.len(), ledger)?;
        for _ in &self.nodes {
            push(&mut pending, false, ledger)?;
        }
        for fact in answer.rows() {
            let at = self.index(RecordKey::term(fact.to.get()), ledger)?;
            pending[at] = true;
        }
        let direct = self.outgoing(from).iter().try_fold(0, |n, edge| {
            step(ledger)?;
            Ok(n + usize::from(edge.record.relation == RelationId::DependsOn))
        })?;
        let mut ordered = reserve(answer.len() + direct, ledger)?;
        let mut queue = reserve(answer.len(), ledger)?;
        for edge in self.outgoing(from) {
            step(ledger)?;
            if edge.record.relation == RelationId::DependsOn {
                let target = edge.target;
                let id = u32::try_from(self.nodes[target].term.key.id())
                    .map_err(|_| Failure::internal())?;
                if answer.row(TermId::new(id)).is_none() {
                    return Err(Failure::internal());
                }
                push(&mut ordered, target, ledger)?;
                if pending[target] {
                    pending[target] = false;
                    push(&mut queue, target, ledger)?;
                }
            }
        }
        let direct_count = ordered.len();
        let mut cursor = 0;
        while cursor < queue.len() {
            let parent = queue[cursor];
            cursor += 1;
            for edge in self.outgoing(parent) {
                step(ledger)?;
                if edge.record.relation == RelationId::DependsOn && pending[edge.target] {
                    pending[edge.target] = false;
                    push(&mut queue, edge.target, ledger)?;
                    push(&mut ordered, edge.target, ledger)?;
                }
            }
        }
        // The formal profile can supply prerequisites through R3 without a
        // corresponding authored depends-on path. Preserve all such rows.
        for (at, remaining) in pending.into_iter().enumerate() {
            step(ledger)?;
            if remaining {
                push(&mut ordered, at, ledger)?;
            }
        }
        self.nodes[from].requires = ordered;
        self.nodes[from].direct_count = direct_count;
        Ok(())
    }

    fn visit(
        &self,
        origin: usize,
        options: &Options<'_>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Vec<usize>, Failure> {
        let mut visited = reserve(self.nodes.len(), ledger)?;
        for _ in &self.nodes {
            push(&mut visited, false, ledger)?;
        }
        visited[origin] = true;
        let mut order = reserve(self.nodes.len(), ledger)?;
        push(&mut order, origin, ledger)?;
        let mut cursor = 0;
        for _ in 0..options.depth {
            let end = order.len();
            if cursor == end {
                break;
            }
            while cursor < end {
                let current = order[cursor];
                cursor += 1;
                let mut edge = |from: usize, to: usize, ledger: &mut dyn SharedLedger| {
                    step(ledger)?;
                    let other = if from == current {
                        to
                    } else if to == current {
                        from
                    } else {
                        return Ok(());
                    };
                    if !visited[other] {
                        visited[other] = true;
                        push(&mut order, other, ledger)?;
                    }
                    Ok(())
                };
                for link in &self.edges {
                    step(ledger)?;
                    if options
                        .strategy
                        .is_none_or(|strategy| strategy.follows(link.record.relation))
                    {
                        edge(link.from, link.target, ledger)?;
                    }
                }
                if options.strategy.is_some() {
                    for (from, node) in self.nodes.iter().enumerate() {
                        step(ledger)?;
                        for &to in &node.requires[node.direct_count..] {
                            edge(from, to, ledger)?;
                        }
                    }
                }
            }
        }
        Ok(order)
    }

    fn edge(&self, row: &Link<'_>) -> Edge<'a> {
        Edge {
            kind: row.record.relation.as_str(),
            target: &self.nodes[row.target].term.slug,
            confidence: row.record.confidence.as_str(),
            from: &self.nodes[row.from].term.slug,
        }
    }

    fn summary(&self, from: usize, ledger: &mut dyn SharedLedger) -> Result<Summary<'a>, Failure> {
        let source = &self.nodes[from];
        let mut requires = reserve(source.requires.len(), ledger)?;
        for &at in &source.requires {
            push(&mut requires, self.nodes[at].term.slug.as_str(), ledger)?;
        }
        let mut load_next = reserve(self.outgoing(from).len(), ledger)?;
        for edge in self.outgoing(from) {
            step(ledger)?;
            if edge.record.relation == RelationId::Enables {
                push(
                    &mut load_next,
                    self.nodes[edge.target].term.slug.as_str(),
                    ledger,
                )?;
            }
        }
        Ok(Summary {
            node: Node::from(source.term),
            requires,
            load_next,
        })
    }

    fn response<'b>(
        &self,
        origin: usize,
        order: &[usize],
        body: &'b str,
        markdown: Option<&'b str>,
        options: &Options<'_>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Activation<'a, 'b>, Failure> {
        let mut present = reserve(self.nodes.len(), ledger)?;
        for _ in &self.nodes {
            push(&mut present, false, ledger)?;
        }
        let mut visited = reserve(order.len(), ledger)?;
        for &at in order {
            present[at] = true;
            push(&mut visited, self.nodes[at].term.slug.as_str(), ledger)?;
        }
        let mut direct_edges = reserve(self.edges.len(), ledger)?;
        for link in &self.edges {
            step(ledger)?;
            if present[link.from] && present[link.target] {
                push(&mut direct_edges, self.edge(link), ledger)?;
            }
        }
        let mut neighbors = reserve(order.len() - 1, ledger)?;
        for (at, shown) in present.into_iter().enumerate() {
            step(ledger)?;
            if shown && at != origin {
                let summary = self.summary(at, ledger)?;
                push(&mut neighbors, summary, ledger)?;
            }
        }
        let primary = self.summary(origin, ledger)?;
        let term = self.nodes[origin].term;
        Ok(Activation {
            concept: &term.slug,
            strategy: options.strategy.map(Strategy::name),
            depth: options.depth,
            molecule: body
                .trim_matches(|ch: char| ch == '\u{feff}' || ch != '\u{85}' && ch.is_whitespace()),
            triggers: &term.triggers,
            node: primary.node,
            direct_edges,
            requires: primary.requires,
            load_next: primary.load_next,
            neighbors,
            visited,
            markdown,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Activation<'a, 'b> {
    concept: &'a str,
    strategy: Option<&'static str>,
    depth: u32,
    molecule: &'b str,
    triggers: &'a [String],
    node: Node<'a>,
    direct_edges: Vec<Edge<'a>>,
    requires: Vec<&'a str>,
    load_next: Vec<&'a str>,
    neighbors: Vec<Summary<'a>>,
    visited: Vec<&'a str>,
    markdown: Option<&'b str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary<'a> {
    node: Node<'a>,
    requires: Vec<&'a str>,
    load_next: Vec<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Node<'a> {
    pub(crate) id: &'a str,
    pub(crate) atom: &'a str,
    layer: u8,
    domain: &'a [String],
    confidence: &'static str,
    file_path: FilePath<'a>,
}

impl<'a> From<&'a TermRecord> for Node<'a> {
    fn from(term: &'a TermRecord) -> Self {
        Self {
            id: &term.slug,
            atom: &term.atom,
            layer: term.layer.get(),
            domain: &term.domains,
            confidence: term.confidence.as_str(),
            file_path: FilePath(term),
        }
    }
}

pub(crate) struct FilePath<'a>(pub(crate) &'a TermRecord);

impl std::fmt::Display for FilePath<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.0.legacy_source_path {
            return out.write_str(path);
        }
        let directory =
            ["L0-axioms", "L1-core", "L2-support", "L3-instances"][usize::from(self.0.layer.get())];
        write!(out, "concepts/{directory}/{}.md", self.0.slug)
    }
}

impl Serialize for FilePath<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Serialize)]
struct Edge<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    target: &'a str,
    confidence: &'static str,
    from: &'a str,
}

fn markdown(
    out: &mut dyn Write,
    term: &TermRecord,
    body: &str,
    graph: &Projection<'_>,
    origin: usize,
) -> io::Result<()> {
    fn field(out: &mut dyn Write, name: &str, value: &impl Serialize) -> io::Result<()> {
        write!(out, "{name}: ")?;
        serde_json::to_writer(&mut *out, value)?;
        out.write_all(b"\n")
    }
    out.write_all(b"---\n")?;
    field(out, "id", &term.slug)?;
    field(out, "atom", &term.atom)?;
    field(out, "layer", &term.layer.get())?;
    field(out, "domain", &term.domains)?;
    field(out, "confidence", &term.confidence.as_str())?;
    field(out, "source", &source_label(term.source))?;
    field(out, "triggers", &term.triggers)?;
    let outgoing = graph.outgoing(origin);
    if outgoing.is_empty() {
        out.write_all(b"edges: []\n")?;
    } else {
        out.write_all(b"edges:\n")?;
        for row in outgoing {
            let edge = graph.edge(row);
            field(out, "  - type", &edge.kind)?;
            field(out, "    target", &edge.target)?;
            field(out, "    confidence", &edge.confidence)?;
        }
    }
    out.write_all(b"---\n")?;
    out.write_all(body.as_bytes())
}

pub(crate) fn source_label(source: TermSource) -> &'static str {
    match source {
        TermSource::Personal => "personal",
        TermSource::Domain => "domain",
        TermSource::Synthesis => "synthesis",
    }
}

pub(crate) fn comma_list(
    out: &mut dyn Write,
    items: impl IntoIterator<Item = impl AsRef<str>>,
) -> io::Result<()> {
    for (i, item) in items.into_iter().enumerate() {
        if i != 0 {
            out.write_all(b", ")?;
        }
        out.write_all(item.as_ref().as_bytes())?;
    }
    Ok(())
}

fn node_text(
    out: &mut dyn Write,
    node: &Node<'_>,
    edges: &[Edge<'_>],
    requires: &[&str],
) -> io::Result<()> {
    writeln!(out, "[CONCEPT: {}]", node.id)?;
    writeln!(out, "ATOM: {}", node.atom)?;
    writeln!(
        out,
        "LAYER: {} | SOURCE: {} | CONFIDENCE: {}",
        node.layer,
        node.domain.first().map_or("unknown", String::as_str),
        node.confidence
    )?;
    out.write_all(b"EDGES: ")?;
    let mut first = true;
    for edge in edges {
        // An empty write still visits the bounded output writer's checkpoint.
        let _ = out.write(b"")?;
        if edge.from == node.id {
            if !first {
                out.write_all(b", ")?;
            }
            write!(out, "{}→{}", edge.kind, edge.target)?;
            first = false;
        }
    }
    if first {
        out.write_all(b"(none)")?;
    }
    out.write_all(b"\nREQUIRES: [")?;
    comma_list(out, requires)?;
    out.write_all(b"]\n")
}

fn text(out: &mut dyn Write, value: &Activation<'_, '_>) -> io::Result<()> {
    if let Some(strategy) = value.strategy {
        writeln!(out, "[Strategy: {strategy}]")?;
    }
    node_text(out, &value.node, &value.direct_edges, &value.requires)?;
    out.write_all(b"TRIGGERS: [")?;
    comma_list(out, value.triggers)?;
    out.write_all(b"]\nLOAD-NEXT: [")?;
    comma_list(out, &value.load_next)?;
    writeln!(out, "]\n---\n{}", value.molecule)?;
    if !value.neighbors.is_empty() {
        if let Some(strategy) = value.strategy {
            writeln!(
                out,
                "\n--- {strategy} activation ({} concepts) ---\n",
                value.visited.len()
            )?;
        } else {
            writeln!(out, "\n--- {}-hop neighbors ---\n", value.depth)?;
        }
        for neighbor in &value.neighbors {
            node_text(out, &neighbor.node, &value.direct_edges, &neighbor.requires)?;
            out.write_all(b"LOAD-NEXT: [")?;
            comma_list(out, &neighbor.load_next)?;
            out.write_all(b"]\n\n")?;
        }
    }
    Ok(())
}
