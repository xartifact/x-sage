//! Legacy edge and path projections over a single admitted native snapshot.

use std::io::{self, Write};
use std::net::TcpStream;

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_query::{NavigationError, shortest_path};
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::Refusal;
use sage_db_types::identity::TermId;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::RecordKey;
use sage_db_types::relation::RelationId;
use sage_db_types::snapshot::SnapshotView;
use serde::Serialize;

use crate::activation::{
    Format, JSON, Node, Projection, TEXT, active_slug, decode_component, facts, format, limit,
    missing, parameter, parts, push, render_error, reserve, snapshot_error, sort, step, wants_text,
};
use crate::http::{self, Deadline, Failure, Head, Route};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Direction {
    Both,
    In,
    Out,
}

impl Direction {
    const fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::In => "in",
            Self::Out => "out",
        }
    }

    const fn includes(self, hit: Self) -> bool {
        matches!(
            (self, hit),
            (Self::Both, _) | (_, Self::Both) | (Self::In, Self::In) | (Self::Out, Self::Out)
        )
    }

    const fn arrow(self) -> &'static str {
        match self {
            Self::Both => "↔",
            Self::In => "←",
            Self::Out => "→",
        }
    }
}

struct EdgeOptions {
    direction: Direction,
    relation: Option<RelationId>,
    derived: bool,
}

impl EdgeOptions {
    fn parse(query: &str) -> Result<Self, Failure> {
        let direction = match parameter(query, "dir") {
            None | Some("both") => Direction::Both,
            Some("in") => Direction::In,
            Some("out") => Direction::Out,
            _ => return Err(Failure::bad("Invalid direction. Must be in, out or both")),
        };
        let relation = match parameter(query, "type") {
            None => None,
            Some(raw) => Some(
                RelationId::ALL
                    .into_iter()
                    .find(|relation| relation.as_str() == raw)
                    .ok_or_else(|| Failure::bad("Invalid edge type"))?,
            ),
        };
        Ok(Self {
            direction,
            relation,
            derived: parameter(query, "derived") == Some("true"),
        })
    }

    fn follows(&self, relation: RelationId) -> bool {
        self.relation.is_none_or(|value| value == relation)
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
    let (target, query) = parts(head);
    let use_text = format(head) == Format::Text;
    let mut from_buf = [0; 64];
    let mut to_buf = [0; 64];
    let (from, to, edge_options) = match head.route {
        Route::Edges => {
            let options = EdgeOptions::parse(query)?;
            let raw = target
                .strip_prefix("/api/concepts/")
                .and_then(|s| s.strip_suffix("/edges"))
                .ok_or_else(Failure::internal)?;
            (
                decode_component(raw, &mut from_buf, false)?,
                None,
                Some(options),
            )
        }
        Route::Path => {
            let required = || Failure::bad("Missing query parameters: from, to");
            let from = parameter(query, "from").ok_or_else(required)?;
            let to = parameter(query, "to").ok_or_else(required)?;
            (
                decode_component(from, &mut from_buf, true)?,
                Some(decode_component(to, &mut to_buf, true)?),
                None,
            )
        }
        _ => return Err(Failure::internal()),
    };
    let Some((from_key, basis)) = active_slug(store, grants, from, ledger)? else {
        missing(socket, from, wants_text(head));
        return Ok(());
    };
    let to_key = if let Some(to) = to {
        let Some((key, to_basis)) = active_slug(store, grants, to, ledger)? else {
            missing(socket, to, wants_text(head));
            return Ok(());
        };
        if to_basis != basis {
            return Err(Failure::internal());
        }
        Some(key)
    } else {
        None
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
    let projection = Projection::collect(&snapshot, ledger)?;
    projection.check_kb(parameter(query, "kb"))?;
    let from_slot = projection.index(from_key, ledger)?;
    let mime = if use_text { TEXT } else { JSON };
    if let Some(options) = edge_options {
        let hits = edge_hits(&snapshot, &projection, from_slot, &options, control, ledger)?;
        let result = EdgeReply {
            id: from,
            direction: options.direction.name(),
            edges: hits,
        };
        http::respond_with(socket, "200 OK", mime, control, ledger, |out| {
            if use_text {
                edge_text(out, &result)
            } else {
                serde_json::to_writer(out, &result).map_err(io::Error::from)
            }
        })
        .map_err(render_error)
    } else {
        let to = to.ok_or_else(Failure::internal)?;
        let to_key = to_key.ok_or_else(Failure::internal)?;
        let to_slot = projection.index(to_key, ledger)?;
        let path = path_slots(&snapshot, &projection, from_slot, to_slot, control, ledger)?;
        let mut nodes = reserve(path.len(), ledger)?;
        let mut slugs = reserve(path.len(), ledger)?;
        for index in path {
            let term = projection.nodes[index].term;
            push(&mut nodes, Node::from(term), ledger)?;
            push(&mut slugs, term.slug.as_str(), ledger)?;
        }
        let reply = PathReply {
            from,
            to,
            path: slugs,
            nodes,
        };
        http::respond_with(socket, "200 OK", mime, control, ledger, |out| {
            if use_text {
                path_text(out, &reply)
            } else {
                serde_json::to_writer(out, &reply).map_err(io::Error::from)
            }
        })
        .map_err(render_error)
    }
}

fn active_id(key: RecordKey) -> Result<TermId, Failure> {
    u32::try_from(key.id())
        .map(TermId::new)
        .map_err(|_| Failure::internal())
}

struct Hit<'a> {
    direction: Direction,
    relation: RelationId,
    confidence: Confidence,
    other: &'a sage_db_types::persist::TermRecord,
    derived: bool,
}

impl Serialize for Hit<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("EdgeHit", 5)?;
        state.serialize_field("direction", self.direction.name())?;
        state.serialize_field("type", self.relation.as_str())?;
        state.serialize_field("confidence", self.confidence.as_str())?;
        state.serialize_field("other", &Node::from(self.other))?;
        state.serialize_field("derived", &self.derived)?;
        state.end()
    }
}

#[derive(Serialize)]
struct EdgeReply<'a, 'slug> {
    id: &'slug str,
    direction: &'static str,
    edges: Vec<Hit<'a>>,
}

fn edge_hits<'a>(
    snapshot: &impl SnapshotView,
    graph: &Projection<'a>,
    origin: usize,
    options: &EdgeOptions,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<Hit<'a>>, Failure> {
    let mut hits: Vec<Hit<'a>> = reserve(graph.edges.len() + 6 * graph.nodes.len(), ledger)?;
    let profile = snapshot.rules().profile;
    // Index the displayed symmetric group so additional authored support can raise
    // its confidence without changing either underlying assertion's direction.
    let mut symmetric = reserve(graph.nodes.len(), ledger)?;
    for _ in &graph.nodes {
        push(&mut symmetric, [None::<usize>; 2], ledger)?;
    }
    for edge in &graph.edges {
        step(ledger)?;
        let relation = edge.record.relation;
        if !options.follows(relation) {
            continue;
        }
        let (direction, other) = if edge.from == origin {
            (Direction::Out, edge.target)
        } else if edge.target == origin {
            (Direction::In, edge.from)
        } else {
            continue;
        };
        let confidence = edge.record.confidence;
        let term = graph.nodes[other].term;
        if relation.spec(profile).symmetric {
            let group = usize::from(relation == RelationId::AnalogousTo);
            let slot = &mut symmetric[other][group];
            if let Some(index) = *slot {
                hits[index].confidence = hits[index].confidence.max(confidence);
            } else {
                *slot = Some(hits.len());
                push(
                    &mut hits,
                    Hit {
                        direction: Direction::Both,
                        relation,
                        confidence,
                        other: term,
                        derived: false,
                    },
                    ledger,
                )?;
            }
        } else if options.direction.includes(direction) {
            push(
                &mut hits,
                Hit {
                    direction,
                    relation,
                    confidence,
                    other: term,
                    derived: false,
                },
                ledger,
            )?;
        }
    }
    if options.derived {
        derive_hits(snapshot, graph, origin, options, control, ledger, &mut hits)?;
    }
    sort(
        &mut hits,
        |a, b| {
            (a.direction, a.relation.as_str(), &a.other.slug)
                < (b.direction, b.relation.as_str(), &b.other.slug)
        },
        ledger,
    )?;
    Ok(hits)
}

fn derive_hits<'a>(
    snapshot: &impl SnapshotView,
    graph: &Projection<'a>,
    origin: usize,
    options: &EdgeOptions,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
    hits: &mut Vec<Hit<'a>>,
) -> Result<(), Failure> {
    let profile = snapshot.rules().profile;
    let mut budget = Budget::DEFAULT;
    let selected = active_id(graph.nodes[origin].term.key)?;
    for relation in RelationId::ALL {
        step(ledger)?;
        if !relation.spec(profile).transitive || !options.follows(relation) {
            continue;
        }
        for (from, node) in graph.nodes.iter().enumerate() {
            step(ledger)?;
            let direction = if from == origin {
                Direction::Out
            } else {
                Direction::In
            };
            if !options.direction.includes(direction) {
                continue;
            }
            let answer = facts(
                snapshot,
                active_id(node.term.key)?,
                relation,
                &mut budget,
                control,
            )?;
            if from == origin {
                for row in answer.rows() {
                    step(ledger)?;
                    if row.has_derived && !row.has_asserted {
                        let index = graph.index(RecordKey::term(row.to.get()), ledger)?;
                        let confidence =
                            row.annotation.confidence().ok_or_else(Failure::internal)?;
                        push(
                            hits,
                            Hit {
                                direction,
                                relation,
                                confidence,
                                other: graph.nodes[index].term,
                                derived: true,
                            },
                            ledger,
                        )?;
                    }
                }
            } else if let Some(row) = answer.row(selected)
                && row.has_derived
                && !row.has_asserted
            {
                let confidence = row.annotation.confidence().ok_or_else(Failure::internal)?;
                push(
                    hits,
                    Hit {
                        direction,
                        relation,
                        confidence,
                        other: node.term,
                        derived: true,
                    },
                    ledger,
                )?;
            }
        }
    }
    Ok(())
}

fn edge_text(out: &mut dyn Write, result: &EdgeReply<'_, '_>) -> io::Result<()> {
    if result.edges.is_empty() {
        return writeln!(out, "No matching edges for \"{}\".", result.id);
    }
    let mut width = 0;
    for hit in &result.edges {
        let _ = out.write(b"")?;
        width = width.max(hit.other.slug.encode_utf16().count());
    }
    let mut cursor = 0;
    while cursor < result.edges.len() {
        let head = &result.edges[cursor];
        let mut end = cursor + 1;
        while let Some(hit) = result.edges.get(end) {
            let _ = out.write(b"")?;
            if (hit.direction, hit.relation) != (head.direction, head.relation) {
                break;
            }
            end += 1;
        }
        writeln!(
            out,
            "{} {} ({}):",
            head.direction.arrow(),
            head.relation.as_str(),
            end - cursor
        )?;
        for hit in &result.edges[cursor..end] {
            let padding = width - hit.other.slug.encode_utf16().count();
            writeln!(
                out,
                "  {}{:<padding$}  L{}  {:<12} {}{}",
                hit.other.slug,
                "",
                hit.other.layer.get(),
                hit.confidence.as_str(),
                hit.other.atom,
                if hit.derived { " (derived)" } else { "" }
            )?;
        }
        cursor = end;
    }
    Ok(())
}

#[derive(Serialize)]
struct PathReply<'a, 'slug> {
    from: &'slug str,
    to: &'slug str,
    path: Vec<&'a str>,
    nodes: Vec<Node<'a>>,
}

fn path_slots(
    snapshot: &impl SnapshotView,
    graph: &Projection<'_>,
    from: usize,
    to: usize,
    control: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<usize>, Failure> {
    let answer = match shortest_path(
        snapshot,
        snapshot.basis(),
        active_id(graph.nodes[from].term.key)?,
        active_id(graph.nodes[to].term.key)?,
        Budget::DEFAULT,
        control,
    ) {
        Ok(answer) => answer,
        Err(NavigationError::NoPath { .. }) => return Ok(Vec::new()),
        Err(NavigationError::BudgetExceeded(_)) => {
            return Err(limit("navigation budget exhausted"));
        }
        Err(NavigationError::Stopped(stop)) => {
            return Err(Failure::refused(Refusal::Stopped(stop)));
        }
        Err(_) => return Err(Failure::internal()),
    };
    // The query owns distance and reachability. The old CLI picks tied witnesses
    // by declaration order, so reproduce only that presentation decision and
    // verify that it has the canonical length.
    let mut predecessors = reserve(graph.nodes.len(), ledger)?;
    for _ in &graph.nodes {
        push(&mut predecessors, None, ledger)?;
    }
    predecessors[from] = Some(from);
    let mut queue = reserve(graph.nodes.len(), ledger)?;
    push(&mut queue, from, ledger)?;
    let mut cursor = 0;
    while predecessors[to].is_none() {
        step(ledger)?;
        let current = *queue.get(cursor).ok_or_else(Failure::internal)?;
        cursor += 1;
        for link in &graph.edges {
            step(ledger)?;
            let other = if link.from == current {
                link.target
            } else if link.target == current {
                link.from
            } else {
                continue;
            };
            if predecessors[other].is_none() {
                predecessors[other] = Some(current);
                push(&mut queue, other, ledger)?;
                if other == to {
                    break;
                }
            }
        }
    }
    let mut path = reserve(answer.nodes().len(), ledger)?;
    let mut current = to;
    loop {
        push(&mut path, current, ledger)?;
        if current == from {
            break;
        }
        current = predecessors[current].ok_or_else(Failure::internal)?;
    }
    if path.len() != answer.nodes().len() {
        return Err(Failure::internal());
    }
    for i in 0..path.len() / 2 {
        step(ledger)?;
        let j = path.len() - 1 - i;
        path.swap(i, j);
    }
    Ok(path)
}

fn path_text(out: &mut dyn Write, reply: &PathReply<'_, '_>) -> io::Result<()> {
    if reply.path.is_empty() {
        return writeln!(
            out,
            "No path found between \"{}\" and \"{}\".",
            reply.from, reply.to
        );
    }
    writeln!(
        out,
        "Path from {} to {} ({} nodes):",
        reply.from,
        reply.to,
        reply.path.len()
    )?;
    for (i, node) in reply.nodes.iter().enumerate() {
        writeln!(out, "  {}. {}: {}", i + 1, node.id, node.atom)?;
    }
    Ok(())
}
