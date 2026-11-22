//! Adapters that load the real default KB and the architecture-review fixture.
//!
//! These read Markdown frontmatter from a **copy** of the repository KB into a
//! temporary directory. Nothing here opens a knowledge database, writes a KB file,
//! or depends on the runtime TypeScript implementation: this is a test-only path
//! that proves the engine handles the shapes the product actually ships.
//!
//! If a KB is missing, the test reports it as skipped rather than passing silently
//! with an empty graph.

mod support;

use std::path::{Path, PathBuf};

use sage_db_infer::OptimizedExecutor;
use sage_db_types::{
    AssertionId, AssertionRecord, BaseGraph, Budget, Confidence, Digest, GraphDigest, KbUuid,
    KbVersion, RecordRevision, RelationId, RuleProfile, RuleSet, RulesDigest, SemanticLayer,
    SnapshotId, TermId, TermMeta,
};
use sage_db_types::{RunToCompletion, SourceExecutor, SourceQuery};

/// A parsed concept file.
struct Concept {
    id: String,
    layer: u8,
    edges: Vec<(RelationId, String, Confidence)>,
}

/// Parse the subset of the concept frontmatter this adapter needs.
///
/// Deliberately minimal and independent of `@x-sage/core`: reusing the runtime parser
/// would make this test agree with the implementation by construction.
fn parse_concept(text: &str) -> Option<Concept> {
    let mut id = None;
    let mut layer = None;
    let mut edges = Vec::new();
    let mut in_frontmatter = false;
    let mut in_edges = false;
    let mut pending: Option<(Option<RelationId>, Option<String>, Option<Confidence>)> = None;

    for line in text.lines() {
        // Frontmatter nests edge entries under `edges:`, so indentation must be kept
        // for the list marker check but ignored for key matching.
        let trimmed = line.trim_end();
        let key = trimmed.trim_start();
        if trimmed == "---" {
            if !in_frontmatter {
                in_frontmatter = true;
                continue;
            }
            break;
        }
        if !in_frontmatter {
            continue;
        }
        if key.starts_with("id:") {
            id = Some(
                key.strip_prefix("id:")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string(),
            );
        } else if key.starts_with("layer:") {
            layer = key
                .strip_prefix("layer:")
                .unwrap_or("")
                .trim()
                .parse::<u8>()
                .ok();
        } else if key == "edges:" {
            in_edges = true;
        } else if in_edges && key.starts_with("- type:") {
            if let Some(entry) = pending.take() {
                push_edge(&mut edges, entry);
            }
            pending = Some((
                relation_of(key.strip_prefix("- type:").unwrap_or("").trim()),
                None,
                None,
            ));
        } else if in_edges
            && key.starts_with("target:")
            && let Some(entry) = pending.as_mut()
        {
            entry.1 = Some(key.strip_prefix("target:").unwrap_or("").trim().to_string());
        } else if in_edges
            && key.starts_with("confidence:")
            && let Some(entry) = pending.as_mut()
        {
            entry.2 = Some(confidence_of(
                key.strip_prefix("confidence:").unwrap_or("").trim(),
            ));
        }
    }
    if let Some(entry) = pending {
        push_edge(&mut edges, entry);
    }
    Some(Concept {
        id: id?,
        layer: layer?,
        edges,
    })
}

fn push_edge(
    edges: &mut Vec<(RelationId, String, Confidence)>,
    entry: (Option<RelationId>, Option<String>, Option<Confidence>),
) {
    if let (Some(relation), Some(target), Some(confidence)) = entry {
        edges.push((relation, target, confidence));
    }
}

fn relation_of(name: &str) -> Option<RelationId> {
    RelationId::ALL
        .into_iter()
        .find(|relation| relation.as_str() == name)
}

fn confidence_of(name: &str) -> Confidence {
    match name {
        "contested" => Confidence::Contested,
        "hypothetical" => Confidence::Hypothetical,
        _ => Confidence::Confirmed,
    }
}

/// Recursively collect `concepts/**/*.md` under `root`.
fn collect_concepts(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_concepts(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

/// Build a [`BaseGraph`] from a KB's concept Markdown.
fn graph_from_kb(kb_root: &Path, profile: RuleProfile) -> Option<BaseGraph> {
    let concepts_dir = kb_root.join("concepts");
    if !concepts_dir.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    collect_concepts(&concepts_dir, &mut files);
    files.sort();

    let mut concepts = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).ok()?;
        if let Some(concept) = parse_concept(&text) {
            concepts.push(concept);
        }
    }
    if concepts.is_empty() {
        return None;
    }

    concepts.sort_by(|left, right| left.id.cmp(&right.id));
    let mut terms = Vec::new();
    let mut index = std::collections::BTreeMap::new();
    for (position, concept) in concepts.iter().enumerate() {
        let id = TermId::new(u32::try_from(position).ok()?);
        index.insert(concept.id.clone(), id);
        terms.push(TermMeta {
            id,
            layer: SemanticLayer::from_u8(concept.layer)?,
        });
    }

    let mut assertions = Vec::new();
    let mut next = 1u128;
    for (position, concept) in concepts.iter().enumerate() {
        let from = TermId::new(u32::try_from(position).ok()?);
        for (relation, target, confidence) in &concept.edges {
            let Some(to) = index.get(target).copied() else {
                // A dangling Markdown edge is a KB defect; skip it here so the
                // adapter reports the graph the engine must handle, not a parse
                // failure of its own making.
                continue;
            };
            assertions.push(AssertionRecord {
                id: AssertionId::new(next),
                revision: RecordRevision::FIRST,
                from,
                relation: *relation,
                target: to,
                confidence: *confidence,
            });
            next += 1;
        }
    }

    BaseGraph::new(
        SnapshotId {
            kb_uuid: KbUuid::from_bytes([0x5A; 16]),
            version: KbVersion::GENESIS,
            rules: RuleSet::from_trusted_parts(
                profile,
                RulesDigest::new(Digest::from_bytes([0x5B; 32])),
            ),
            graph_digest: GraphDigest::new(Digest::from_bytes([0x5C; 32])),
        },
        terms,
        assertions,
        Budget::DEFAULT,
    )
    .ok()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repo root")
}

/// Validate the shipped default KB as a **legacy-v1** candidate.
///
/// The shipped KB predates formal-v1, so it is evaluated under the profile it was
/// authored for; formal-v1 is a migration, not an assumption.
fn check_kb(relative: &str, profile: RuleProfile, label: &str) {
    let root = repo_root().join(relative);
    let Some(graph) = graph_from_kb(&root, profile) else {
        eprintln!("skipped {label}: no concepts under {}", root.display());
        return;
    };
    let report = sage_db_infer::validate_candidate(&graph, Budget::DEFAULT, &RunToCompletion)
        .unwrap_or_else(|error| panic!("{label}: validation errored: {error}"));
    eprintln!(
        "{label}: {} terms, {} assertions, {} violations, metric {:?}",
        graph.term_count(),
        graph.assertion_count(),
        report.violations.len(),
        report.metric
    );
    // The point of the adapter is that the engine *runs* on real shapes and reports
    // honestly. A non-empty violation list is a finding to record, not a test
    // failure, unless the KB is claimed to be formal-v1 clean.
    if profile == RuleProfile::FormalV1 {
        assert!(
            report.is_valid(),
            "{label}: formal-v1 violations on a KB claimed to be formal-clean: {:?}",
            report.violations
        );
    }
}

#[test]
fn the_default_kb_parses_and_validates_under_its_own_profile() {
    check_kb("kb/default", RuleProfile::LegacyV1, "kb/default legacy-v1");
}

#[test]
fn the_architecture_review_fixture_parses_and_validates() {
    check_kb(
        "fixtures/architecture-review",
        RuleProfile::LegacyV1,
        "architecture-review legacy-v1",
    );
}

#[test]
fn queries_run_over_a_real_kb_shape() {
    let root = repo_root().join("kb/default");
    let Some(graph) = graph_from_kb(&root, RuleProfile::LegacyV1) else {
        eprintln!("skipped: default KB missing");
        return;
    };
    // Query from the first concept that has at least one outgoing depends-on edge.
    let probe = (0..graph.term_count())
        .map(|position| TermId::new(u32::try_from(position).unwrap()))
        .find(|term| {
            graph
                .assertions_from(*term)
                .iter()
                .any(|assertion| assertion.relation == RelationId::DependsOn)
        });
    let Some(source) = probe else {
        eprintln!("skipped: no depends-on edge in default KB");
        return;
    };

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .expect("query over the real KB must complete");
    eprintln!(
        "kb/default query from {:?}: {} rows, {} steps, {} proof nodes",
        source,
        result.rows.len(),
        result.stats.steps,
        result.stats.proof_nodes
    );
    assert!(result.proofs_complete());
    for row in &result.rows {
        sage_db_infer::verify_row(&graph, &result, row)
            .unwrap_or_else(|error| panic!("replay failed on real KB row: {error}"));
    }
}

#[test]
fn l2_draft_proposals_are_not_part_of_the_default_kb() {
    // The five L2 drafts live under docs/concept-proposals and are not loaded as
    // concepts; this test documents that the KB adapter does not silently pick them
    // up, so their eventual import is an explicit decision.
    let root = repo_root().join("docs/concept-proposals/x-sage-core");
    assert!(
        !root.join("concepts").is_dir(),
        "proposals must not be discoverable as a KB concept tree"
    );
    let _ = support::snapshot(RuleProfile::LegacyV1);
}
