//! KB-scoped capability authorization.
//!
//! Public serving per `docs/design/server.md` §5 uses the five
//! named capabilities `read`, `propose`, `review`, `write_aux` and `admin`, and
//! those capabilities **never imply one another**: an `admin` grant does not
//! imply `read` or `review`, and a write-capable principal is not a reviewer.
//! Delegation across KBs does not exist in v1, so a grant names exactly one KB
//! and is refused for any other.
//!
//! This module is the decision point only. It performs no token verification:
//! [`Grants`] is a record of what trusted authentication already established, and
//! a caller must never build it from request payload fields such as a body-declared
//! author or `capturedBy` (those stay attribution, per `storage-format` field
//! notes). Authentication and credential storage are a later phase; until they
//! exist, the only legitimate producer of [`Grants`] is server-side auth code.
//!
//! Nothing here allocates: a capability set is a single copied byte, only the
//! principal string is borrowed, and a successful check returns another borrow.
//! The single ledger interaction is [`SharedLedger::checkpoint`], taken before the
//! grant is inspected so cancellation is observed even for an empty grant; no
//! byte or entry charge is made, because the check performs no counted work and a
//! fabricated charge would misreport a resource that was never reserved.
//!
//! A successful [`Authorized`] records that one check passed for one capability
//! on one KB for one principal. It is a result value, not a session token and not
//! a capability carrier: every later operation must run its own check.

use core::fmt;
use sage_db_types::framing::Refusal;
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;

/// One KB-scoped capability.
///
/// The private database listener can explicitly grant direct concept creation
/// in addition to its existing grants. No grant implies another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Capability {
    /// Read committed records and query results.
    Read,
    /// Submit a proposal for review; cannot approve one.
    Propose,
    /// Approve or reject a proposal; cannot author one.
    Review,
    /// Upload vectors/assets and write session state.
    WriteAux,
    /// Import, restore and upgrade rules; does not include read or review.
    Admin,
    /// Create one concept with initial relations. Available to trusted kernel callers.
    WriteConcept,
}

impl Capability {
    /// Stable name for logs and typed error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Propose => "propose",
            Self::Review => "review",
            Self::WriteAux => "write_aux",
            Self::Admin => "admin",
            Self::WriteConcept => "write_concept",
        }
    }

    /// The single bit this capability occupies in a [`CapabilitySet`].
    const fn bit(self) -> u8 {
        1u8 << (self as u8)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An explicit, ordered-independent set of granted capabilities.
///
/// Stored as one byte so a grant is `Copy` and building or testing it cannot
/// allocate. Membership is exact bit membership: there is deliberately no
/// inheritance rule anywhere in this type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct CapabilitySet {
    bits: u8,
}

impl CapabilitySet {
    /// The empty set: a principal that authenticated but holds no capability.
    pub const NONE: Self = Self { bits: 0 };

    /// Build a set from the listed capabilities.
    ///
    /// Duplicates are harmless. This is the only way a grant is populated; no
    /// capability is added implicitly.
    #[must_use]
    pub const fn of(capabilities: &[Capability]) -> Self {
        let mut bits = 0u8;
        let mut at = 0;
        while at < capabilities.len() {
            bits |= capabilities[at].bit();
            at += 1;
        }
        Self { bits }
    }

    /// The same set with `capability` added.
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self {
            bits: self.bits | capability.bit(),
        }
    }

    /// Whether `capability` was granted explicitly.
    #[must_use]
    pub const fn contains(self, capability: Capability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Whether nothing at all was granted.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// How many capabilities are granted.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.bits.count_ones()
    }
}

/// What a verified principal was granted on one KB.
///
/// Constructing this value asserts nothing cryptographic: it is the output of
/// authentication, not a credential. `principal` and the KB are borrowed/copied
/// so building a grant never allocates. An empty principal is representable here
/// (the type records what auth produced) and is refused by [`Grants::authorize`]
/// as [`AuthError::Unauthenticated`].
#[derive(Clone, Copy, Debug)]
pub struct Grants<'a> {
    principal: &'a str,
    kb: KbUuid,
    capabilities: CapabilitySet,
}

impl<'a> Grants<'a> {
    /// Record a principal's capabilities on one KB.
    ///
    /// The caller must be authentication code holding a verified identity; this
    /// constructor performs no verification of its own.
    #[must_use]
    pub const fn new(principal: &'a str, kb: KbUuid, capabilities: CapabilitySet) -> Self {
        Self {
            principal,
            kb,
            capabilities,
        }
    }

    /// The authenticated principal name (or free-form identity) that was granted.
    #[must_use]
    pub const fn principal(&self) -> &'a str {
        self.principal
    }

    /// The KB this grant is scoped to.
    #[must_use]
    pub const fn kb(&self) -> KbUuid {
        self.kb
    }

    /// The explicit capability set; no capability is added by reading it.
    #[must_use]
    pub const fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// Decide whether `kb` may run one operation that needs `capability`.
    ///
    /// Checks, in order: caller cancellation, authenticated principal, KB scope,
    /// then exact capability membership. Order matters — a stop request is
    /// honoured even for an empty grant, and an unauthenticated or cross-KB caller
    /// learns nothing about which capability was missing.
    ///
    /// # Errors
    ///
    /// [`AuthError::Refused`] with the caller's exact [`Refusal`] payload (never a
    /// synthesised allocation), [`AuthError::Unauthenticated`] for an empty
    /// principal, [`AuthError::WrongKb`] when the grant is scoped to another KB, or
    /// [`AuthError::MissingCapability`] when the capability was not granted
    /// explicitly.
    pub fn authorize(
        &self,
        kb: KbUuid,
        capability: Capability,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Authorized<'a>, AuthError> {
        ledger
            .checkpoint()
            .map_err(|stop| AuthError::Refused(Refusal::Stopped(stop)))?;
        if self.principal.is_empty() {
            return Err(AuthError::Unauthenticated);
        }
        if kb != self.kb {
            return Err(AuthError::WrongKb {
                expected: kb,
                found: self.kb,
            });
        }
        if !self.capabilities.contains(capability) {
            return Err(AuthError::MissingCapability { capability });
        }
        Ok(Authorized {
            principal: self.principal,
            kb,
            capability,
        })
    }
}

/// A principal admitted for exactly one capability on one KB.
///
/// It borrows the principal string and copies the KB and capability values; it is
/// **not** lifetime-tied to the [`Grants`] value that produced it, so it may
/// outlive a local `Grants`. That is why it is a record of one completed check and
/// not a token: being `Copy`, a value of this type can be cloned and reused for an
/// unrelated later operation, and no type-level rule prevents that. Every
/// operation must call [`Grants::authorize`] again against its own KB and
/// capability; holding an `Authorized` is never itself permission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Authorized<'a> {
    principal: &'a str,
    kb: KbUuid,
    capability: Capability,
}

impl<'a> Authorized<'a> {
    /// The authenticated principal, non-empty by construction.
    #[must_use]
    pub const fn principal(&self) -> &'a str {
        self.principal
    }

    /// The KB this authorization applies to.
    #[must_use]
    pub const fn kb(&self) -> KbUuid {
        self.kb
    }

    /// The capability that was granted and checked.
    #[must_use]
    pub const fn capability(&self) -> Capability {
        self.capability
    }
}

/// Why an authorization check did not admit the operation.
///
/// The three authorization outcomes are distinct because their remedies differ:
/// authenticate, correct the KB, or obtain the capability. Any of them may also
/// be pre-empted by the caller's own stop or budget refusal, which keeps its
/// canonical payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthError {
    /// No verified principal: denied before any scope or capability is examined.
    Unauthenticated,
    /// The grant belongs to a different KB; v1 has no cross-KB delegation.
    WrongKb {
        /// The KB the operation targets.
        expected: KbUuid,
        /// The KB the grant was issued for.
        found: KbUuid,
    },
    /// The principal is known and in scope, but this capability is not granted.
    MissingCapability {
        /// Capability that was required and not granted.
        capability: Capability,
    },
    /// The caller's ledger refused or stopped the check; payload preserved.
    Refused(Refusal),
}

impl From<Refusal> for AuthError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("unauthenticated"),
            Self::WrongKb { expected, found } => {
                write!(
                    f,
                    "grant is scoped to kb {found}, operation targets {expected}"
                )
            }
            Self::MissingCapability { capability } => {
                write!(f, "permission denied: {capability} not granted")
            }
            Self::Refused(refusal) => write!(f, "authorization check refused: {refusal:?}"),
        }
    }
}

impl std::error::Error for AuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop};

    const ALL: [Capability; 6] = [
        Capability::Read,
        Capability::Propose,
        Capability::Review,
        Capability::WriteAux,
        Capability::Admin,
        Capability::WriteConcept,
    ];

    fn kb(seed: u8) -> KbUuid {
        KbUuid::from_bytes([seed; 16])
    }

    /// Ledger that records every interaction, so a fabricated charge or a missing
    /// checkpoint is observable rather than inferred.
    struct Probe {
        checkpoints: u32,
        entry_charges: u32,
        byte_charges: u32,
        stop: Option<ControlStop>,
    }

    impl Probe {
        fn open() -> Self {
            Self {
                checkpoints: 0,
                entry_charges: 0,
                byte_charges: 0,
                stop: None,
            }
        }
    }

    impl SharedLedger for Probe {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            self.byte_charges += 1;
            Err(BudgetError::new(BudgetUnit::Bytes, 1, 0))
        }

        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            self.entry_charges += 1;
            Err(BudgetError::new(BudgetUnit::Entries, 1, 0))
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.checkpoints += 1;
            match self.stop {
                Some(stop) => Err(stop),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn proposer_denied_review() {
        let grants = Grants::new(
            "agent-7",
            kb(1),
            CapabilitySet::of(&[Capability::Propose, Capability::Read]),
        );
        let mut ledger = Probe::open();
        assert_eq!(
            grants.authorize(kb(1), Capability::Review, &mut ledger),
            Err(AuthError::MissingCapability {
                capability: Capability::Review
            })
        );
        assert_eq!(ledger.checkpoints, 1);
    }

    #[test]
    fn reviewer_denied_propose_without_explicit_grant() {
        let grants = Grants::new("alice", kb(2), CapabilitySet::of(&[Capability::Review]));
        let mut ledger = Probe::open();
        assert_eq!(
            grants.authorize(kb(2), Capability::Propose, &mut ledger),
            Err(AuthError::MissingCapability {
                capability: Capability::Propose
            })
        );
        // The reviewer grant is not diminished by the refusal.
        assert!(grants.capabilities().contains(Capability::Review));
    }

    #[test]
    fn admin_denied_read_without_explicit_grant() {
        let grants = Grants::new("ops", kb(3), CapabilitySet::of(&[Capability::Admin]));
        let mut ledger = Probe::open();
        assert_eq!(
            grants.authorize(kb(3), Capability::Read, &mut ledger),
            Err(AuthError::MissingCapability {
                capability: Capability::Read
            })
        );
        // Admin itself is usable, so this is an exact-capability rule, not a
        // degraded grant.
        assert_eq!(
            grants
                .authorize(kb(3), Capability::Admin, &mut ledger)
                .map(|a| a.capability()),
            Ok(Capability::Admin)
        );
    }

    #[test]
    fn grant_kb_isolation() {
        let grants = Grants::new("alice", kb(0x10), CapabilitySet::of(&ALL));
        let mut ledger = Probe::open();
        assert_eq!(
            grants.authorize(kb(0x11), Capability::Read, &mut ledger),
            Err(AuthError::WrongKb {
                expected: kb(0x11),
                found: kb(0x10)
            })
        );
        // Same grant, correct KB: it is the scope, not the capability, that failed.
        assert!(
            grants
                .authorize(kb(0x10), Capability::Read, &mut ledger)
                .is_ok()
        );
    }

    #[test]
    fn empty_principal_denied() {
        let grants = Grants::new("", kb(4), CapabilitySet::of(&ALL));
        let mut ledger = Probe::open();
        assert_eq!(
            grants.authorize(kb(4), Capability::Read, &mut ledger),
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(ledger.checkpoints, 1);
    }

    #[test]
    fn cancellation_on_empty_capability() {
        let grants = Grants::new("nobody", kb(5), CapabilitySet::NONE);
        for stop in [ControlStop::Cancelled, ControlStop::DeadlineExceeded] {
            let mut ledger = Probe {
                stop: Some(stop),
                ..Probe::open()
            };
            assert_eq!(
                grants.authorize(kb(5), Capability::Read, &mut ledger),
                Err(AuthError::Refused(Refusal::Stopped(stop)))
            );
            // The stop is observed before the empty grant is examined, and the
            // payload is the caller's own, not a substitute.
            assert_eq!(ledger.checkpoints, 1);
        }
    }

    #[test]
    fn permitted_exact_capability() {
        let grants = Grants::new("bob", kb(6), CapabilitySet::of(&[Capability::Review]));
        let mut ledger = Probe::open();
        let authorized = grants
            .authorize(kb(6), Capability::Review, &mut ledger)
            .expect("explicit capability is admitted");
        assert_eq!(authorized.principal(), "bob");
        assert_eq!(authorized.kb(), kb(6));
        assert_eq!(authorized.capability(), Capability::Review);
        assert_eq!(ledger.checkpoints, 1);
        // An authorization check reserves nothing; a fabricated charge would be
        // a resource lie.
        assert_eq!(ledger.entry_charges, 0);
        assert_eq!(ledger.byte_charges, 0);
    }

    #[test]
    fn no_capability_implies_another() {
        for &granted in &ALL {
            let grants = Grants::new("p", kb(7), CapabilitySet::of(&[granted]));
            let mut ledger = Probe::open();
            for &wanted in &ALL {
                let result = grants.authorize(kb(7), wanted, &mut ledger);
                assert_eq!(
                    result.is_ok(),
                    granted == wanted,
                    "granting {granted} must not admit {wanted}"
                );
            }
        }
        assert_eq!(CapabilitySet::of(&ALL).len(), 6);
        assert!(CapabilitySet::NONE.is_empty());
    }
}
