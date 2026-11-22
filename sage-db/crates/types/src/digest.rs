//! Opaque 256-bit digests.
//!
//! The types crate never computes a digest; it only identifies one. Domain
//! separation, canonical encoding and the hash function belong to the format and
//! store layers (`docs/design/storage.md` §4). Keeping the value opaque prevents
//! two digests computed over different domains from being compared by accident.

use core::fmt;

/// A 32-byte digest whose domain is implied by the type that carries it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Identifies the active rule templates (profile and full template text).
///
/// It deliberately does **not** include the engine version: the rules decide what
/// may be derived, [`EngineVersion`] decides which build derived it. Mixing them
/// would make every upgrade look like a rule migration and would hide real ones.
///
/// It is a cache-key component and never part of `graph_digest`
/// (`docs/design/storage.md` §4.3).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RulesDigest(Digest);

impl RulesDigest {
    /// Wrap an already-computed digest.
    #[must_use]
    pub const fn new(digest: Digest) -> Self {
        Self(digest)
    }

    /// Underlying bytes.
    #[must_use]
    pub const fn as_digest(self) -> Digest {
        self.0
    }
}

/// Identifies the executor build that computed a result.
///
/// Deliberately separate from [`RulesDigest`]: the rules decide *what* may be
/// derived, the engine version decides *which implementation* derived it. Two
/// engine versions must agree on semantics for the same rules, so the engine
/// version is a cache/result provenance field and never an input to rule
/// evaluation or to `graph_digest`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EngineVersion(u32);

impl EngineVersion {
    /// Wrap an engine build number.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Numeric build number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Identifies the graph-relevant fields of one committed snapshot.
///
/// Two snapshots with equal `graph_digest` have the same active graph, so a
/// cached result may be reused; a cached result must never cross this boundary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GraphDigest(Digest);

impl GraphDigest {
    /// Wrap an already-computed digest.
    #[must_use]
    pub const fn new(digest: Digest) -> Self {
        Self(digest)
    }

    /// Underlying bytes.
    #[must_use]
    pub const fn as_digest(self) -> Digest {
        self.0
    }
}
