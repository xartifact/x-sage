//! Confidence and the four-element annotation carrier.
//!
//! `K = { bottom < contested < hypothetical < confirmed }` with `max` as
//! addition and `min` as multiplication (see `docs/design/axiomatics.md` §2).
//! Absence of proof is its own element: it must never collapse with
//! `contested`, which means "present, and disputed".

use core::fmt;

/// One of the three levels at which a fact can be present.
///
/// The ordering is strength, not chronology: `Contested < Hypothetical < Confirmed`.
/// This is not a probability and carries no statistical meaning.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Confidence {
    /// Supported by at least one disputed assertion.
    Contested,
    /// Supported, but not reviewed or only tentatively established.
    Hypothetical,
    /// Supported by reviewed, confirmed evidence.
    Confirmed,
}

impl Confidence {
    /// All levels in ascending strength.
    pub const ALL: [Self; 3] = [Self::Contested, Self::Hypothetical, Self::Confirmed];

    /// Persisted discriminant, as `storage-format.md` tag 21 defines it.
    ///
    /// This is the *wire* number, not the strength order: `Contested` is 0 even
    /// though it is the weakest. A reader that sorted by this value would invert the
    /// lattice, so the two orders are kept visibly separate here.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::Contested => 0,
            Self::Hypothetical => 1,
            Self::Confirmed => 2,
        }
    }

    /// Decode a persisted discriminant; `None` outside `0..=2`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Contested),
            1 => Some(Self::Hypothetical),
            2 => Some(Self::Confirmed),
            _ => None,
        }
    }

    /// Stable wire and filename name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contested => "contested",
            Self::Hypothetical => "hypothetical",
            Self::Confirmed => "confirmed",
        }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Annotation carrier: bottom plus the three confidence levels.
///
/// `Annotation::ABSENT` is the bottom element. It is deliberately not
/// `Option<Confidence>` at the API edge: `join`/`meet` are the only algebra
/// exposed, and `Ord` is not implemented, so a caller cannot accidentally sort
/// absent above contested.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Annotation(Option<Confidence>);

impl Annotation {
    /// Bottom: no proof at all. Unit of [`Self::join`], absorber of [`Self::meet`].
    pub const ABSENT: Self = Self(None);

    /// A present fact at the given level.
    #[must_use]
    pub const fn present(confidence: Confidence) -> Self {
        Self(Some(confidence))
    }

    /// The level, if the fact is present at all.
    #[must_use]
    pub const fn confidence(self) -> Option<Confidence> {
        self.0
    }

    /// Whether a proof exists.
    #[must_use]
    pub const fn is_present(self) -> bool {
        self.0.is_some()
    }

    /// Alternative composition (path choice): `max`, with bottom as unit.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        match (self.0, other.0) {
            (None, other) => Self(other),
            (some, None) => Self(some),
            (Some(left), Some(right)) => Self(Some(if left as u8 >= right as u8 {
                left
            } else {
                right
            })),
        }
    }

    /// Conjunctive composition (path step): `min`, with bottom as absorber.
    #[must_use]
    pub const fn meet(self, other: Self) -> Self {
        match (self.0, other.0) {
            (Some(left), Some(right)) => Self(Some(if left as u8 <= right as u8 {
                left
            } else {
                right
            })),
            _ => Self::ABSENT,
        }
    }
}

impl fmt::Display for Annotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(confidence) => fmt::Display::fmt(&confidence, f),
            None => f.write_str("absent"),
        }
    }
}
