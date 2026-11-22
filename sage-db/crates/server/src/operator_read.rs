//! Deployment identity, bounded validation and exact Term revision reads.

use std::fmt::{self, Display, Formatter};
use std::net::TcpStream;

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::catalog::{CatalogError, SlugResolution, SlugScanError, resolve_term_slug};
use sage_db_kernel::claims::CommandError;
use sage_db_kernel::snapshot::{Expectation, Snapshot, Violation};
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::fold::{FoldedRecord, RecordIdentity};
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::history::{HistoryBudget, HistoryIncomplete, HistoryOutcome};
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::OpenRefusal;
use sage_db_store::view::{
    CommitReadError, ImportReadiness, RecordReadError, RecordScanError, ViewBasis, ViewDurability,
};
use sage_db_types::budget::Budget;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{KbMetadataRecord, RecordKey, TermRecord};
use sage_db_types::query::QueryError;
use serde::{Serialize, Serializer};

use crate::activation::{
    FilePath, Format, JSON, Projection, TEXT, decode_component, format, limit, parameter, parts,
    push, render_error, reserve, snapshot_error, step,
};
use crate::http::{self, Deadline, Failure, Head, Route};

pub(crate) fn health<F: FileSystem>(
    store: &Store<F>,
    source_identity: &str,
    stream: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let basis = match store.import_readiness(ledger) {
        Ok(ImportReadiness::Ready { basis }) => Some(basis),
        Ok(ImportReadiness::Incomplete { .. }) | Err(CommitReadError::Incomplete(_)) => None,
        Err(error) => return Err(crate::runtime::kernel_error(error.into())),
    };
    let read_ready = basis.is_some();
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Health<'a> {
        ok: bool,
        engine: &'static str,
        source_identity: &'a str,
        read_ready: bool,
        write_ready: bool,
    }
    json(
        stream,
        if read_ready {
            "200 OK"
        } else {
            "503 Service Unavailable"
        },
        &Health {
            ok: read_ready,
            engine: "sage-db",
            source_identity,
            read_ready,
            write_ready: read_ready && !store.is_fenced(),
        },
        control,
        ledger,
    )
}

pub(crate) fn serve<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    head: &Head<'_>,
    stream: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(crate::runtime::auth_error)?;
    let basis = match store
        .import_readiness(ledger)
        .map_err(|error| crate::runtime::kernel_error(error.into()))?
    {
        ImportReadiness::Ready { basis } => basis,
        ImportReadiness::Incomplete { .. } => {
            return Err(crate::runtime::kernel_error(CommandError::ImportInProgress));
        }
    };
    let (_, query) = parts(head);
    let mut kb_buffer = [0; 64];
    let kb = parameter(query, "kb")
        .map(|raw| decode_component(raw, &mut kb_buffer, true))
        .transpose()?;
    match head.route {
        Route::Catalog => {
            let actual = metadata(store, kb, basis, ledger)?;
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Entry<'a> {
                id: &'a str,
                name: &'a str,
                is_default: bool,
            }
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Catalog<'a> {
                knowledge_bases: [Entry<'a>; 1],
            }
            return json(
                stream,
                "200 OK",
                &Catalog {
                    knowledge_bases: [Entry {
                        id: &actual.slug,
                        name: &actual.name,
                        is_default: true,
                    }],
                },
                control,
                ledger,
            );
        }
        Route::NativeCheckpoint | Route::LegacyCheckpoint => {
            metadata(store, kb, basis, ledger)?;
            return crate::checkpoint_reads::serve(
                store, grants, basis, head, stream, control, ledger,
            );
        }
        Route::ConceptBody => {
            metadata(store, kb, basis, ledger)?;
            return crate::activation::serve_body(
                store, grants, basis, head, stream, control, ledger,
            );
        }
        Route::Meta if parameter(query, "identity") == Some("true") => {
            let actual = metadata(store, kb, basis, ledger)?;
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Identity<'a> {
                kb: &'a str,
                kb_uuid: Decimal<sage_db_types::identity::KbUuid>,
            }
            return json(
                stream,
                "200 OK",
                &Identity {
                    kb: &actual.slug,
                    kb_uuid: Decimal(basis.kb_uuid),
                },
                control,
                ledger,
            );
        }
        Route::Revision => {
            metadata(store, kb, basis, ledger)?;
            return revision(store, grants, basis, query, stream, control, ledger);
        }
        Route::Frontier
        | Route::NativeFrontier
        | Route::Proposals
        | Route::KnowledgeProposals
        | Route::Proposal => {
            metadata(store, kb, basis, ledger)?;
            return crate::queue_reads::serve(store, basis, head, stream, control, ledger);
        }
        Route::Meta | Route::Validate => {}
        _ => return Err(Failure::internal()),
    }
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
    if matches!(head.route, Route::Meta) {
        let actual = metadata(snapshot.store(), kb, basis, ledger)?;
        #[derive(Serialize)]
        struct Meta<'a> {
            kb: &'a str,
            authority: &'static str,
            nodes: usize,
            edges: usize,
        }
        return json(
            stream,
            "200 OK",
            &Meta {
                kb: &actual.slug,
                authority: "structured",
                nodes: graph.nodes.len(),
                edges: graph.edges.len(),
            },
            control,
            ledger,
        );
    }
    graph.check_kb(kb)?;
    validate(&snapshot, &graph, head, stream, control, ledger)
}

fn metadata<'a>(
    store: &'a Store<impl FileSystem>,
    requested: Option<&str>,
    basis: ViewBasis,
    ledger: &mut dyn SharedLedger,
) -> Result<&'a KbMetadataRecord, Failure> {
    let mut metadata = None;
    let actual = store
        .scan_current_records(ledger, |row, _| {
            if let FoldedRecord::KbMetadata(found) = row
                && metadata.replace(found).is_some()
            {
                return Err(Failure::internal());
            }
            Ok(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(error) => error,
            RecordScanError::Read(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
            RecordScanError::Read(_) => Failure::internal(),
        })?;
    if actual != basis {
        return Err(Failure::internal());
    }
    let metadata = metadata.ok_or_else(Failure::internal)?;
    if requested.is_some_and(|slug| slug != metadata.slug) {
        return Err(Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Unknown knowledge base",
        ));
    }
    Ok(metadata)
}

fn json(
    stream: &TcpStream,
    status: &'static str,
    value: &impl Serialize,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    http::respond_with(stream, status, JSON, control, ledger, |out| {
        serde_json::to_writer(&mut *out, value)?;
        out.write_all(b"\n")
    })
    .map_err(render_error)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileCheck<'a> {
    file_path: FilePath<'a>,
    valid: bool,
}

#[derive(Serialize)]
struct Validation<'a> {
    files: &'a [FileCheck<'a>],
    total: usize,
    failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph: Option<GraphCheck<'a>>,
    valid: bool,
}

#[derive(Serialize)]
struct GraphCheck<'a> {
    issues: &'a [GraphIssue<'a>],
    staleness: Staleness,
    valid: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Staleness {
    missing: [(); 0],
    extra: [(); 0],
    node_drift: bool,
}

#[derive(Serialize)]
struct GraphIssue<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    details: Detail<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Detail<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
    target: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    edge_type: Option<&'static str>,
}

fn validate(
    snapshot: &Snapshot<'_, impl FileSystem>,
    graph: &Projection<'_>,
    head: &Head<'_>,
    stream: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let (_, query) = parts(head);
    let with_graph = parameter(query, "graph") == Some("true");
    let graph_only = parameter(query, "format") == Some("graph");
    if graph_only && !with_graph {
        return Err(Failure::bad("format=graph requires graph=true"));
    }
    let mut files = reserve(if graph_only { 0 } else { graph.nodes.len() }, ledger)?;
    if !graph_only {
        for vertex in &graph.nodes {
            push(
                &mut files,
                FileCheck {
                    file_path: FilePath(vertex.term),
                    valid: true,
                },
                ledger,
            )?;
        }
    }
    let mut issues = Vec::new();
    if with_graph {
        let report =
            snapshot
                .validate_graph(Budget::DEFAULT, control)
                .map_err(|error| match error {
                    QueryError::BudgetExceeded(_) => limit("validation work exceeded its ceiling"),
                    QueryError::Stopped(stop) => Failure::refused(Refusal::Stopped(stop)),
                    _ => Failure::internal(),
                })?;
        let capacity = report
            .violations
            .len()
            .checked_add(graph.nodes.len())
            .ok_or_else(Failure::internal)?;
        issues = reserve(capacity, ledger)?;
        for violation in &report.violations {
            let issue = native_issue(graph, violation, ledger)?;
            push(&mut issues, issue, ledger)?;
        }
        // Keep the legacy integrity diagnostic for disconnected concepts. The
        // native candidate validator owns which shapes may commit.
        let mut attached = reserve(graph.nodes.len(), ledger)?;
        for _ in &graph.nodes {
            push(&mut attached, false, ledger)?;
        }
        for edge in &graph.edges {
            step(ledger)?;
            attached[edge.from] = true;
            attached[edge.target] = true;
        }
        for (i, connected) in attached.into_iter().enumerate() {
            step(ledger)?;
            if !connected {
                push(
                    &mut issues,
                    GraphIssue {
                        kind: "orphan-node",
                        message: "Concept has no incoming or outgoing edges",
                        code: None,
                        details: Detail {
                            from: None,
                            target: &graph.nodes[i].term.slug,
                            edge_type: None,
                        },
                    },
                    ledger,
                )?;
            }
        }
    }
    let report = Validation {
        files: &files,
        total: graph.nodes.len(),
        failed: 0,
        graph: with_graph.then_some(GraphCheck {
            issues: &issues,
            staleness: Staleness {
                missing: [],
                extra: [],
                node_drift: false,
            },
            valid: issues.is_empty(),
        }),
        valid: issues.is_empty(),
    };
    let status = if report.valid {
        "200 OK"
    } else {
        "422 Unprocessable Content"
    };
    if !graph_only && format(head) != Format::Text {
        return json(stream, status, &report, control, ledger);
    }
    http::respond_with(stream, status, TEXT, control, ledger, |out| {
        if !graph_only {
            writeln!(out, "Validating concepts in sage-db...\n")?;
            for file in report.files {
                writeln!(out, "✓ {}", file.file_path)?;
            }
            writeln!(out, "\n{0}/{0} concepts valid. 0 errors.", report.total)?;
        }
        if with_graph {
            if !graph_only {
                writeln!(
                    out,
                    "\n--- Graph integrity ---\n\n✓ Committed graph has no generated-file drift."
                )?;
            }
            if issues.is_empty() {
                writeln!(out, "✓ Graph integrity: no issues found.")?;
            } else {
                for issue in &issues {
                    writeln!(
                        out,
                        "✗ [{}] {} ({})",
                        issue.kind, issue.message, issue.details.target
                    )?;
                }
            }
        }
        Ok(())
    })
    .map_err(render_error)
}

fn native_issue<'a>(
    graph: &Projection<'a>,
    violation: &Violation,
    ledger: &mut dyn SharedLedger,
) -> Result<GraphIssue<'a>, Failure> {
    use Violation::*;
    let (kind, message, subject, edge_type, assertion) = match violation {
        UnknownEndpoint { assertion, .. } => (
            "dangling-edge",
            "Assertion endpoint is absent",
            None,
            None,
            Some(*assertion),
        ),
        SelfRelation { term, assertion } => (
            "self-reference",
            "Assertion points to its own term",
            Some(*term),
            None,
            Some(*assertion),
        ),
        LayerDirection {
            assertion,
            relation,
            ..
        } => (
            "layer-violation",
            "Relation violates the committed layer rule",
            None,
            Some(relation.as_str()),
            Some(*assertion),
        ),
        MultipleInstantiationTargets { subject, .. } => (
            "instantiation-cardinality",
            "Concept instantiates multiple distinct targets",
            Some(*subject),
            Some("instantiates"),
            None,
        ),
        LayerThreeWithoutTarget { term } => (
            "instantiation-coverage",
            "Layer 3 concept has no instantiation target",
            Some(*term),
            None,
            None,
        ),
        RelationCycle { relation, cycle } => (
            "edge-cycle",
            "Relation contains a cycle",
            cycle.first().copied(),
            Some(relation.as_str()),
            None,
        ),
        DefinitionUnionCycle { cycle } => (
            "edge-cycle",
            "Definition and dependency relations contain a mixed cycle",
            cycle.first().copied(),
            None,
            None,
        ),
    };
    let details = if let Some(assertion) = assertion {
        let mut found = None;
        for edge in &graph.edges {
            step(ledger)?;
            if edge.record.key.id() == assertion.get() {
                found = Some(Detail {
                    from: Some(&graph.nodes[edge.from].term.slug),
                    target: &graph.nodes[edge.target].term.slug,
                    edge_type: Some(edge.record.relation.as_str()),
                });
                break;
            }
        }
        found.ok_or_else(Failure::internal)?
    } else {
        let subject = subject.ok_or_else(Failure::internal)?;
        let at = graph.index(RecordKey::term(subject.get()), ledger)?;
        Detail {
            from: None,
            target: &graph.nodes[at].term.slug,
            edge_type,
        }
    };
    Ok(GraphIssue {
        kind,
        message,
        code: Some(violation.code()),
        details,
    })
}

pub(crate) struct Decimal<T>(pub(crate) T);

impl<T: Display> Serialize for Decimal<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

pub(crate) struct Digest(pub(crate) [u8; 32]);

impl Display for Digest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn revision(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    basis: ViewBasis,
    query: &str,
    stream: &TcpStream,
    control: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let mut slug_buffer = [0; 64];
    let raw = parameter(query, "id")
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| Failure::bad("Missing query parameter: id"))?;
    let slug = decode_component(raw, &mut slug_buffer, true)?;
    let requested = parameter(query, "revision")
        .map(|raw| {
            if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Failure::bad("revision must be a positive decimal integer"));
            }
            raw.parse::<u64>()
                .ok()
                .filter(|&number| number > 0)
                .ok_or_else(|| Failure::bad("revision must be a positive decimal integer"))
        })
        .transpose()?;
    let resolution =
        resolve_term_slug(store, grants, slug, ledger).map_err(|error| match error {
            CatalogError::Command(error) => crate::runtime::kernel_error(error),
            CatalogError::Visitor(SlugScanError::Refused(refusal)) => Failure::refused(refusal),
            _ => Failure::internal(),
        })?;
    let SlugResolution::Found {
        term,
        basis: served,
    } = resolution
    else {
        return Err(Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Concept not found",
        ));
    };
    if served != basis {
        return Err(Failure::internal());
    }
    let Some(wanted) = requested else {
        return revision_reply(term, basis, None, stream, control, ledger);
    };
    let key = term.key;
    match store.history(
        &RecordIdentity::Identified { key },
        wanted,
        HistoryBudget::new(10_000),
        &Limits::V1,
        ledger,
    ) {
        HistoryOutcome::Found(hit) => {
            if hit.basis != basis || hit.durability != ViewDurability::Durable {
                return Err(Failure::unavailable("no complete readable publication"));
            }
            let FoldedRecord::Term(term) = hit.image.record() else {
                return Err(Failure::internal());
            };
            if term.key != key || term.revision.get() != wanted {
                return Err(Failure::internal());
            }
            revision_reply(
                term,
                basis,
                Some(hit.source_version),
                stream,
                control,
                ledger,
            )
        }
        HistoryOutcome::Absent {
            basis: served,
            durability: ViewDurability::Durable,
            ..
        } if served == basis => Err(Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Revision not found",
        )),
        HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::Refused(refusal),
            ..
        }
        | HistoryOutcome::Error(OpenRefusal::Refused(refusal)) => Err(Failure::refused(refusal)),
        HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::EventLimit { .. },
            ..
        } => Err(limit("revision lookup exceeded its event ceiling")),
        HistoryOutcome::Error(OpenRefusal::Corrupt(_)) => Err(Failure::new(
            "503 Service Unavailable",
            ErrorCode::Corrupt,
            "Committed history is corrupt",
        )),
        _ => Err(Failure::unavailable("no complete readable revision")),
    }
}

fn revision_reply(
    term: &TermRecord,
    basis: ViewBasis,
    source_version: Option<u64>,
    stream: &TcpStream,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Revision<'a> {
        id: &'a str,
        term_id: Decimal<u128>,
        revision: Decimal<u64>,
        version: Decimal<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_version: Option<Decimal<u64>>,
        manifest_sha256: Digest,
        atom: &'a str,
        body_sha256: Digest,
        retired: bool,
    }
    json(
        stream,
        "200 OK",
        &Revision {
            id: &term.slug,
            term_id: Decimal(term.key.id()),
            revision: Decimal(term.revision.get()),
            version: Decimal(basis.version.get()),
            source_version: source_version.map(Decimal),
            manifest_sha256: Digest(basis.manifest_sha256),
            atom: &term.atom,
            body_sha256: Digest(term.body_ref),
            retired: term.retired,
        },
        control,
        ledger,
    )
}
