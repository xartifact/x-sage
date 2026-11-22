//! Read stored term metadata and authored assertions on one database basis.

use std::net::TcpStream;

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::ImportReadiness;
use sage_db_types::identity::KbUuid;
use serde::Serialize;

use crate::activation::{Projection, push, render_error, reserve, snapshot_error};
use crate::http::{self, Deadline, Failure};
use crate::operator_read::{Decimal, Digest};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Basis {
    kb_uuid: Decimal<KbUuid>,
    version: Decimal<u64>,
    manifest_sha256: Digest,
}

#[derive(Serialize)]
struct Key {
    kind: u16,
    id: Decimal<u128>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Term<'a> {
    key: Key,
    revision: Decimal<u64>,
    slug: &'a str,
    atom: &'a str,
    layer: u8,
    domains: &'a [String],
    confidence: &'static str,
    source: &'static str,
    triggers: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_source_path: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Assertion<'a> {
    from_slug: &'a str,
    target_slug: &'a str,
    relation: &'static str,
    confidence: &'static str,
    declaration_order: u32,
}

#[derive(Serialize)]
struct Reply<'a> {
    basis: Basis,
    terms: Vec<Term<'a>>,
    assertions: Vec<Assertion<'a>>,
}

pub(crate) fn serve<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    socket: &TcpStream,
    control: &Deadline,
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
    let mut terms = reserve(graph.nodes.len(), ledger)?;
    for vertex in &graph.nodes {
        let term = vertex.term;
        push(
            &mut terms,
            Term {
                key: Key {
                    kind: term.key.kind().as_u16(),
                    id: Decimal(term.key.id()),
                },
                revision: Decimal(term.revision.get()),
                slug: &term.slug,
                atom: &term.atom,
                layer: term.layer.get(),
                domains: &term.domains,
                confidence: term.confidence.as_str(),
                source: crate::activation::source_label(term.source),
                triggers: &term.triggers,
                legacy_source_path: term.legacy_source_path.as_deref(),
            },
            ledger,
        )?;
    }
    let mut assertions = reserve(graph.edges.len(), ledger)?;
    for link in &graph.edges {
        push(
            &mut assertions,
            Assertion {
                from_slug: &graph.nodes[link.from].term.slug,
                target_slug: &graph.nodes[link.target].term.slug,
                relation: link.record.relation.as_str(),
                confidence: link.record.confidence.as_str(),
                declaration_order: link.record.declaration_order,
            },
            ledger,
        )?;
    }
    http::respond(
        socket,
        &Reply {
            basis: Basis {
                kb_uuid: Decimal(basis.kb_uuid),
                version: Decimal(basis.version.get()),
                manifest_sha256: Digest(basis.manifest_sha256),
            },
            terms,
            assertions,
        },
        control,
        ledger,
    )
    .map_err(render_error)
}
