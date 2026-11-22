//! Object-level revision checks for a single immutable candidate basis.
//!
//! The caller supplies the current revision table built from one committed
//! snapshot. This module validates its ordering and key domains before borrowing
//! it. It neither observes disk authority nor constructs a snapshot itself.

use sage_db_types::framing::Refusal;
use sage_db_types::identity::{KbUuid, KbVersion, RecordRevision};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{ProposalRecord, ProposalStatus, RecordKey, RevPair};

/// A structurally invalid revision input, distinct from a stale baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionError {
    /// A key exceeds its record kind's identity range.
    InvalidKey(RecordKey),
    /// Current rows or the baseline are not strictly increasing by record key.
    UnorderedOrDuplicate(RecordKey),
    /// The caller supplied a revision table from another KB.
    WrongKb {
        /// The KB requested by the operation.
        expected: KbUuid,
        /// The KB attached to the supplied basis.
        found: KbUuid,
    },
    /// A terminal proposal cannot be applied again.
    ProposalNotPending(ProposalStatus),
    /// A required dependency has no baseline revision.
    MissingBaseline(RecordKey),
    /// A revision basis must come from a committed version, not pre-genesis.
    UncommittedBasis,
    /// The same caller ledger refused work or stopped it.
    Refused(Refusal),
}

/// One object changed or disappeared since the proposal was authored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevisionConflict {
    /// Object whose revision differs.
    pub key: RecordKey,
    /// Revision named by the proposal.
    pub expected: RecordRevision,
    /// Current revision; absent means the object does not exist on this basis.
    pub found: Option<RecordRevision>,
}

/// An approval baseline result; no proposal or content has been persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineVerdict {
    /// All declared revisions match on the checked basis.
    Current,
    /// The proposal must be handled as conflicted, not applied.
    Conflict(RevisionConflict),
}

/// A validated borrowed table of current identified records at one KB version.
///
/// It contains current revisions, not historical revisions. Construction does
/// not establish durability: its owner must derive it from one verified view.
/// Holding this immutable borrow prevents its backing table changing mid-check.
#[derive(Debug)]
pub struct RevisionSet<'a> {
    kb: KbUuid,
    version: KbVersion,
    rows: &'a [RevPair],
}

fn step(ledger: &mut dyn SharedLedger) -> Result<(), RevisionError> {
    ledger
        .checkpoint()
        .map_err(|e| RevisionError::Refused(Refusal::Stopped(e)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|e| RevisionError::Refused(Refusal::Budget(e)))
}

fn ordered_keys(
    keys: impl IntoIterator<Item = RecordKey>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), RevisionError> {
    let mut previous = None;
    for key in keys {
        step(ledger)?;
        if !key.id_is_in_range() {
            return Err(RevisionError::InvalidKey(key));
        }
        if previous.is_some_and(|p| p >= key) {
            return Err(RevisionError::UnorderedOrDuplicate(key));
        }
        previous = Some(key);
    }
    Ok(())
}

impl<'a> RevisionSet<'a> {
    /// Validate and borrow a current table, without copying any row.
    ///
    /// # Errors
    /// Invalid key, non-strict ordering, or the caller's actual budget/stop.
    pub fn new(
        kb: KbUuid,
        version: KbVersion,
        rows: &'a [RevPair],
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, RevisionError> {
        step(ledger)?;
        if !version.is_genesis_or_later() {
            return Err(RevisionError::UncommittedBasis);
        }
        ordered_keys(rows.iter().map(|r| r.key), ledger)?;
        Ok(Self { kb, version, rows })
    }

    /// The KB attached to this table.
    #[must_use]
    pub const fn kb(&self) -> KbUuid {
        self.kb
    }

    /// The table's committed version, for explicit administrative version checks.
    /// Approval never substitutes this value for object-level revision checks.
    #[must_use]
    pub const fn version(&self) -> KbVersion {
        self.version
    }

    /// Find one current revision with bounded comparison work.
    ///
    /// # Errors
    /// Invalid identity or an actual caller-ledger refusal.
    pub fn current(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<RecordRevision>, RevisionError> {
        step(ledger)?;
        if !key.id_is_in_range() {
            return Err(RevisionError::InvalidKey(key));
        }
        let mut lo = 0;
        let mut hi = self.rows.len();
        while lo < hi {
            step(ledger)?;
            let mid = lo + (hi - lo) / 2;
            match self.rows[mid].key.cmp(&key) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Ok(Some(self.rows[mid].revision)),
            }
        }
        Ok(None)
    }

    /// Check a proposal's baseline, without scanning any other proposal.
    ///
    /// `required` is the strictly ordered read/write dependency set produced by
    /// the payload planner for existing dependencies. Every required object must
    /// have a baseline. Creation/absence checks belong to the lifecycle planner;
    /// an absent record must never be represented as revision zero. The planner
    /// must derive the COMPLETE set from decoded
    /// payload semantics; passing an arbitrary subset is not full admission.
    /// This function validates the supplied set, not that upstream derivation.
    /// A conflict is a verdict to persist atomically by the future writer; this
    /// function does not update the proposal status in memory or on disk.
    ///
    /// # Errors
    /// Wrong KB, terminal proposal, missing dependency, malformed ordering or an
    /// actual ledger refusal. A stale or absent current object returns Conflict.
    pub fn check_approval(
        &self,
        kb: KbUuid,
        proposal: &ProposalRecord,
        required: &[RecordKey],
        ledger: &mut dyn SharedLedger,
    ) -> Result<BaselineVerdict, RevisionError> {
        step(ledger)?;
        if kb != self.kb {
            return Err(RevisionError::WrongKb {
                expected: kb,
                found: self.kb,
            });
        }
        check_approval_with(proposal, required, ledger, |key, ledger| {
            self.current(key, ledger)
        })
    }
}

// The payload planner supplies required existing-object dependencies. Both
// callers use this same validation, without materializing a second current set.
pub(crate) fn check_approval_with<E: From<RevisionError>>(
    proposal: &ProposalRecord,
    required: &[RecordKey],
    ledger: &mut dyn SharedLedger,
    mut lookup: impl FnMut(RecordKey, &mut dyn SharedLedger) -> Result<Option<RecordRevision>, E>,
) -> Result<BaselineVerdict, E> {
    step(ledger).map_err(E::from)?;
    if proposal.status != ProposalStatus::Pending {
        return Err(E::from(RevisionError::ProposalNotPending(proposal.status)));
    }
    ordered_keys(proposal.base_revisions.iter().map(|r| r.key), ledger).map_err(E::from)?;
    ordered_keys(required.iter().copied(), ledger).map_err(E::from)?;
    // Merge the ordered required keys and baseline without a temporary set.
    let mut at = 0;
    for &key in required {
        step(ledger).map_err(E::from)?;
        while at < proposal.base_revisions.len() && proposal.base_revisions[at].key < key {
            step(ledger).map_err(E::from)?;
            at += 1;
        }
        if proposal.base_revisions.get(at).map(|r| r.key) != Some(key) {
            return Err(E::from(RevisionError::MissingBaseline(key)));
        }
    }
    for row in &proposal.base_revisions {
        let found = lookup(row.key, ledger)?;
        if found != Some(row.revision) {
            return Ok(BaselineVerdict::Conflict(RevisionConflict {
                key: row.key,
                expected: row.revision,
                found,
            }));
        }
    }
    Ok(BaselineVerdict::Current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_store::lease::AttemptLedger;
    use sage_db_types::ledger::{BudgetError, BudgetUnit};
    use sage_db_types::persist::{ActorOrigin, CommonFields, Extensions, ProposalKind, RecordKind};
    use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

    const KB: KbUuid = KbUuid::from_bytes([1; 16]);

    fn row(id: u32, revision: u64) -> RevPair {
        RevPair {
            key: RecordKey::term(id),
            revision: RecordRevision::new(revision).unwrap(),
        }
    }

    fn proposal(baseline: Vec<RevPair>) -> ProposalRecord {
        ProposalRecord {
            key: RecordKey::new(RecordKind::Proposal, 7),
            revision: RecordRevision::FIRST,
            status: ProposalStatus::Pending,
            kind: ProposalKind::ConceptMarkdown,
            subject: RecordKey::term(1),
            base_revisions: baseline,
            payload_ref: [3; 32],
            review_note: None,
            declared_author: "author".into(),
            common: CommonFields {
                recorded_by: Some("author".into()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    #[test]
    fn unrelated_version_and_record_changes_do_not_conflict() {
        let mut ledger = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 1), row(2, 99)];
        let current = RevisionSet::new(KB, KbVersion::new(100), &rows, &mut ledger).unwrap();
        let proposal = proposal(vec![row(1, 1)]);
        assert_eq!(
            current.check_approval(KB, &proposal, &[RecordKey::term(1)], &mut ledger),
            Ok(BaselineVerdict::Current)
        );
        assert_eq!(current.version(), KbVersion::new(100));
    }

    #[test]
    fn edited_and_missing_objects_are_conflicts_with_real_revisions() {
        let mut ledger = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 2)];
        let current = RevisionSet::new(KB, KbVersion::new(3), &rows, &mut ledger).unwrap();
        let mut proposal = proposal(vec![row(1, 1)]);
        assert_eq!(
            current.check_approval(KB, &proposal, &[RecordKey::term(1)], &mut ledger),
            Ok(BaselineVerdict::Conflict(RevisionConflict {
                key: RecordKey::term(1),
                expected: RecordRevision::FIRST,
                found: RecordRevision::new(2)
            }))
        );
        proposal.subject = RecordKey::term(2);
        proposal.base_revisions = vec![row(2, 1)];
        assert_eq!(
            current.check_approval(KB, &proposal, &[RecordKey::term(2)], &mut ledger),
            Ok(BaselineVerdict::Conflict(RevisionConflict {
                key: RecordKey::term(2),
                expected: RecordRevision::FIRST,
                found: None
            }))
        );
    }

    #[test]
    fn missing_dependency_cannot_be_relabelled_current() {
        let mut ledger = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 1), row(2, 1)];
        let current = RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ledger).unwrap();
        let proposal = proposal(vec![row(1, 1)]);
        assert_eq!(
            current.check_approval(
                KB,
                &proposal,
                &[RecordKey::term(1), RecordKey::term(2)],
                &mut ledger
            ),
            Err(RevisionError::MissingBaseline(RecordKey::term(2)))
        );
    }

    #[test]
    fn every_declared_read_revision_matters_even_when_not_written() {
        let mut ledger = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 1), row(2, 2)];
        let current = RevisionSet::new(KB, KbVersion::new(3), &rows, &mut ledger).unwrap();
        let proposal = proposal(vec![row(1, 1), row(2, 1)]);
        assert!(
            matches!(current.check_approval(KB, &proposal, &[RecordKey::term(1)], &mut ledger),
            Ok(BaselineVerdict::Conflict(RevisionConflict { key, .. })) if key == RecordKey::term(2))
        );
    }

    #[test]
    fn terminal_proposals_are_not_reapplied_and_kb_identity_is_checked() {
        let mut ledger = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 1)];
        let current = RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ledger).unwrap();
        let mut proposal = proposal(vec![row(1, 1)]);
        for status in [
            ProposalStatus::Approved,
            ProposalStatus::Rejected,
            ProposalStatus::Conflicted,
        ] {
            proposal.status = status;
            assert_eq!(
                current.check_approval(KB, &proposal, &[RecordKey::term(1)], &mut ledger),
                Err(RevisionError::ProposalNotPending(status))
            );
        }
        assert_eq!(
            current.check_approval(KbUuid::from_bytes([2; 16]), &proposal, &[], &mut ledger),
            Err(RevisionError::WrongKb {
                expected: KbUuid::from_bytes([2; 16]),
                found: KB
            })
        );
    }

    #[test]
    fn malformed_tables_and_baselines_are_refused_not_normalized() {
        let mut ledger = AttemptLedger::open(1024, 512, &RunToCompletion).unwrap();
        for rows in [[row(1, 1), row(1, 2)], [row(2, 1), row(1, 1)]] {
            assert!(matches!(
                RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ledger),
                Err(RevisionError::UnorderedOrDuplicate(_))
            ));
        }
        let invalid = RecordKey::new(RecordKind::Term, u128::from(u32::MAX) + 1);
        let rows = [RevPair {
            key: invalid,
            revision: RecordRevision::FIRST,
        }];
        assert!(
            matches!(RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ledger), Err(RevisionError::InvalidKey(key)) if key == invalid)
        );
        assert!(matches!(
            RevisionSet::new(KB, KbVersion::PRE_GENESIS, &[], &mut ledger),
            Err(RevisionError::UncommittedBasis)
        ));
        let rows = [row(1, 1), row(2, 1)];
        let current = RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ledger).unwrap();
        let proposal = proposal(vec![row(2, 1), row(1, 1)]);
        assert!(matches!(
            current.check_approval(KB, &proposal, &[RecordKey::term(1)], &mut ledger),
            Err(RevisionError::UnorderedOrDuplicate(_))
        ));
    }

    #[test]
    fn actual_entry_budget_refuses_before_comparison_and_counts_no_bytes() {
        let mut ample = AttemptLedger::open(1024, 256, &RunToCompletion).unwrap();
        let rows = [row(1, 1)];
        let current = RevisionSet::new(KB, KbVersion::GENESIS, &rows, &mut ample).unwrap();
        let mut exhausted = AttemptLedger::open(1024, 0, &RunToCompletion).unwrap();
        assert_eq!(
            current.current(RecordKey::term(1), &mut exhausted),
            Err(RevisionError::Refused(Refusal::Budget(BudgetError::new(
                BudgetUnit::Entries,
                1,
                0
            ))))
        );
        assert_eq!(exhausted.stats().live_bytes, 0);
    }

    #[test]
    fn cancelled_empty_input_is_stopped_not_successful() {
        struct Stop;
        impl QueryControl for Stop {
            fn checkpoint(&self) -> Result<(), ControlStop> {
                Err(ControlStop::Cancelled)
            }
        }
        let mut ledger = AttemptLedger::open(1024, 256, &Stop).unwrap();
        assert!(matches!(
            RevisionSet::new(KB, KbVersion::GENESIS, &[], &mut ledger),
            Err(RevisionError::Refused(Refusal::Stopped(
                ControlStop::Cancelled
            )))
        ));
    }
}
