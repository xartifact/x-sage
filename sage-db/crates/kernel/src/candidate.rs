//! Assembly and validation of authored graph candidates under the committed rules.

use sage_db_infer::{validate::Violation, validate_candidate};
use sage_db_store::{
    fold::FoldedRecord,
    fs::FileSystem,
    handle::Store,
    typed,
    view::{CommitBasis, RecordScanError},
};
use sage_db_types::{
    budget::Budget,
    digest::{Digest, GraphDigest, RulesDigest},
    framing::Refusal,
    graph::{BaseGraph, GraphBuildRefusal, SnapshotId},
    identity::{AssertionId, KbVersion, TermId},
    ledger::SharedLedger,
    persist::RecordKey,
    query::{QueryControl, QueryError},
    record::{AssertionRecord, TermMeta},
    relation::RuleSet,
};

use crate::claims::{CommandError, InternalDefect, admitted_vec, version_next};

pub(crate) enum Addition<'a> {
    Term(TermMeta),
    Assertion(AssertionRecord),
    ReplaceAssertion {
        retract: AssertionId,
        add: AssertionRecord,
    },
    TermWithAssertions(TermMeta, &'a [AssertionRecord]),
}

#[derive(Debug)]
pub(crate) enum CandidateError {
    Command(CommandError),
    Validation(QueryError),
    Invalid(Vec<Violation>),
}

pub(crate) fn term_id(key: RecordKey) -> Result<TermId, CommandError> {
    u32::try_from(key.id())
        .ok()
        .and_then(|id| {
            (id != 0 && key.kind() == sage_db_types::persist::RecordKind::Term)
                .then(|| TermId::new(id))
        })
        .ok_or(CommandError::Internal(
            InternalDefect::EncodeDecodeDivergence,
        ))
}

fn scan_error(error: RecordScanError<CommandError>) -> CommandError {
    match error {
        RecordScanError::Visitor(error) => error,
        RecordScanError::Incomplete(reason) => CommandError::Incomplete(reason),
        RecordScanError::Read(error) => error.into(),
    }
}

fn digest_error(error: typed::DigestAssemblyError) -> CommandError {
    let refusal = match error {
        typed::DigestAssemblyError::Budget(error) => Refusal::Budget(error),
        typed::DigestAssemblyError::Stopped(stop) => Refusal::Stopped(stop),
        typed::DigestAssemblyError::Allocation { requested } => Refusal::Allocation { requested },
        typed::DigestAssemblyError::Structure(_) => {
            return CommandError::Internal(InternalDefect::EncodeDecodeDivergence);
        }
    };
    CommandError::Refused(refusal)
}

fn sort_by_key<T, K: Ord>(
    rows: &mut [T],
    key: impl Fn(&T) -> K,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CommandError> {
    ledger.checkpoint()?;
    for index in 1..rows.len() {
        let mut position = index;
        while position > 0 {
            ledger.try_charge_entries(1).map_err(Refusal::Budget)?;
            ledger.checkpoint()?;
            if key(&rows[position - 1]) <= key(&rows[position]) {
                break;
            }
            rows.swap(position - 1, position);
            position -= 1;
        }
    }
    ledger.checkpoint()?;
    Ok(())
}

fn build(
    store: &Store<impl FileSystem>,
    basis: &CommitBasis,
    addition: Addition<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<BaseGraph, CommandError> {
    let (term, added_assertions, retracted) = match &addition {
        Addition::Term(term) => (Some(*term), &[][..], None),
        Addition::Assertion(assertion) => (None, std::slice::from_ref(assertion), None),
        Addition::ReplaceAssertion { retract, add } => {
            (None, std::slice::from_ref(add), Some(*retract))
        }
        Addition::TermWithAssertions(term, assertions) => (Some(*term), *assertions, None),
    };
    let included = |row: &sage_db_types::persist::AssertionRecordFull| {
        !row.retracted && Some(AssertionId::new(row.key.id())) != retracted
    };
    let mut term_count = 0usize;
    let mut assertion_count = 0usize;
    let mut found_retraction = false;
    let scanned = store
        .scan_identified_records(ledger, |row, work| {
            work.checkpoint()?;
            let count = match row {
                FoldedRecord::Term(term) if !term.retired => &mut term_count,
                FoldedRecord::Assertion(assertion) if included(assertion) => &mut assertion_count,
                FoldedRecord::Assertion(assertion) if !assertion.retracted => {
                    found_retraction = true;
                    return Ok(());
                }
                _ => return Ok(()),
            };
            *count = count
                .checked_add(1)
                .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?;
            Ok(())
        })
        .map_err(scan_error)?;
    if scanned != basis.basis || found_retraction != retracted.is_some() {
        return Err(CommandError::BasisDivergence);
    }
    let mut terms = admitted_vec(
        term_count
            .checked_add(usize::from(term.is_some()))
            .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?,
        ledger,
    )?;
    let mut assertions = admitted_vec(
        assertion_count
            .checked_add(added_assertions.len())
            .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?,
        ledger,
    )?;
    let filled = store
        .scan_identified_records(ledger, |row, work| {
            work.checkpoint()?;
            match row {
                FoldedRecord::Term(term) if !term.retired && terms.len() < term_count => {
                    terms.push(TermMeta {
                        id: term_id(term.key)?,
                        layer: term.layer,
                    });
                }
                FoldedRecord::Assertion(row)
                    if included(row) && assertions.len() < assertion_count =>
                {
                    assertions.push(AssertionRecord {
                        id: AssertionId::new(row.key.id()),
                        revision: row.revision,
                        from: term_id(row.from)?,
                        target: term_id(row.target)?,
                        relation: row.relation,
                        confidence: row.confidence,
                    });
                }
                FoldedRecord::Term(term) if !term.retired => {
                    return Err(CommandError::BasisDivergence);
                }
                FoldedRecord::Assertion(row) if included(row) => {
                    return Err(CommandError::BasisDivergence);
                }
                _ => {}
            }
            Ok(())
        })
        .map_err(scan_error)?;
    if filled != scanned || terms.len() != term_count || assertions.len() != assertion_count {
        return Err(CommandError::BasisDivergence);
    }
    if let Some(term) = term {
        terms.push(term);
    }
    for &assertion in added_assertions {
        ledger.checkpoint()?;
        assertions.push(assertion);
    }
    sort_by_key(&mut terms, |term| term.id, ledger)?;
    sort_by_key(&mut assertions, |assertion| assertion.id, ledger)?;
    let graph_digest = typed::graph_digest(&terms, &assertions, ledger).map_err(digest_error)?;
    let snapshot = SnapshotId {
        kb_uuid: basis.basis.kb_uuid,
        version: KbVersion::new(version_next(store.head())?),
        rules: RuleSet::from_trusted_parts(
            basis.rules.profile,
            RulesDigest::new(Digest::from_bytes(basis.rules.rules_digest)),
        ),
        graph_digest: GraphDigest::new(Digest::from_bytes(graph_digest)),
    };
    BaseGraph::index_admitted_inputs(snapshot, terms, assertions, ledger).map_err(|error| {
        match error {
            GraphBuildRefusal::Refused(refusal) => CommandError::Refused(refusal),
            GraphBuildRefusal::Invalid(_) => {
                CommandError::Internal(InternalDefect::EncodeDecodeDivergence)
            }
        }
    })
}

pub(crate) fn validate(
    store: &Store<impl FileSystem>,
    basis: &CommitBasis,
    addition: Addition<'_>,
    budget: Budget,
    control: &dyn QueryControl,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CandidateError> {
    let graph = build(store, basis, addition, ledger).map_err(CandidateError::Command)?;
    let report = validate_candidate(&graph, budget, control).map_err(CandidateError::Validation)?;
    if report.is_valid() {
        return Ok(());
    }
    let mut violations =
        admitted_vec(report.violations.len(), ledger).map_err(CandidateError::Command)?;
    violations.extend_from_slice(&report.violations);
    Err(CandidateError::Invalid(violations))
}
