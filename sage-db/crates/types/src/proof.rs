//! Proof vocabulary.
//!
//! The vocabulary is shared so both executors produce comparable, replayable
//! proofs; the *construction* is not. Reference and optimized proofs do not have
//! to be byte-identical, and neither executor may call the other to derive an
//! expected value.
//!
//! Every node names the authored assertion revisions it rests on, so a replay
//! check can re-derive the annotation from the base graph without trusting the
//! producer. Index order carries the composition direction: `Transitive` keeps
//! `from → mid → to` as `(left, right)` and must never be swapped for
//! normalisation.

use core::fmt;
use core::mem::size_of;

use crate::identity::{AssertionId, RecordRevision};

/// Position of a node in one proof arena.
///
/// A [`ProofRef`] is only meaningful together with its arena; comparing handles
/// across responses is meaningless, and differential comparison of proofs must
/// compare replayed semantics, not these numbers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProofRef(u32);

impl ProofRef {
    /// Wrap a node position.
    ///
    /// Arena construction enforces that the position exists; a bare handle is
    /// never treated as proof of anything.
    #[must_use]
    pub const fn new(position: u32) -> Self {
        Self(position)
    }

    /// Numeric position within its arena.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One step of a proof.
///
/// The variants mirror the closed rule set R1–R3 plus the leaves: an authored
/// assertion, a symmetry completion, a definition-implied dependency, and a
/// transitive composition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProofNode {
    /// Leaf: a specific revision of an active assertion.
    Asserted {
        /// Assertion this leaf rests on.
        assertion_id: AssertionId,
        /// The assertion revision that was active at the proof's version.
        revision: RecordRevision,
    },
    /// R2 applied once. The premise still carries its authored direction.
    Symmetry {
        /// Proof of the authored direction.
        premise: ProofRef,
    },
    /// R3 applied once, from a definition assertion to `depends-on`.
    DefinitionImpliesDependency {
        /// Proof of the definition assertion.
        premise: ProofRef,
    },
    /// R1 applied once: `left` ends where `right` starts.
    ///
    /// `left` is the `from → mid` half and `right` is the `mid → to` half. The
    /// order is semantic, not cosmetic; a normalising swap would describe a
    /// different composition.
    Transitive {
        /// `from → mid` sub-proof.
        left: ProofRef,
        /// `mid → to` sub-proof.
        right: ProofRef,
    },
}

impl ProofNode {
    /// The premises this node references, in semantic order.
    pub fn premises(self) -> impl Iterator<Item = ProofRef> {
        match self {
            Self::Asserted { .. } => [None, None],
            Self::Symmetry { premise } | Self::DefinitionImpliesDependency { premise } => {
                [Some(premise), None]
            }
            Self::Transitive { left, right } => [Some(left), Some(right)],
        }
        .into_iter()
        .flatten()
    }

    /// Stable node-kind name, for diagnostics and test assertions.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Asserted { .. } => "asserted",
            Self::Symmetry { .. } => "symmetry",
            Self::DefinitionImpliesDependency { .. } => "definition-implies-dependency",
            Self::Transitive { .. } => "transitive",
        }
    }

    /// Resident bytes one arena slot costs, from the actual in-memory layout.
    ///
    /// This is **working memory**, not serialized size: the arena is charged for
    /// what the process holds while evaluating, so a padded enum is charged as
    /// padded. Serialization is a separate concern with its own budget.
    #[must_use]
    pub const fn slot_bytes() -> u64 {
        size_of::<Self>() as u64
    }
}

/// Why a proof arena could not accept a node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProofArenaError {
    /// A premise points at or beyond the node being defined, so the arena is not
    /// acyclic and cannot describe a finite derivation.
    ForwardReference {
        /// Node whose premise is invalid.
        node: u32,
        /// The invalid premise.
        premise: ProofRef,
    },
    /// The arena exceeds the request's `max_proof_nodes` allowance.
    NodeBudgetExceeded {
        /// Nodes attempted.
        attempted: u32,
    },
    /// The arena exceeds the request's `max_work_bytes` allowance.
    ByteBudgetExceeded {
        /// Bytes attempted.
        attempted: u64,
    },
    /// The arena would exceed `u32` positions.
    TooLarge {
        /// Nodes attempted.
        attempted: usize,
    },
    /// The allocator refused the next capacity growth.
    AllocationFailed {
        /// Slots attempted.
        attempted: usize,
    },
}

impl fmt::Display for ProofArenaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForwardReference { node, premise } => write!(
                f,
                "proof node {node} references non-earlier premise {}",
                premise.get()
            ),
            Self::NodeBudgetExceeded { attempted } => {
                write!(f, "proof arena exceeded node budget at {attempted} nodes")
            }
            Self::ByteBudgetExceeded { attempted } => {
                write!(f, "proof arena exceeded byte budget at {attempted} bytes")
            }
            Self::TooLarge { attempted } => {
                write!(f, "proof arena cannot hold {attempted} nodes")
            }
            Self::AllocationFailed { attempted } => {
                write!(f, "proof arena allocation failed at {attempted} slots")
            }
        }
    }
}

impl std::error::Error for ProofArenaError {}

/// Append-only arena of proof nodes.
///
/// Nodes are pushed in topological order, so a premise always precedes its use.
/// Construction checks that invariant and both resource limits; it does not replay
/// the proof, re-derive the annotation or evaluate any rule. Those obligations
/// belong to the executors and their independent verification tests.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProofArena {
    nodes: Vec<ProofNode>,
    bytes: u64,
}

impl ProofArena {
    /// Empty arena.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            bytes: 0,
        }
    }

    /// Append a node whose premises already exist.
    ///
    /// `max_nodes` bounds positions; `max_bytes` bounds **resident** arena bytes,
    /// measured as `capacity * size_of::<ProofNode>()`. Capacity is what the
    /// process actually holds, so growth is charged as it happens — a vector that
    /// doubles is charged for the doubled slot count, not for the logical length.
    ///
    /// # Errors
    ///
    /// - [`ProofArenaError::NodeBudgetExceeded`] past `max_nodes`;
    /// - [`ProofArenaError::ByteBudgetExceeded`] when the next capacity would pass
    ///   `max_bytes`;
    /// - [`ProofArenaError::TooLarge`] past `u32` positions;
    /// - [`ProofArenaError::AllocationFailed`] when the allocator refuses;
    /// - [`ProofArenaError::ForwardReference`] for a premise that does not point
    ///   strictly backwards.
    ///
    /// A rejected push changes nothing: no position is consumed, no capacity is
    /// reserved, and `nodes()`/`bytes()` are unchanged.
    pub fn push(
        &mut self,
        node: ProofNode,
        max_nodes: u32,
        max_bytes: u64,
    ) -> Result<ProofRef, ProofArenaError> {
        let position = self.nodes.len();
        let position_u32 = u32::try_from(position).map_err(|_| ProofArenaError::TooLarge {
            attempted: position,
        })?;
        if position_u32 >= max_nodes {
            return Err(ProofArenaError::NodeBudgetExceeded {
                attempted: position_u32.saturating_add(1),
            });
        }
        for premise in node.premises() {
            if premise.get() >= position_u32 {
                return Err(ProofArenaError::ForwardReference {
                    node: position_u32,
                    premise,
                });
            }
        }

        // Existing allocation is checked first: a caller may tighten `max_bytes`
        // below what the arena already holds (that is how a shared work budget is
        // enforced). Checking only on growth would let a push succeed while the
        // arena is already over its allowance.
        let resident = self.resident_bytes();
        if resident > max_bytes {
            return Err(ProofArenaError::ByteBudgetExceeded {
                attempted: resident,
            });
        }

        // Capacity next: charging a post-push length would understate a vector
        // that grew by doubling, and the process holds the *capacity* slots.
        if position == self.nodes.capacity() {
            let wanted = if self.nodes.capacity() == 0 {
                1
            } else {
                self.nodes.capacity().saturating_mul(2)
            };
            let wanted_bytes = (wanted as u64).saturating_mul(ProofNode::slot_bytes());
            if wanted_bytes > max_bytes {
                return Err(ProofArenaError::ByteBudgetExceeded {
                    attempted: wanted_bytes,
                });
            }
            self.nodes
                .try_reserve_exact(wanted - self.nodes.len())
                .map_err(|_| ProofArenaError::AllocationFailed { attempted: wanted })?;
        }

        self.nodes.push(node);
        self.bytes = self.resident_bytes();
        Ok(ProofRef::new(position_u32))
    }

    /// Resident bytes the arena currently holds.
    ///
    /// Capacity-based, because that is what the allocator keeps alive. This is
    /// what `max_work_bytes` bounds and what the owner must add to the shared
    /// work-area ledger rather than granting each arena a full budget.
    fn resident_bytes(&self) -> u64 {
        (self.nodes.capacity() as u64).saturating_mul(ProofNode::slot_bytes())
    }

    /// All nodes in topological order.
    #[must_use]
    pub fn nodes(&self) -> &[ProofNode] {
        &self.nodes
    }

    /// Number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the arena is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Resident bytes charged so far (capacity-based).
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Slots currently allocated.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.nodes.capacity()
    }

    /// Look up a node.
    #[must_use]
    pub fn get(&self, reference: ProofRef) -> Option<ProofNode> {
        self.nodes.get(reference.get() as usize).copied()
    }
}
