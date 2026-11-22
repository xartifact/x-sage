//! Semantic layer and derived definition depth.
//!
//! `layer` is an authored 0..3 classification stored on the term; it is the
//! authority for export directory names and ordering. `definition_depth` is a
//! derived topological metric on the definition DAG, unbounded above and never
//! a substitute for `layer` (see `docs/design/data-model.md` §3).

use core::fmt;

/// Authored semantic classification, L0..=L3.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SemanticLayer {
    /// Axiom: cannot be defined by other concepts.
    L0,
    /// Core vocabulary defined from L0.
    L1,
    /// Supporting concepts defined from L0+L1.
    L2,
    /// Instances of an abstraction in a concrete setting.
    L3,
}

impl SemanticLayer {
    /// All layers in ascending order.
    pub const ALL: [Self; 4] = [Self::L0, Self::L1, Self::L2, Self::L3];

    /// Numeric layer for encoding and export directory names.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::L0 => 0,
            Self::L1 => 1,
            Self::L2 => 2,
            Self::L3 => 3,
        }
    }

    /// Decode a persisted layer number.
    ///
    /// Returns `None` for anything outside 0..=3; the decoder must report a
    /// format error rather than clamping.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::L0),
            1 => Some(Self::L1),
            2 => Some(Self::L2),
            3 => Some(Self::L3),
            _ => None,
        }
    }

    /// The axiom predicate is derived from the layer, never stored separately.
    #[must_use]
    pub const fn is_axiom(self) -> bool {
        matches!(self, Self::L0)
    }
}

impl fmt::Display for SemanticLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}", self.get())
    }
}

/// Topological depth on the definition DAG: 0 for a node without definition
/// parents, otherwise `1 + max(parent depth)`.
///
/// Deliberately a different type from [`SemanticLayer`]: it is not restricted to
/// 0..=3, and an L3 instance may be shallower than an L1 concept.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DefinitionDepth(u32);

impl DefinitionDepth {
    /// Wrap a computed depth.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Numeric depth for ordering and reporting.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DefinitionDepth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a definition depth cannot be reported.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DepthUnavailable {
    /// The profile's definition-plus-dependency edge union contains a cycle.
    ///
    /// Under `legacy-v1` this only makes the metric unavailable; it does not
    /// invalidate existing data. Under `formal-v1` the same cycle is a commit
    /// error, so a snapshot carrying this value cannot be `formal-v1`.
    CyclicDefinitionUnion,
}

impl fmt::Display for DepthUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CyclicDefinitionUnion => f.write_str("definition edge union is cyclic"),
        }
    }
}

/// Result of the derived depth computation.
///
/// `unavailable` is not `absent`: the term exists, but the metric has no value
/// under the current rule profile.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DepthStatus {
    /// Computed depth.
    Available(DefinitionDepth),
    /// Metric undefined for this snapshot.
    Unavailable(DepthUnavailable),
}

impl DepthStatus {
    /// The depth, when available.
    #[must_use]
    pub const fn depth(self) -> Option<DefinitionDepth> {
        match self {
            Self::Available(depth) => Some(depth),
            Self::Unavailable(_) => None,
        }
    }
}
