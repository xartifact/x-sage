//! The ordered fact table.
//!
//! This is the reference evaluator's whole mutable state: a `Vec` kept sorted by
//! `(relation, from, to)`, with every insertion charged against the work budget
//! and every growth done through a fallible reservation. It is deliberately not a
//! hash map: iteration order is part of the oracle's determinism, and a tree that
//! grows through infallible node allocation could not report a typed budget
//! failure.
//!
//! The table stores *reported directions*. R2 mirrors are separate rows with
//! `asserted == false`, so an authored direction is never rewritten.

use core::mem::size_of;

use sage_db_types::{
    Annotation, AssertionId, Confidence, QueryError, RecordRevision, RelationId, TermId,
};

use crate::work::{Work, reserve};

/// One fact direction: subject, relation, object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FactKey {
    /// Subject, in the reported direction.
    pub from: TermId,
    /// Relation of the fact.
    pub relation: RelationId,
    /// Object, in the reported direction.
    pub to: TermId,
}

impl FactKey {
    /// Build a key.
    pub const fn new(from: TermId, relation: RelationId, to: TermId) -> Self {
        Self { from, relation, to }
    }

    /// Internal ordering: `(relation, from, to)`.
    ///
    /// Grouping by `(relation, from)` has to be contiguous for the saturation
    /// scan, so the table is not ordered like the reported rows.
    const fn order(self) -> (u8, u32, u32) {
        (self.relation.get(), self.from.get(), self.to.get())
    }

    /// Reported row order, `(to, relation, from)`.
    pub fn sort_key(self) -> (TermId, RelationId, TermId) {
        (self.to, self.relation, self.from)
    }
}

impl Ord for FactKey {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.order().cmp(&other.order())
    }
}

impl PartialOrd for FactKey {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The strongest direct-authored support for one direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SupportWitness {
    /// Assertion identity.
    pub id: AssertionId,
    /// Revision active at the queried snapshot.
    pub revision: RecordRevision,
    /// Confidence authored on that assertion.
    pub confidence: Confidence,
}

impl SupportWitness {
    fn rank(self) -> (Confidence, core::cmp::Reverse<u128>) {
        (self.confidence, core::cmp::Reverse(self.id.get()))
    }

    /// The stronger of two witnesses: higher confidence, then lower assertion id.
    fn stronger(left: Self, right: Self) -> Self {
        if left.rank() >= right.rank() {
            left
        } else {
            right
        }
    }
}

/// Everything known about one fact direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactState {
    /// Effective annotation: `max` over every derivation found.
    pub annotation: Annotation,
    /// An authored assertion exists **in this direction**.
    pub asserted: bool,
    /// A non-circular rule derivation exists.
    pub derived: bool,
    /// Some contributing support or path is contested.
    pub disputed: bool,
    /// Strongest direct-authored support, if this direction has one.
    pub witness: Option<SupportWitness>,
}

impl FactState {
    /// Nothing known.
    pub const EMPTY: Self = Self {
        annotation: Annotation::ABSENT,
        asserted: false,
        derived: false,
        disputed: false,
        witness: None,
    };

    /// A direct-authored support at `confidence`.
    pub const fn authored(confidence: Confidence, witness: SupportWitness) -> Self {
        Self {
            annotation: Annotation::present(confidence),
            asserted: true,
            derived: false,
            disputed: matches!(confidence, Confidence::Contested),
            witness: Some(witness),
        }
    }

    /// Fold `other` into `self`; returns whether anything changed.
    ///
    /// Every field is monotone — annotations join, flags only ever rise — so the
    /// fixed point terminates and a later, weaker arrival can still raise
    /// `disputed` or `derived` without moving the annotation.
    pub fn merge(&mut self, other: Self) -> bool {
        let before = *self;
        self.annotation = self.annotation.join(other.annotation);
        self.asserted |= other.asserted;
        self.derived |= other.derived;
        self.disputed |= other.disputed;
        self.witness = match (self.witness, other.witness) {
            (None, other) | (other, None) => other,
            (Some(left), Some(right)) => Some(SupportWitness::stronger(left, right)),
        };
        *self != before
    }
}

/// One table slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Fact direction.
    pub key: FactKey,
    /// Aggregated state.
    pub state: FactState,
}

/// Sorted, budgeted fact table.
#[derive(Debug, Default)]
pub struct FactTable {
    entries: Vec<Entry>,
}

impl FactTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Number of stored facts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All entries, ordered by `(relation, from, to)`.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Look one fact up.
    pub fn get(&self, key: &FactKey) -> Option<&FactState> {
        match self.position(key) {
            Ok(position) => self.entries.get(position).map(|entry| &entry.state),
            Err(_) => None,
        }
    }

    fn position(&self, key: &FactKey) -> Result<usize, usize> {
        self.entries.binary_search_by(|entry| entry.key.cmp(key))
    }

    /// Fold `candidate` into `key`, inserting a slot when the fact is new.
    ///
    /// Returns whether the stored state changed. A new slot charges one work row
    /// and its bytes, and the growth is fallible: a hostile graph is refused with
    /// a typed budget error instead of aborting on allocation failure.
    pub fn merge(
        &mut self,
        key: FactKey,
        candidate: FactState,
        work: &mut Work,
    ) -> Result<bool, QueryError> {
        match self.position(&key) {
            Ok(position) => Ok(self.entries[position].state.merge(candidate)),
            Err(position) => {
                work.row()?;
                work.bytes(size_of::<Entry>() as u64)?;
                reserve(&mut self.entries, 1, work)?;
                self.entries.insert(
                    position,
                    Entry {
                        key,
                        state: candidate,
                    },
                );
                Ok(candidate.annotation.is_present())
            }
        }
    }

    /// Entries with `from == term` in `relation`, in `to` order.
    ///
    /// Contiguous because the table is ordered by `(relation, from, to)`.
    pub fn group(&self, relation: RelationId, term: TermId) -> &[Entry] {
        let start = self.entries.partition_point(|entry| {
            (entry.key.relation.get(), entry.key.from.get()) < (relation.get(), term.get())
        });
        let end = self.entries.partition_point(|entry| {
            (entry.key.relation.get(), entry.key.from.get()) <= (relation.get(), term.get())
        });
        &self.entries[start..end]
    }

    /// Copy the entries so the caller can scan while the table grows.
    ///
    /// The copy is charged and fallible; it is the reference's honest way to
    /// iterate and mutate at once without an infallible scratch structure.
    pub fn snapshot(&self, work: &mut Work) -> Result<Vec<Entry>, QueryError> {
        let mut copy: Vec<Entry> = Vec::new();
        reserve(&mut copy, self.entries.len(), work)?;
        copy.extend_from_slice(&self.entries);
        Ok(copy)
    }
}

/// Entries of `entries` with `from == term` in `relation`, in `to` order.
///
/// The slice is ordered by `(relation, from, to)`, so one partition gives the group.
pub fn group_of(entries: &[Entry], relation: RelationId, term: TermId) -> &[Entry] {
    let start = entries.partition_point(|entry| {
        (entry.key.relation.get(), entry.key.from.get()) < (relation.get(), term.get())
    });
    let end = entries.partition_point(|entry| {
        (entry.key.relation.get(), entry.key.from.get()) <= (relation.get(), term.get())
    });
    &entries[start..end]
}
