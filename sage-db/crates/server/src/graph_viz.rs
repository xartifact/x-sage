//! Legacy graph and visualiser projections over a complete native publication.

use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_protocol::generated::ErrorCode;
use sage_db_query::Answer;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::{
    CommittedBlob, ImportReadiness, RecordReadError, RecordScanError, ViewBasis,
};
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::identity::TermId;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::RecordKey;
use sage_db_types::proof::ProofNode;
use sage_db_types::query::FactRow;
use sage_db_types::relation::{RelationId, RuleProfile};
use sage_db_types::snapshot::SnapshotView;
use serde::{Serialize, Serializer};

use crate::activation::{
    Format, JSON, Link, MAX_TERMS, Node, Projection, TEXT, decode_component, facts_for_display,
    format, limit, parameter, parts, push, render_error, reserve, snapshot_error, source_label,
    step,
};
use crate::http::{self, Deadline, Failure, Head, Route};

const MAX_VIZ_BYTES: usize = 512 * 1024;
const MAX_SEMANTIC_ROWS: usize = Budget::DEFAULT.max_rows as usize;

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
    let ImportReadiness::Ready { basis } = store
        .import_readiness(ledger)
        .map_err(|e| crate::runtime::kernel_error(e.into()))?
    else {
        return Err(Failure::unavailable("no complete readable publication"));
    };
    let (_, query) = parts(head);
    let mut kb_buffer = [0; 64];
    let kb = parameter(query, "kb")
        .map(|value| decode_component(value, &mut kb_buffer, true))
        .transpose()?;
    let viz = matches!(head.route, Route::Viz);
    let stats = viz && format(head) == Format::Text;
    let rebuild = matches!(head.route, Route::GraphRebuild);
    let bodies = if viz && !stats || rebuild {
        let refs = body_plan(store, basis, kb, !rebuild, ledger)?;
        if rebuild {
            return Err(Failure::new(
                "409 Conflict",
                ErrorCode::Conflict,
                "graph.json is derived from the structured store; there is no file to rebuild. Use GET /api/graph.",
            ));
        }
        read_bodies(store, basis, refs, ledger)?
    } else {
        Vec::new()
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
    graph.check_kb(kb)?;
    if viz {
        return visualise(&snapshot, &graph, &bodies, stats, socket, control, ledger);
    }
    let mut nodes = reserve(graph.nodes.len(), ledger)?;
    for vertex in &graph.nodes {
        push(&mut nodes, Node::from(vertex.term), ledger)?;
    }
    let semantic = parameter(query, "semantic") == Some("true");
    let edges = if semantic {
        semantic_edges(&snapshot, &graph, control, ledger)?
    } else {
        authored_edges(&graph, graph.edges.len(), ledger)?
    };
    reply(socket, JSON, control, ledger, |out| {
        json(out, &GraphReply { nodes, edges })
    })
}

fn reply(
    socket: &TcpStream,
    mime: &'static str,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<(), Failure> {
    http::respond_with(socket, "200 OK", mime, deadline, ledger, write).map_err(render_error)
}

fn json(out: &mut dyn Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum Witness<'a> {
    Direct([&'a str; 0]),
    Via([&'a str; 1]),
}

#[derive(Clone, Copy, Serialize)]
struct GraphEdge<'a> {
    #[serde(rename = "type")]
    relation: &'static str,
    target: &'a str,
    confidence: &'static str,
    from: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    derived: Option<Witness<'a>>,
}

impl<'a> GraphEdge<'a> {
    fn authored(graph: &Projection<'a>, edge: &Link<'_>) -> Self {
        Self {
            from: &graph.nodes[edge.from].term.slug,
            target: &graph.nodes[edge.target].term.slug,
            relation: edge.record.relation.as_str(),
            confidence: edge.record.confidence.as_str(),
            derived: None,
        }
    }

    fn mirrored(self) -> Self {
        Self {
            from: self.target,
            target: self.from,
            ..self
        }
    }
}

#[derive(Serialize)]
struct GraphReply<'a> {
    nodes: Vec<Node<'a>>,
    edges: Vec<GraphEdge<'a>>,
}

fn authored_edges<'a>(
    graph: &Projection<'a>,
    capacity: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<GraphEdge<'a>>, Failure> {
    if graph.edges.len() > capacity {
        return Err(limit("semantic graph exceeds its row ceiling"));
    }
    let mut edges = reserve(capacity, ledger)?;
    for edge in &graph.edges {
        push(&mut edges, GraphEdge::authored(graph, edge), ledger)?;
    }
    Ok(edges)
}

fn add_edge<'a>(
    output: &mut Vec<GraphEdge<'a>>,
    edge: GraphEdge<'a>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    if output.len() == MAX_SEMANTIC_ROWS {
        return Err(limit("semantic graph exceeds its row ceiling"));
    }
    push(output, edge, ledger)
}

fn term_id(key: RecordKey) -> Result<TermId, Failure> {
    u32::try_from(key.id())
        .map(TermId::new)
        .map_err(|_| Failure::internal())
}

/// Display traversal. Native fact rows remain the authority for reachability and annotation.
#[derive(Clone, Copy)]
struct Hop {
    first: usize,
    confidence: Confidence,
}

fn authored_walk(
    graph: &Projection<'_>,
    origin: usize,
    relation: RelationId,
    visited: &mut [Option<Hop>],
    queue: &mut Vec<usize>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    // The queue contains only reached targets, so the known origin needs no slot in it.
    for &target in &*queue {
        step(ledger)?;
        visited[target] = None;
    }
    queue.clear();
    let mut from = origin;
    let mut prefix: Option<Hop> = None;
    let mut cursor = 0;
    loop {
        for edge in graph.outgoing(from) {
            step(ledger)?;
            if edge.record.relation != relation
                || edge.target == origin
                || visited[edge.target].is_some()
            {
                continue;
            }
            let confidence = edge.record.confidence;
            let hop = prefix.map_or(
                Hop {
                    first: edge.target,
                    confidence,
                },
                |prev| Hop {
                    first: prev.first,
                    confidence: prev.confidence.min(confidence),
                },
            );
            push(queue, edge.target, ledger)?;
            visited[edge.target] = Some(hop);
        }
        let Some(&next) = queue.get(cursor) else {
            break;
        };
        from = next;
        prefix = visited[next];
        cursor += 1;
    }
    Ok(())
}

fn semantic_edges<'a>(
    snapshot: &impl SnapshotView,
    graph: &Projection<'a>,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<GraphEdge<'a>>, Failure> {
    let profile = snapshot.rules().profile;
    let mut edges = authored_edges(graph, MAX_SEMANTIC_ROWS, ledger)?;
    append_mirrors(&mut edges, graph, profile, ledger)?;

    let mut budget = Budget::DEFAULT;
    let mut visited = reserve(graph.nodes.len(), ledger)?;
    let mut queue = reserve(graph.nodes.len(), ledger)?;
    let mut indexed = reserve(graph.nodes.len(), ledger)?;
    let mut native_targets = reserve(graph.nodes.len(), ledger)?;
    for _ in &graph.nodes {
        push(&mut visited, None, ledger)?;
        push(&mut indexed, None, ledger)?;
    }
    for relation in RelationId::ALL
        .into_iter()
        .filter(|r| r.spec(profile).transitive)
    {
        for (origin, vertex) in graph.nodes.iter().enumerate() {
            step(ledger)?;
            let source = term_id(vertex.term.key)?;
            let answer = facts_for_display(snapshot, source, relation, true, &mut budget, control)?;
            authored_walk(graph, origin, relation, &mut visited, &mut queue, ledger)?;
            native_targets.clear();
            for row in answer.rows() {
                let to = graph.index(RecordKey::term(row.to.get()), ledger)?;
                push(&mut native_targets, to, ledger)?;
                indexed[to] = Some(*row);
            }
            for &target in &queue {
                step(ledger)?;
                let row = indexed[target].take().ok_or_else(Failure::internal)?;
                if !row.has_asserted {
                    let confidence = row.annotation.confidence().ok_or_else(Failure::internal)?;
                    let display = visited[target].filter(|hop| hop.confidence == confidence);
                    let witness = match display {
                        Some(hop) => Witness::Via([&graph.nodes[hop.first].term.slug]),
                        None => proof_witness(snapshot, graph, &answer, row, ledger)?,
                    };
                    add_edge(
                        &mut edges,
                        derived_edge(graph, origin, target, relation, confidence, witness),
                        ledger,
                    )?;
                }
            }
            // Formal rules can introduce dependencies with no same-relation
            // authored walk. Display these in the native answer's canonical order.
            for &target in &native_targets {
                step(ledger)?;
                if let Some(row) = indexed[target].take() {
                    if row.has_asserted {
                        return Err(Failure::internal());
                    }
                    let confidence = row.annotation.confidence().ok_or_else(Failure::internal)?;
                    let witness = proof_witness(snapshot, graph, &answer, row, ledger)?;
                    add_edge(
                        &mut edges,
                        derived_edge(graph, origin, target, relation, confidence, witness),
                        ledger,
                    )?;
                }
            }
        }
    }
    Ok(edges)
}

fn append_mirrors<'a>(
    edges: &mut Vec<GraphEdge<'a>>,
    graph: &Projection<'a>,
    profile: RuleProfile,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    // Keep authored rows intact. For an absent reverse direction, add one row
    // using the strongest active direct support of the symmetric relation.
    struct Mirror {
        direction: (usize, RelationId, usize),
        index: usize,
        confidence: Confidence,
    }
    let mut mirrored: Vec<Mirror> = reserve(graph.edges.len(), ledger)?;
    for edge in &graph.edges {
        step(ledger)?;
        let relation = edge.record.relation;
        if !relation.spec(profile).symmetric {
            continue;
        }
        let mut authored_reverse = false;
        for other in graph.outgoing(edge.target) {
            step(ledger)?;
            if other.target == edge.from && other.record.relation == relation {
                authored_reverse = true;
                break;
            }
        }
        if authored_reverse {
            continue;
        }
        let mut existing = None;
        let direction = (edge.target, relation, edge.from);
        for mirror in &mut mirrored {
            step(ledger)?;
            if mirror.direction == direction {
                mirror.confidence = mirror.confidence.max(edge.record.confidence);
                existing = Some((mirror.index, mirror.confidence));
                break;
            }
        }
        if let Some((index, confidence)) = existing {
            edges[index].confidence = confidence.as_str();
        } else {
            let index = edges.len();
            add_edge(edges, GraphEdge::authored(graph, edge).mirrored(), ledger)?;
            push(
                &mut mirrored,
                Mirror {
                    direction,
                    index,
                    confidence: edge.record.confidence,
                },
                ledger,
            )?;
        }
    }

    Ok(())
}

fn derived_edge<'a>(
    graph: &Projection<'a>,
    origin: usize,
    target: usize,
    relation: RelationId,
    confidence: Confidence,
    witness: Witness<'a>,
) -> GraphEdge<'a> {
    GraphEdge {
        from: &graph.nodes[origin].term.slug,
        target: &graph.nodes[target].term.slug,
        relation: relation.as_str(),
        confidence: confidence.as_str(),
        derived: Some(witness),
    }
}

fn proof_witness<'a>(
    snapshot: &impl SnapshotView,
    graph: &Projection<'a>,
    answer: &Answer,
    row: FactRow,
    ledger: &mut dyn SharedLedger,
) -> Result<Witness<'a>, Failure> {
    let proof = answer.proof().ok_or_else(Failure::internal)?;
    let mut root = row.proof.ok_or_else(Failure::internal)?;
    loop {
        step(ledger)?;
        match proof.get(root).ok_or_else(Failure::internal)? {
            ProofNode::Asserted { assertion_id, .. } => {
                let premise = snapshot
                    .graph()
                    .assertion(assertion_id)
                    .ok_or_else(Failure::internal)?;
                if premise.from != row.from {
                    return Err(Failure::internal());
                }
                if premise.target == row.to {
                    return Ok(Witness::Direct([]));
                }
                let hop = graph.index(RecordKey::term(premise.target.get()), ledger)?;
                return Ok(Witness::Via([&graph.nodes[hop].term.slug]));
            }
            ProofNode::Transitive { left, .. } => root = left,
            ProofNode::DefinitionImpliesDependency { premise } => root = premise,
            ProofNode::Symmetry { .. } => return Err(Failure::internal()),
        }
    }
}

struct BodyRef {
    key: RecordKey,
    digest: [u8; 32],
}

fn body_plan(
    store: &Store<impl FileSystem>,
    basis: ViewBasis,
    kb: Option<&str>,
    include_bodies: bool,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<BodyRef>, Failure> {
    let mut refs = reserve(if include_bodies { MAX_TERMS } else { 0 }, ledger)?;
    let mut actual = None;
    let observed = store
        .scan_current_records(ledger, |row, work| {
            match row {
                FoldedRecord::KbMetadata(meta) => actual = Some(meta.slug.as_str()),
                FoldedRecord::Term(term) if include_bodies && !term.retired => {
                    if refs.len() == MAX_TERMS {
                        return Err(limit("visualisation exceeds its node ceiling"));
                    }
                    push(
                        &mut refs,
                        BodyRef {
                            key: term.key,
                            digest: term.body_ref,
                        },
                        work,
                    )?;
                }
                _ => {}
            }
            Ok(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(error) => error,
            RecordScanError::Read(RecordReadError::Refused(refused)) => Failure::refused(refused),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
            RecordScanError::Read(_) => Failure::internal(),
        })?;
    if observed != basis {
        return Err(Failure::internal());
    }
    if kb.is_some_and(|value| Some(value) != actual) {
        return Err(Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Unknown knowledge base",
        ));
    }
    Ok(refs)
}

fn read_bodies(
    store: &mut Store<impl FileSystem>,
    basis: ViewBasis,
    refs: Vec<BodyRef>,
    ledger: &mut AttemptLedger<'_>,
) -> Result<Vec<(RecordKey, CommittedBlob)>, Failure> {
    let mut results = reserve(refs.len(), ledger)?;
    let mut remaining = MAX_VIZ_BYTES as u64;
    for row in refs {
        step(ledger)?;
        let body = store
            .read_committed_blob(row.digest, remaining, ledger)
            .map_err(crate::runtime::blob_error)?;
        if body.basis() != basis {
            return Err(Failure::internal());
        }
        remaining = remaining
            .checked_sub(u64::try_from(body.len()).map_err(|_| Failure::internal())?)
            .ok_or_else(Failure::internal)?;
        push(&mut results, (row.key, body), ledger)?;
    }
    Ok(results)
}

fn trim_markdown(body: &str) -> &str {
    // ECMAScript String.trim excludes NEL and includes the BOM.
    body.trim_matches(|ch| {
        matches!(ch,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

struct GeneratedAt(DateTime<Utc>);

impl Display for GeneratedAt {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.format("%Y-%m-%dT%H:%M:%S%.3fZ").fmt(f)
    }
}

impl Serialize for GeneratedAt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VizMeta<'a> {
    concept_count: usize,
    asserted_edge_count: usize,
    edge_types: &'a [&'static str],
    generated_at: GeneratedAt,
    confidence_counts: ConfidenceCounts,
    layer_counts: LayerCounts,
}

#[derive(Default, Serialize)]
struct ConfidenceCounts {
    confirmed: usize,
    hypothetical: usize,
    contested: usize,
}

#[derive(Serialize)]
struct LayerCounts {
    #[serde(rename = "L0")]
    l0: usize,
    #[serde(rename = "L1")]
    l1: usize,
    #[serde(rename = "L2")]
    l2: usize,
    #[serde(rename = "L3")]
    l3: usize,
}

#[derive(Serialize)]
struct VizNode<'a> {
    #[serde(flatten)]
    node: Node<'a>,
    source: &'static str,
    triggers: &'a [String],
    molecule: &'a str,
}

#[derive(Serialize)]
struct VizEdge<'a> {
    #[serde(flatten)]
    edge: GraphEdge<'a>,
    mirrored: bool,
}

#[derive(Serialize)]
struct VizReply<'a> {
    nodes: Vec<VizNode<'a>>,
    edges: Vec<VizEdge<'a>>,
    meta: VizMeta<'a>,
}

fn visualise(
    snapshot: &impl SnapshotView,
    graph: &Projection<'_>,
    bodies: &[(RecordKey, CommittedBlob)],
    stats: bool,
    socket: &TcpStream,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    let mut layers = [0; 4];
    let mut confidence_counts = ConfidenceCounts::default();
    for node in &graph.nodes {
        step(ledger)?;
        layers[usize::from(node.term.layer.get())] += 1;
        match node.term.confidence {
            Confidence::Confirmed => confidence_counts.confirmed += 1,
            Confidence::Hypothetical => confidence_counts.hypothetical += 1,
            Confidence::Contested => confidence_counts.contested += 1,
        }
    }
    let mut types = [false; 7];
    let profile: RuleProfile = snapshot.rules().profile;
    let mut mirrors = 0;
    for edge in &graph.edges {
        step(ledger)?;
        let relation = edge.record.relation;
        types[usize::from(relation.get())] = true;
        mirrors += usize::from(relation.spec(profile).symmetric);
    }
    let mut names = [""; 7];
    let mut count = 0;
    for relation in RelationId::ALL {
        if types[usize::from(relation.get())] {
            names[count] = relation.as_str();
            count += 1;
        }
    }
    names[..count].sort_unstable();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Failure::internal())?;
    let millis = i64::try_from(now.as_millis()).map_err(|_| Failure::internal())?;
    let generated_at =
        DateTime::<Utc>::from_timestamp_millis(millis).ok_or_else(Failure::internal)?;
    let meta = VizMeta {
        concept_count: graph.nodes.len(),
        asserted_edge_count: graph.edges.len(),
        edge_types: &names[..count],
        generated_at: GeneratedAt(generated_at),
        confidence_counts,
        layer_counts: LayerCounts {
            l0: layers[0],
            l1: layers[1],
            l2: layers[2],
            l3: layers[3],
        },
    };
    if stats {
        return reply(socket, TEXT, deadline, ledger, |out| {
            writeln!(out, "concepts: {}", meta.concept_count)?;
            writeln!(
                out,
                "edges:    {} ({} asserted, {mirrors} mirrored)",
                meta.asserted_edge_count + mirrors,
                meta.asserted_edge_count
            )?;
            writeln!(
                out,
                "layers:   L0={} L1={} L2={} L3={}",
                layers[0], layers[1], layers[2], layers[3]
            )?;
            out.write_all(b"types:    ")?;
            crate::activation::comma_list(out, meta.edge_types)?;
            write!(out, "\ngenerated: {}\n", meta.generated_at)
        });
    }

    if bodies.len() != graph.nodes.len() {
        return Err(Failure::internal());
    }
    let mut nodes = reserve(graph.nodes.len(), ledger)?;
    for vertex in &graph.nodes {
        let term = vertex.term;
        let mut body = None;
        for (key, content) in bodies {
            step(ledger)?;
            if *key == term.key {
                body = Some(content);
                break;
            }
        }
        let raw = body.ok_or_else(Failure::internal)?.as_bytes();
        let raw = std::str::from_utf8(raw).map_err(|_| Failure::internal())?;
        push(
            &mut nodes,
            VizNode {
                node: Node::from(term),
                source: source_label(term.source),
                triggers: &term.triggers,
                molecule: trim_markdown(raw),
            },
            ledger,
        )?;
    }
    let mut edges = reserve(graph.edges.len() + mirrors, ledger)?;
    for link in &graph.edges {
        let edge = GraphEdge::authored(graph, link);
        push(
            &mut edges,
            VizEdge {
                edge,
                mirrored: false,
            },
            ledger,
        )?;
        if link.record.relation.spec(profile).symmetric {
            push(
                &mut edges,
                VizEdge {
                    edge: edge.mirrored(),
                    mirrored: true,
                },
                ledger,
            )?;
        }
    }
    let mut buffer = reserve(MAX_VIZ_BYTES, ledger)?;
    buffer.resize(MAX_VIZ_BYTES, 0);
    http::respond_prepaid(socket, JSON, &mut buffer, deadline, ledger, |out| {
        json(out, &VizReply { nodes, edges, meta })
    })
    .map_err(render_error)
}

#[cfg(test)]
mod tests {
    use super::trim_markdown;

    #[test]
    fn molecule_boundary_uses_legacy_unicode_whitespace() {
        assert_eq!(trim_markdown("\u{feff}\u{00a0}\t中文\n\u{2029}"), "中文");
        assert_eq!(
            trim_markdown(" \u{0085}quoted\u{0085} "),
            "\u{0085}quoted\u{0085}"
        );
    }
}
