//! Fixed relation registry and the versioned rule profiles.
//!
//! The engine supports a closed set of relations. A relation ID selects a rule
//! template from `docs/design/data-model.md` §4; clients cannot submit new
//! semantics, and adding one is a contract change, not a data change.
//!
//! Assertions keep their authored direction. Symmetric relations may be
//! normalised for computation keys, but the stored assertion's `from`/`target`
//! are never rewritten (see `docs/design/axiomatics.md` §4).

use core::fmt;

use crate::digest::RulesDigest;

/// One of the seven relations the engine can evaluate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RelationId {
    /// Subsumption: a kind of.
    IsA,
    /// Composition: a part of.
    PartOf,
    /// Abstraction instance: an instance of.
    Instantiates,
    /// Prerequisite: requires.
    DependsOn,
    /// Navigation only; not the inverse of `depends-on`.
    Enables,
    /// Recorded tension; not logical negation.
    Contradicts,
    /// Structural analogy; not an assertion of equality.
    AnalogousTo,
}

impl RelationId {
    /// All relations, in ascending ID order.
    ///
    /// This order is part of the deterministic visit order: inference scans
    /// adjacency as `(relation_id, target_id, assertion_id)`.
    pub const ALL: [Self; 7] = [
        Self::IsA,
        Self::PartOf,
        Self::Instantiates,
        Self::DependsOn,
        Self::Enables,
        Self::Contradicts,
        Self::AnalogousTo,
    ];

    /// Stable wire, Markdown and filename name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IsA => "is-a",
            Self::PartOf => "part-of",
            Self::Instantiates => "instantiates",
            Self::DependsOn => "depends-on",
            Self::Enables => "enables",
            Self::Contradicts => "contradicts",
            Self::AnalogousTo => "analogous-to",
        }
    }

    /// Numeric relation ID used in canonical encoding and ordering.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::IsA => 0,
            Self::PartOf => 1,
            Self::Instantiates => 2,
            Self::DependsOn => 3,
            Self::Enables => 4,
            Self::Contradicts => 5,
            Self::AnalogousTo => 6,
        }
    }

    /// Decode a persisted relation ID, or `None` when it is unknown.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::IsA),
            1 => Some(Self::PartOf),
            2 => Some(Self::Instantiates),
            3 => Some(Self::DependsOn),
            4 => Some(Self::Enables),
            5 => Some(Self::Contradicts),
            6 => Some(Self::AnalogousTo),
            _ => None,
        }
    }

    /// The rule template this relation uses under `profile`.
    #[must_use]
    pub const fn spec(self, profile: RuleProfile) -> RelationSpec {
        let definition = matches!(self, Self::IsA | Self::PartOf | Self::Instantiates);
        // `instantiates` is a definition edge but not transitive: the design table
        // gives it no R1 composition, only the R3 dependency implication.
        let transitive = matches!(self, Self::IsA | Self::PartOf | Self::DependsOn);
        RelationSpec {
            id: self,
            is_definition: definition,
            transitive,
            symmetric: matches!(self, Self::Contradicts | Self::AnalogousTo),
            acyclic: definition || matches!(self, Self::DependsOn),
            layer_rule: if definition {
                // formal-v1 tightens "target <= source" to "target < source".
                if matches!(profile, RuleProfile::FormalV1) {
                    LayerRule::TargetBelowSource
                } else {
                    LayerRule::TargetAtMostSource
                }
            } else {
                LayerRule::Unrestricted
            },
            implies_dependency: matches!(profile, RuleProfile::FormalV1) && definition,
        }
    }
}

impl fmt::Display for RelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Layer-direction constraint attached to a relation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LayerRule {
    /// Any authoring order is legal (`depends-on`, `enables`, ...).
    Unrestricted,
    /// Definition targets may sit on the same or a shallower layer.
    TargetAtMostSource,
    /// Definition targets must sit on a strictly shallower layer.
    TargetBelowSource,
}

impl LayerRule {
    /// Declarative table entry: does this rule accept `source -> target`?
    ///
    /// This is the rule itself, not a candidate validator; V3 in
    /// `docs/design/inference.md` §2 is the check that applies it to a graph.
    #[must_use]
    pub const fn accepts(
        self,
        source: crate::layer::SemanticLayer,
        target: crate::layer::SemanticLayer,
    ) -> bool {
        // Layers compare numerically: `Ordering` is not usable in a const context.
        let (source, target) = (source.get(), target.get());
        match self {
            Self::Unrestricted => true,
            Self::TargetAtMostSource => target <= source,
            Self::TargetBelowSource => target < source,
        }
    }
}

impl fmt::Display for LayerRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrestricted => f.write_str("unrestricted"),
            Self::TargetAtMostSource => f.write_str("target<=source"),
            Self::TargetBelowSource => f.write_str("target<source"),
        }
    }
}

/// The fixed rule template for one relation under one profile.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelationSpec {
    /// Relation this template applies to.
    pub id: RelationId,
    /// Whether R3 turns assertions of this relation into `depends-on` seeds.
    pub is_definition: bool,
    /// Whether R1 (transitive composition) applies.
    pub transitive: bool,
    /// Whether R2 (symmetry completion) applies.
    pub symmetric: bool,
    /// Whether cycles in this relation are rejected at commit.
    pub acyclic: bool,
    /// Layer constraint applied by V3.
    pub layer_rule: LayerRule,
    /// Whether this relation implies `depends-on` under the active profile.
    pub implies_dependency: bool,
}

/// Versioned rule configuration.
///
/// A profile selects one of two fixed template sets. It never enables user
/// scripts, and changing it is an explicit migration transaction
/// (`docs/design/migration.md` §4), not an import side effect.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RuleProfile {
    /// Preserves the legacy edge semantics: layer direction is non-strict and
    /// definition edges do not imply `depends-on`.
    LegacyV1,
    /// Definition edges imply `depends-on`; definition targets must be strictly
    /// shallower, `instantiates` has at most one target per subject, and every
    /// active L3 term instantiates something.
    FormalV1,
}

impl RuleProfile {
    /// Both profiles, genesis order.
    pub const ALL: [Self; 2] = [Self::LegacyV1, Self::FormalV1];

    /// Persisted discriminant: `0` legacy-v1, `1` formal-v1.
    ///
    /// The wire number is part of the format (tags 160, 161 and 196), so it is
    /// declared here rather than derived from variant order in a codec.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::LegacyV1 => 0,
            Self::FormalV1 => 1,
        }
    }

    /// Decode a persisted discriminant; `None` outside `0..=1`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::LegacyV1),
            1 => Some(Self::FormalV1),
            _ => None,
        }
    }

    /// Stable profile name; also the input to the store's `rules_digest`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyV1 => "legacy-v1",
            Self::FormalV1 => "formal-v1",
        }
    }

    /// Profile used for newly created KBs.
    #[must_use]
    pub const fn default_for_new_kb() -> Self {
        Self::FormalV1
    }

    /// Profile used when migrating an existing KB.
    #[must_use]
    pub const fn default_for_migration() -> Self {
        Self::LegacyV1
    }

    /// Template for `relation`.
    #[must_use]
    pub const fn spec(self, relation: RelationId) -> RelationSpec {
        relation.spec(self)
    }

    /// Whether R3 fires for `relation` under this profile.
    #[must_use]
    pub const fn implies_dependency(self, relation: RelationId) -> bool {
        relation.spec(self).implies_dependency
    }
}

impl fmt::Display for RuleProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The immutable rule configuration a snapshot was evaluated under.
///
/// A [`RulesDigest`] alone cannot select `legacy-v1` vs `formal-v1`: a digest is
/// an opaque comparison value, and a reader holding only a digest cannot decide
/// whether R3 fires. The profile therefore travels with the graph and the query,
/// and the digest is compared *alongside* it rather than used instead of it.
///
/// # What this type does and does not guarantee
///
/// It pairs a profile with a digest **as supplied by a trusted caller**. It does
/// not verify that the digest was actually produced from that profile: P0 has no
/// hash function and no canonical rule serialization, so such a check would have
/// to be faked with a `debug_assert` or a constant comparison. Neither is a
/// runtime guarantee, and pretending otherwise is worse than stating the boundary.
///
/// Responsibilities are therefore split:
///
/// | layer | responsibility |
/// |---|---|
/// | `types` (P0) | carry the pair; compare whole pairs for equality |
/// | `graph` / `query` | reject a graph/query pairing whose pairs differ |
/// | `store` (P1) | generate the canonical digest from the rule text and verify it on load |
///
/// Consistency of the graph/query pair is enforced where both are available
/// ([`crate::query::SourceQuery::check_against`]), never here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RuleSet {
    profile: RuleProfile,
    digest: RulesDigest,
}

impl RuleSet {
    /// Pair a profile with the digest a trusted caller computed for it.
    ///
    /// The caller must have produced `digest` from `profile` under the P1
    /// canonical rule serialization. This constructor records that assertion; it
    /// does not check it, and it must not be presented as if it did.
    #[must_use]
    pub const fn from_trusted_parts(profile: RuleProfile, digest: RulesDigest) -> Self {
        Self { profile, digest }
    }

    /// The fixed template set in force.
    #[must_use]
    pub const fn profile(self) -> RuleProfile {
        self.profile
    }

    /// Digest over the profile and its full rule text, for cache and result
    /// identity.
    #[must_use]
    pub const fn digest(self) -> RulesDigest {
        self.digest
    }

    /// Template for one relation under this profile.
    #[must_use]
    pub const fn spec(self, relation: RelationId) -> RelationSpec {
        self.profile.spec(relation)
    }

    /// Whether R3 fires for `relation`.
    #[must_use]
    pub const fn implies_dependency(self, relation: RelationId) -> bool {
        self.profile.implies_dependency(relation)
    }
}

impl fmt::Display for RuleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.profile)
    }
}
