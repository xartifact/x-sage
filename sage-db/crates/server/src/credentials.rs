//! Borrowed credential-binding validation and bearer token verification.
//!
//! Design authority: `docs/design/server.md` §3, §5, §7.
//!
//! # Security and accounting boundaries
//!
//! * **Hashes only in configuration**: [`CredentialBinding`] stores a 32-byte
//!   SHA-256 [`TokenHash`], never a plaintext bearer token. Both [`TokenHash`]
//!   and [`CredentialBinding`] redact hash bytes in [`core::fmt::Debug`].
//! * **Constant-time hash equality**: [`TokenHash`] implements
//!   [`subtle::ConstantTimeEq`] and its [`PartialEq`] delegates directly to
//!   [`ConstantTimeEq::ct_eq`], so ordinary `==` cannot accidentally introduce
//!   a short-circuiting comparison over digest bytes. This module does **not**
//!   claim the entire [`CredentialVerifier::verify_bearer`] call is
//!   constant-time: input length checks, ledger charges/checkpoints, table
//!   cardinality, and refusal branches remain externally observable.
//! * **Trusted principal only**: [`CredentialVerifier::verify_bearer`] accepts
//!   only raw bearer bytes, the target [`KbUuid`], and the caller's
//!   [`SharedLedger`]. It never accepts a request-body-declared principal, so
//!   payload attribution cannot influence the returned [`Grants`].
//! * **Explicit KB isolation and non-inheriting capabilities**: a token may hold
//!   distinct [`CredentialBinding`] entries for different KBs, each with its own
//!   explicit [`CapabilitySet`]. Requesting an unbound KB, a wrong token, an
//!   empty token, or a revoked binding yields the single opaque
//!   [`VerifyError::Unauthenticated`] class with no principal, KB, capability,
//!   or revocation detail.
//! * **Runtime work ceiling, not entropy proof**: [`VerifierLimits::max_token_bytes`]
//!   bounds bearer bytes before hashing to limit CPU/ledger work. It does not
//!   measure or guarantee token entropy; high-entropy opaque token generation is
//!   a separate future dependency not implemented in this slice.
//! * **Single caller ledger on both construction and verification**:
//!   [`CredentialVerifier::new`] runs entry/exit checkpoints and charges 1 entry
//!   per binding row plus 1 entry per pairwise duplicate comparison (`n * (n - 1) / 2`)
//!   on the caller's startup [`SharedLedger`] without allocating an auxiliary
//!   index. [`VerifierLimits::max_bindings`] is a configuration input bound, not
//!   a promise that every value up to [`MAX_BINDINGS_CEILING`] fits inside any
//!   particular startup work allowance. Principal NUL scanning runs only after
//!   bounding length to at most [`MAX_PRINCIPAL_BYTES_CEILING`] (120 bytes).
//!   [`CredentialVerifier::verify_bearer`] uses one caller [`SharedLedger`]
//!   across entry checkpoint, bounded SHA-256 hashing (byte charge + per-chunk
//!   checkpoints), full binding table walk (checkpoint + 1 entry charge per row,
//!   no early exit on match), and exit checkpoint before returning [`Grants`].

use core::fmt;

use sage_db_kernel::auth::{CapabilitySet, Grants};
use sage_db_types::framing::Refusal;
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sha2::{Digest as _, Sha256};
use subtle::{Choice, ConstantTimeEq};

/// Upper bound on principal byte length aligned with `storage-format` (`MAX_PRINCIPAL_ID_BYTES`).
pub const MAX_PRINCIPAL_BYTES_CEILING: usize = 120;

/// Maximum configurable bearer token byte ceiling (64 KiB).
pub const MAX_TOKEN_BYTES_CEILING: usize = 64 * 1024;

/// Maximum configurable binding count accepted as a configuration input bound.
///
/// This is an input ceiling, **not** a promise that every table up to this size fits within any
/// given startup ledger allowance: allocation-free duplicate detection in
/// [`CredentialVerifier::new`] performs `n * (n - 1) / 2` pairwise comparisons metered through
/// the caller's startup [`SharedLedger`].
pub const MAX_BINDINGS_CEILING: usize = 65_536;

/// Default maximum bearer token length admitted before hashing (work ceiling, not entropy proof).
pub const DEFAULT_MAX_TOKEN_BYTES: usize = 512;

/// Default maximum number of credential bindings in one verifier table.
pub const DEFAULT_MAX_BINDINGS: usize = 4_096;

/// Default SHA-256 chunk size between [`SharedLedger::checkpoint`] calls.
pub const DEFAULT_HASH_CHUNK_BYTES: usize = 4_096;

/// A 32-byte SHA-256 digest of an opaque bearer token.
///
/// Equality via [`ConstantTimeEq`] compares all 32 bytes using `subtle`, and
/// [`PartialEq`] delegates directly to [`ConstantTimeEq::ct_eq`] so `==` cannot
/// silently short-circuit on digest bytes. [`fmt::Debug`] is redacted so hash
/// bytes never appear in diagnostic output.
#[derive(Clone, Copy)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Wrap a precomputed 32-byte SHA-256 digest.
    #[must_use]
    pub const fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw 32-byte digest explicitly (never exposed through `Debug`).
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl ConstantTimeEq for TokenHash {
    fn ct_eq(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }
}

impl PartialEq for TokenHash {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.ct_eq(other))
    }
}

impl Eq for TokenHash {}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHash([REDACTED])")
    }
}

/// Explicit lifecycle state of one credential binding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BindingStatus {
    /// Binding is active and may authenticate its exact principal, KB, and capabilities.
    Active,
    /// Binding is explicitly revoked and must be refused as [`VerifyError::Unauthenticated`].
    Revoked,
}

/// One borrowed credential binding mapping a token hash to a `(principal, kb, capabilities)` grant.
///
/// Plaintext tokens are never stored here. `principal` is borrowed so validating and scanning
/// bindings requires no owned copy of principal strings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CredentialBinding<'a> {
    token_hash: TokenHash,
    principal: &'a str,
    kb: KbUuid,
    capabilities: CapabilitySet,
    status: BindingStatus,
}

impl<'a> CredentialBinding<'a> {
    /// Construct a binding with an explicit [`BindingStatus`].
    #[must_use]
    pub const fn new(
        token_hash: TokenHash,
        principal: &'a str,
        kb: KbUuid,
        capabilities: CapabilitySet,
        status: BindingStatus,
    ) -> Self {
        Self {
            token_hash,
            principal,
            kb,
            capabilities,
            status,
        }
    }

    /// Construct an active binding.
    #[must_use]
    pub const fn active(
        token_hash: TokenHash,
        principal: &'a str,
        kb: KbUuid,
        capabilities: CapabilitySet,
    ) -> Self {
        Self::new(
            token_hash,
            principal,
            kb,
            capabilities,
            BindingStatus::Active,
        )
    }

    /// Construct an explicitly revoked binding.
    #[must_use]
    pub const fn revoked(
        token_hash: TokenHash,
        principal: &'a str,
        kb: KbUuid,
        capabilities: CapabilitySet,
    ) -> Self {
        Self::new(
            token_hash,
            principal,
            kb,
            capabilities,
            BindingStatus::Revoked,
        )
    }

    /// The stored 32-byte SHA-256 token digest.
    #[must_use]
    pub const fn token_hash(&self) -> TokenHash {
        self.token_hash
    }

    /// Borrowed principal identity granted when this binding is active and matched.
    #[must_use]
    pub const fn principal(&self) -> &'a str {
        self.principal
    }

    /// The single KB this binding is scoped to.
    #[must_use]
    pub const fn kb(&self) -> KbUuid {
        self.kb
    }

    /// Explicit, non-inheriting capability set for this `(token_hash, kb)` binding.
    #[must_use]
    pub const fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// Explicit active or revoked state.
    #[must_use]
    pub const fn status(&self) -> BindingStatus {
        self.status
    }

    /// Whether this binding is explicitly revoked.
    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        matches!(self.status, BindingStatus::Revoked)
    }
}

impl fmt::Debug for CredentialBinding<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialBinding")
            .field("token_hash", &self.token_hash)
            .field("principal", &self.principal)
            .field("kb", &self.kb)
            .field("capabilities", &self.capabilities)
            .field("status", &self.status)
            .finish()
    }
}

/// Configured runtime bounds for [`CredentialVerifier`] and [`hash_bearer_token`].
///
/// `max_token_bytes` is a runtime byte work ceiling checked before hashing; it is not an
/// entropy requirement or guarantee. `max_bindings` is a configuration input bound; pairwise
/// duplicate validation in [`CredentialVerifier::new`] still requires sufficient startup ledger
/// entry allowance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifierLimits {
    /// Maximum bearer token byte length accepted before hashing (`1..=MAX_TOKEN_BYTES_CEILING`).
    pub max_token_bytes: usize,
    /// Maximum number of bindings accepted in a verifier table (`1..=MAX_BINDINGS_CEILING`).
    pub max_bindings: usize,
    /// Maximum principal UTF-8 byte length (`1..=MAX_PRINCIPAL_BYTES_CEILING`).
    pub max_principal_bytes: usize,
    /// Bytes hashed per [`SharedLedger::checkpoint`] interval (`1..=MAX_TOKEN_BYTES_CEILING`).
    pub hash_chunk_bytes: usize,
}

impl VerifierLimits {
    /// Conservative v1 defaults aligned with `storage-format` principal limits and 4 KiB hash chunks.
    pub const DEFAULT: Self = Self {
        max_token_bytes: DEFAULT_MAX_TOKEN_BYTES,
        max_bindings: DEFAULT_MAX_BINDINGS,
        max_principal_bytes: MAX_PRINCIPAL_BYTES_CEILING,
        hash_chunk_bytes: DEFAULT_HASH_CHUNK_BYTES,
    };

    /// Validate that every configured limit is non-zero and within its declared ceiling.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidLimit`] naming the first invalid field.
    pub const fn validate(&self) -> Result<(), ConfigError> {
        if self.max_token_bytes == 0 || self.max_token_bytes > MAX_TOKEN_BYTES_CEILING {
            return Err(ConfigError::InvalidLimit {
                field: "max_token_bytes",
                value: self.max_token_bytes,
                max: MAX_TOKEN_BYTES_CEILING,
            });
        }
        if self.max_bindings == 0 || self.max_bindings > MAX_BINDINGS_CEILING {
            return Err(ConfigError::InvalidLimit {
                field: "max_bindings",
                value: self.max_bindings,
                max: MAX_BINDINGS_CEILING,
            });
        }
        if self.max_principal_bytes == 0 || self.max_principal_bytes > MAX_PRINCIPAL_BYTES_CEILING {
            return Err(ConfigError::InvalidLimit {
                field: "max_principal_bytes",
                value: self.max_principal_bytes,
                max: MAX_PRINCIPAL_BYTES_CEILING,
            });
        }
        if self.hash_chunk_bytes == 0 || self.hash_chunk_bytes > MAX_TOKEN_BYTES_CEILING {
            return Err(ConfigError::InvalidLimit {
                field: "hash_chunk_bytes",
                value: self.hash_chunk_bytes,
                max: MAX_TOKEN_BYTES_CEILING,
            });
        }
        Ok(())
    }
}

impl Default for VerifierLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Internal witness that [`VerifierLimits::validate`] succeeded, preventing silent clamping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ValidatedLimits(VerifierLimits);

impl ValidatedLimits {
    const fn try_new(limits: VerifierLimits) -> Result<Self, ConfigError> {
        match limits.validate() {
            Ok(()) => Ok(Self(limits)),
            Err(err) => Err(err),
        }
    }
}

/// Why a credential verifier configuration was rejected during construction or limit validation.
///
/// Preserves configuration defects separately from caller ledger [`Refusal`] payloads and never
/// synthesizes [`Refusal::Allocation`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// A configured limit was zero or exceeded its allowed ceiling.
    InvalidLimit {
        /// Name of the offending [`VerifierLimits`] field.
        field: &'static str,
        /// Configured value.
        value: usize,
        /// Maximum permitted value for that field.
        max: usize,
    },
    /// The binding table exceeded `limits.max_bindings`.
    TooManyBindings {
        /// Number of bindings supplied.
        count: usize,
        /// Configured maximum bindings.
        max: usize,
    },
    /// A binding declared an empty principal string.
    EmptyPrincipal {
        /// Zero-based index in the supplied binding slice.
        index: usize,
    },
    /// A binding declared a principal containing a forbidden NUL byte.
    PrincipalContainsNul {
        /// Zero-based index in the supplied binding slice.
        index: usize,
    },
    /// A binding declared a principal exceeding `limits.max_principal_bytes`.
    PrincipalTooLong {
        /// Zero-based index in the supplied binding slice.
        index: usize,
        /// Observed UTF-8 byte length.
        len: usize,
        /// Configured maximum principal byte length.
        max: usize,
    },
    /// Two bindings share the same `(token_hash, kb)` pair (including active/revoked ambiguity).
    DuplicateBinding {
        /// Index of the earlier binding.
        first_index: usize,
        /// Index of the conflicting later binding.
        second_index: usize,
        /// The KB where the collision occurred.
        kb: KbUuid,
    },
    /// The caller's startup [`SharedLedger`] refused entry budget or requested cancellation/stop.
    Refused(Refusal),
}

impl From<Refusal> for ConfigError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit { field, value, max } => {
                write!(
                    f,
                    "invalid verifier limit {field}={value} (must be 1..={max})"
                )
            }
            Self::TooManyBindings { count, max } => {
                write!(f, "binding table has {count} entries, exceeding max {max}")
            }
            Self::EmptyPrincipal { index } => {
                write!(f, "binding at index {index} has an empty principal")
            }
            Self::PrincipalContainsNul { index } => {
                write!(
                    f,
                    "binding at index {index} has a principal containing a NUL byte"
                )
            }
            Self::PrincipalTooLong { index, len, max } => {
                write!(
                    f,
                    "binding at index {index} principal length {len} exceeds max {max}"
                )
            }
            Self::DuplicateBinding {
                first_index,
                second_index,
                kb,
            } => {
                write!(
                    f,
                    "duplicate or conflicting token binding for kb {kb} at indices {first_index} and {second_index}"
                )
            }
            Self::Refused(refusal) => {
                write!(
                    f,
                    "credential configuration validation refused: {refusal:?}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Why bearer verification or token hashing did not succeed.
///
/// Keeps authentication refusal, input bound violation, configuration error, and caller ledger
/// [`Refusal`] distinct. Never synthesizes [`Refusal::Allocation`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyError {
    /// Missing (empty), unknown, revoked, or wrong-KB credential.
    ///
    /// Single opaque class: discloses neither whether a token exists, whether it is revoked,
    /// nor which principal or KB it belongs to.
    Unauthenticated,
    /// Candidate bearer bytes exceeded `limits.max_token_bytes` before hashing.
    TokenTooLong {
        /// Observed byte length of the candidate bearer token.
        len: usize,
        /// Configured maximum byte length.
        max: usize,
    },
    /// Supplied [`VerifierLimits`] failed validation before any byte or entry charge.
    InvalidConfig(ConfigError),
    /// Caller's [`SharedLedger`] refused budget or requested cancellation/deadline stop.
    Refused(Refusal),
}

impl From<Refusal> for VerifyError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<ConfigError> for VerifyError {
    fn from(error: ConfigError) -> Self {
        Self::InvalidConfig(error)
    }
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("unauthenticated"),
            Self::TokenTooLong { len, max } => {
                write!(f, "bearer token length {len} exceeds max {max}")
            }
            Self::InvalidConfig(error) => {
                write!(f, "invalid verifier configuration: {error}")
            }
            Self::Refused(refusal) => write!(f, "credential verification refused: {refusal:?}"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Compute the SHA-256 [`TokenHash`] of bounded bearer bytes using the caller's [`SharedLedger`].
///
/// Runs an entry checkpoint even on invalid limits or empty input, validates `limits` via
/// [`VerifierLimits::validate`] before any byte/entry admission (with no silent clamping),
/// refuses empty input as [`VerifyError::Unauthenticated`], checks bearer length against
/// `limits.max_token_bytes` before charging or hashing, charges `bearer_token.len() as u64` bytes,
/// and checkpoints before each `limits.hash_chunk_bytes` chunk.
///
/// # Errors
///
/// Returns [`VerifyError::Refused`] if `ledger` stops or exhausts byte budget,
/// [`VerifyError::InvalidConfig`] if `limits` fails validation,
/// [`VerifyError::Unauthenticated`] if `bearer_token` is empty, or
/// [`VerifyError::TokenTooLong`] if `bearer_token.len() > limits.max_token_bytes`.
pub fn hash_bearer_token(
    bearer_token: &[u8],
    limits: VerifierLimits,
    ledger: &mut dyn SharedLedger,
) -> Result<TokenHash, VerifyError> {
    // Entry checkpoint runs even on invalid limits or empty input before any work.
    ledger
        .checkpoint()
        .map_err(|stop| VerifyError::Refused(Refusal::Stopped(stop)))?;
    let validated = ValidatedLimits::try_new(limits).map_err(VerifyError::InvalidConfig)?;
    hash_validated_after_entry_checkpoint(bearer_token, validated, ledger)
}

/// Internal hashing helper whose caller has already executed the entry checkpoint and validated
/// `limits` (guaranteeing `hash_chunk_bytes >= 1` with no silent clamping).
fn hash_validated_after_entry_checkpoint(
    bearer_token: &[u8],
    limits: ValidatedLimits,
    ledger: &mut dyn SharedLedger,
) -> Result<TokenHash, VerifyError> {
    if bearer_token.is_empty() {
        return Err(VerifyError::Unauthenticated);
    }
    if bearer_token.len() > limits.0.max_token_bytes {
        return Err(VerifyError::TokenTooLong {
            len: bearer_token.len(),
            max: limits.0.max_token_bytes,
        });
    }

    ledger
        .try_charge(bearer_token.len() as u64)
        .map_err(|err| VerifyError::Refused(Refusal::Budget(err)))?;

    let mut hasher = Sha256::new();
    for chunk in bearer_token.chunks(limits.0.hash_chunk_bytes) {
        ledger
            .checkpoint()
            .map_err(|stop| VerifyError::Refused(Refusal::Stopped(stop)))?;
        hasher.update(chunk);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(TokenHash::from_sha256(digest))
}

/// Validated, borrowed credential verifier over a slice of hashed [`CredentialBinding`] rows.
///
/// Stores no plaintext tokens and borrows its binding slice.
#[derive(Clone, Copy, Debug)]
pub struct CredentialVerifier<'a> {
    bindings: &'a [CredentialBinding<'a>],
    limits: ValidatedLimits,
}

impl<'a> CredentialVerifier<'a> {
    /// Validate `limits` and `bindings` under the caller's startup [`SharedLedger`] and construct
    /// a verifier over the borrowed slice.
    ///
    /// Execution order:
    /// 1. Entry checkpoint (observed even for invalid limits or an empty binding slice).
    /// 2. [`VerifierLimits::validate`] and `bindings.len() <= limits.max_bindings` before any
    ///    entry admission.
    /// 3. Per-binding validation: checkpoint + 1 entry charge per row, rejecting empty principals,
    ///    principals longer than `limits.max_principal_bytes` (checked **before** NUL scanning so
    ///    principal-byte traversal is bounded by at most [`MAX_PRINCIPAL_BYTES_CEILING`] = 120),
    ///    and principals containing NUL bytes.
    /// 4. Pairwise duplicate check (`n * (n - 1) / 2` pairs, allocation-free): checkpoint + 1 entry
    ///    charge per pair, rejecting any colliding `(token_hash, kb)` pair including active/revoked
    ///    ambiguity. Same token hash across *different* KBs is permitted.
    /// 5. Exit checkpoint before returning the validated verifier (including when `bindings` is
    ///    empty).
    ///
    /// Because duplicate detection is quadratic without an auxiliary index, `limits.max_bindings`
    /// is an input ceiling rather than a promise that every size fits within an arbitrary startup
    /// ledger allowance.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] describing the first configuration violation or preserving the
    /// caller's [`Refusal`] via [`ConfigError::Refused`]. Never synthesizes byte charges or
    /// [`Refusal::Allocation`].
    pub fn new(
        bindings: &'a [CredentialBinding<'a>],
        limits: VerifierLimits,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, ConfigError> {
        ledger
            .checkpoint()
            .map_err(|stop| ConfigError::Refused(Refusal::Stopped(stop)))?;
        let validated = ValidatedLimits::try_new(limits)?;
        if bindings.len() > validated.0.max_bindings {
            return Err(ConfigError::TooManyBindings {
                count: bindings.len(),
                max: validated.0.max_bindings,
            });
        }

        for (index, binding) in bindings.iter().enumerate() {
            ledger
                .checkpoint()
                .map_err(|stop| ConfigError::Refused(Refusal::Stopped(stop)))?;
            ledger
                .try_charge_entries(1)
                .map_err(|err| ConfigError::Refused(Refusal::Budget(err)))?;

            if binding.principal.is_empty() {
                return Err(ConfigError::EmptyPrincipal { index });
            }
            // Length check runs BEFORE scanning bytes for NUL so traversal is strictly bounded
            // by `validated.0.max_principal_bytes <= MAX_PRINCIPAL_BYTES_CEILING` (120 bytes).
            if binding.principal.len() > validated.0.max_principal_bytes {
                return Err(ConfigError::PrincipalTooLong {
                    index,
                    len: binding.principal.len(),
                    max: validated.0.max_principal_bytes,
                });
            }
            if binding.principal.as_bytes().contains(&0) {
                return Err(ConfigError::PrincipalContainsNul { index });
            }
        }

        // Pairwise scan checks uniqueness of (token_hash, kb) without allocating an index.
        // Every pair is metered on the caller's startup ledger (checkpoint + 1 entry).
        for i in 0..bindings.len() {
            for j in (i + 1)..bindings.len() {
                ledger
                    .checkpoint()
                    .map_err(|stop| ConfigError::Refused(Refusal::Stopped(stop)))?;
                ledger
                    .try_charge_entries(1)
                    .map_err(|err| ConfigError::Refused(Refusal::Budget(err)))?;

                if bindings[i].kb == bindings[j].kb
                    && bool::from(bindings[i].token_hash.ct_eq(&bindings[j].token_hash))
                {
                    return Err(ConfigError::DuplicateBinding {
                        first_index: i,
                        second_index: j,
                        kb: bindings[i].kb,
                    });
                }
            }
        }

        // Exit checkpoint runs before releasing the constructed verifier, even on empty tables.
        ledger
            .checkpoint()
            .map_err(|stop| ConfigError::Refused(Refusal::Stopped(stop)))?;

        Ok(Self {
            bindings,
            limits: validated,
        })
    }

    /// Borrow the validated binding slice.
    #[must_use]
    pub const fn bindings(&self) -> &'a [CredentialBinding<'a>] {
        self.bindings
    }

    /// The validated runtime limits governing this verifier.
    #[must_use]
    pub const fn limits(&self) -> VerifierLimits {
        self.limits.0
    }

    /// Verify `bearer_token` for target `kb` using the caller's single [`SharedLedger`].
    ///
    /// Execution order:
    /// 1. Entry checkpoint (observed even for empty/oversized tokens or empty tables).
    /// 2. Empty-token refusal ([`VerifyError::Unauthenticated`]) and length ceiling check
    ///    ([`VerifyError::TokenTooLong`]) before charging or hashing.
    /// 3. Bounded SHA-256 hashing charging `bearer_token.len() as u64` bytes and checkpointing per
    ///    validated chunk.
    /// 4. Full walk over every binding row, running a checkpoint and charging 1 entry per row with
    ///    no early return on match; hash and KB comparisons use [`ConstantTimeEq`].
    /// 5. Exit checkpoint before returning [`Grants`] or [`VerifyError::Unauthenticated`].
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::Unauthenticated`] for empty, unknown, revoked, or wrong-KB tokens;
    /// [`VerifyError::TokenTooLong`] when `bearer_token` exceeds `limits.max_token_bytes`; or
    /// [`VerifyError::Refused`] when `ledger` stops or runs out of bytes/entries before completion.
    pub fn verify_bearer(
        &self,
        bearer_token: &[u8],
        kb: KbUuid,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Grants<'a>, VerifyError> {
        ledger
            .checkpoint()
            .map_err(|stop| VerifyError::Refused(Refusal::Stopped(stop)))?;
        let candidate_hash =
            hash_validated_after_entry_checkpoint(bearer_token, self.limits, ledger)?;

        let mut matched_index: Option<usize> = None;
        for (index, binding) in self.bindings.iter().enumerate() {
            ledger
                .checkpoint()
                .map_err(|stop| VerifyError::Refused(Refusal::Stopped(stop)))?;
            ledger
                .try_charge_entries(1)
                .map_err(|err| VerifyError::Refused(Refusal::Budget(err)))?;

            let hash_eq = candidate_hash.ct_eq(&binding.token_hash);
            let kb_eq = kb.as_bytes().ct_eq(binding.kb.as_bytes());
            if bool::from(hash_eq & kb_eq) {
                matched_index = Some(index);
            }
        }

        // Exit checkpoint runs before any grant or denial is released, including on empty tables.
        ledger
            .checkpoint()
            .map_err(|stop| VerifyError::Refused(Refusal::Stopped(stop)))?;

        let Some(index) = matched_index else {
            return Err(VerifyError::Unauthenticated);
        };
        let binding = &self.bindings[index];
        match binding.status {
            BindingStatus::Active => Ok(Grants::new(
                binding.principal,
                binding.kb,
                binding.capabilities,
            )),
            BindingStatus::Revoked => Err(VerifyError::Unauthenticated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_kernel::auth::{AuthError, Capability};
    use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop};

    // Independent SHA-256 literal vectors (NIST FIPS 180-4 "abc" and Python hashlib.sha256).
    const SHA256_ABC: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    const TOKEN_ALPHA_V1: &[u8] = b"sage-bearer-alpha-v1";
    const SHA256_ALPHA_V1: [u8; 32] = [
        0xea, 0xa4, 0xec, 0xa4, 0x91, 0x3d, 0x83, 0x82, 0xdd, 0x91, 0x61, 0x0b, 0xb7, 0x67, 0x4b,
        0x66, 0x2b, 0x57, 0x80, 0x3f, 0x62, 0x61, 0xab, 0xaa, 0x2e, 0xa8, 0xb6, 0x76, 0x06, 0xe0,
        0xc3, 0x2f,
    ];
    const TOKEN_ALPHA_V2: &[u8] = b"sage-bearer-alpha-v2";
    const SHA256_ALPHA_V2: [u8; 32] = [
        0x6b, 0x23, 0x55, 0x5a, 0xe4, 0xfb, 0xe2, 0x4f, 0x6e, 0xe9, 0x5b, 0xc6, 0x4f, 0x5b, 0xd0,
        0x94, 0x33, 0xec, 0xa6, 0x3f, 0x17, 0xfb, 0x1f, 0x4b, 0x19, 0xdb, 0x49, 0xda, 0xc5, 0x76,
        0xd1, 0xc2,
    ];
    const TOKEN_MULTI_KB: &[u8] = b"sage-bearer-multi-kb";
    const SHA256_MULTI_KB: [u8; 32] = [
        0x0d, 0xf3, 0x73, 0x41, 0xe7, 0xc7, 0x03, 0x91, 0x9b, 0x59, 0x26, 0x15, 0x4b, 0xb2, 0xe0,
        0x1c, 0x24, 0x14, 0x8b, 0x51, 0x18, 0x6f, 0x65, 0xd8, 0x13, 0xa4, 0x6e, 0x9f, 0xee, 0x8e,
        0x83, 0x3a,
    ];
    const TOKEN_WRONG: &[u8] = b"sage-bearer-wrong";

    const ALL_CAPS: [Capability; 5] = [
        Capability::Read,
        Capability::Propose,
        Capability::Review,
        Capability::WriteAux,
        Capability::Admin,
    ];

    fn kb(seed: u8) -> KbUuid {
        KbUuid::from_bytes([seed; 16])
    }

    /// Real caller ledger tracking exact byte charges, entry charges, and checkpoint stops.
    struct TestLedger {
        bytes_remaining: u64,
        entries_remaining: u64,
        bytes_charged: u64,
        entries_charged: u64,
        checkpoints: u64,
        stop_on_checkpoint: Option<(u64, ControlStop)>,
    }

    impl TestLedger {
        fn generous() -> Self {
            Self {
                bytes_remaining: 1024 * 1024,
                entries_remaining: 1024 * 1024,
                bytes_charged: 0,
                entries_charged: 0,
                checkpoints: 0,
                stop_on_checkpoint: None,
            }
        }

        fn with_budget(bytes: u64, entries: u64) -> Self {
            Self {
                bytes_remaining: bytes,
                entries_remaining: entries,
                bytes_charged: 0,
                entries_charged: 0,
                checkpoints: 0,
                stop_on_checkpoint: None,
            }
        }

        fn stopping_at(checkpoint_num: u64, stop: ControlStop) -> Self {
            Self {
                stop_on_checkpoint: Some((checkpoint_num, stop)),
                ..Self::generous()
            }
        }
    }

    impl SharedLedger for TestLedger {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.bytes_remaining {
                return Err(BudgetError::new(
                    BudgetUnit::Bytes,
                    bytes,
                    self.bytes_remaining,
                ));
            }
            self.bytes_remaining -= bytes;
            self.bytes_charged += bytes;
            Ok(())
        }

        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            if count > self.entries_remaining {
                return Err(BudgetError::new(
                    BudgetUnit::Entries,
                    count,
                    self.entries_remaining,
                ));
            }
            self.entries_remaining -= count;
            self.entries_charged += count;
            Ok(())
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.checkpoints += 1;
            if let Some((at, stop)) = self.stop_on_checkpoint
                && self.checkpoints >= at
            {
                return Err(stop);
            }
            Ok(())
        }
    }

    fn invalid_limit_cases() -> [(VerifierLimits, &'static str, usize, usize); 8] {
        [
            (
                VerifierLimits {
                    max_token_bytes: 0,
                    ..VerifierLimits::DEFAULT
                },
                "max_token_bytes",
                0,
                MAX_TOKEN_BYTES_CEILING,
            ),
            (
                VerifierLimits {
                    max_token_bytes: MAX_TOKEN_BYTES_CEILING + 1,
                    ..VerifierLimits::DEFAULT
                },
                "max_token_bytes",
                MAX_TOKEN_BYTES_CEILING + 1,
                MAX_TOKEN_BYTES_CEILING,
            ),
            (
                VerifierLimits {
                    max_bindings: 0,
                    ..VerifierLimits::DEFAULT
                },
                "max_bindings",
                0,
                MAX_BINDINGS_CEILING,
            ),
            (
                VerifierLimits {
                    max_bindings: MAX_BINDINGS_CEILING + 1,
                    ..VerifierLimits::DEFAULT
                },
                "max_bindings",
                MAX_BINDINGS_CEILING + 1,
                MAX_BINDINGS_CEILING,
            ),
            (
                VerifierLimits {
                    max_principal_bytes: 0,
                    ..VerifierLimits::DEFAULT
                },
                "max_principal_bytes",
                0,
                MAX_PRINCIPAL_BYTES_CEILING,
            ),
            (
                VerifierLimits {
                    max_principal_bytes: MAX_PRINCIPAL_BYTES_CEILING + 1,
                    ..VerifierLimits::DEFAULT
                },
                "max_principal_bytes",
                MAX_PRINCIPAL_BYTES_CEILING + 1,
                MAX_PRINCIPAL_BYTES_CEILING,
            ),
            (
                VerifierLimits {
                    hash_chunk_bytes: 0,
                    ..VerifierLimits::DEFAULT
                },
                "hash_chunk_bytes",
                0,
                MAX_TOKEN_BYTES_CEILING,
            ),
            (
                VerifierLimits {
                    hash_chunk_bytes: MAX_TOKEN_BYTES_CEILING + 1,
                    ..VerifierLimits::DEFAULT
                },
                "hash_chunk_bytes",
                MAX_TOKEN_BYTES_CEILING + 1,
                MAX_TOKEN_BYTES_CEILING,
            ),
        ]
    }

    #[test]
    fn independent_literal_sha256_vectors_match() {
        let mut ledger = TestLedger::generous();
        let h_abc =
            hash_bearer_token(b"abc", VerifierLimits::DEFAULT, &mut ledger).expect("hash abc");
        assert_eq!(h_abc.as_bytes(), &SHA256_ABC);
        assert_eq!(ledger.bytes_charged, 3);

        let h_v1 = hash_bearer_token(TOKEN_ALPHA_V1, VerifierLimits::DEFAULT, &mut ledger)
            .expect("hash alpha v1");
        assert_eq!(h_v1.as_bytes(), &SHA256_ALPHA_V1);

        let h_v2 = hash_bearer_token(TOKEN_ALPHA_V2, VerifierLimits::DEFAULT, &mut ledger)
            .expect("hash alpha v2");
        assert_eq!(h_v2.as_bytes(), &SHA256_ALPHA_V2);

        let h_multi = hash_bearer_token(TOKEN_MULTI_KB, VerifierLimits::DEFAULT, &mut ledger)
            .expect("hash multi kb");
        assert_eq!(h_multi.as_bytes(), &SHA256_MULTI_KB);
    }

    #[test]
    fn hash_bearer_token_validates_limits_before_admission_and_checkpoints_entry() {
        // Zero and above-ceiling limits on every field must return VerifyError::InvalidConfig
        // with zero byte/entry admission even when the ledger has 0 remaining budget.
        for (bad_limits, field, value, max) in invalid_limit_cases() {
            let mut zero_budget = TestLedger::with_budget(0, 0);
            let err = hash_bearer_token(TOKEN_ALPHA_V1, bad_limits, &mut zero_budget).unwrap_err();
            assert_eq!(
                err,
                VerifyError::InvalidConfig(ConfigError::InvalidLimit { field, value, max }),
                "invalid limit {field}={value} must fail validation without silent clamping"
            );
            assert_eq!(zero_budget.checkpoints, 1);
            assert_eq!(zero_budget.bytes_charged, 0);
            assert_eq!(zero_budget.entries_charged, 0);

            // Empty input with invalid limits also runs entry checkpoint and rejects invalid config.
            let mut empty_zero = TestLedger::with_budget(0, 0);
            assert_eq!(
                hash_bearer_token(b"", bad_limits, &mut empty_zero).unwrap_err(),
                VerifyError::InvalidConfig(ConfigError::InvalidLimit { field, value, max })
            );
            assert_eq!(empty_zero.checkpoints, 1);
            assert_eq!(empty_zero.bytes_charged, 0);
            assert_eq!(empty_zero.entries_charged, 0);

            // Entry stop is observed BEFORE config validation even for invalid + empty input.
            let mut stopped = TestLedger::stopping_at(1, ControlStop::Cancelled);
            assert_eq!(
                hash_bearer_token(b"", bad_limits, &mut stopped).unwrap_err(),
                VerifyError::Refused(Refusal::Stopped(ControlStop::Cancelled))
            );
            assert_eq!(stopped.checkpoints, 1);
            assert_eq!(stopped.bytes_charged, 0);
            assert_eq!(stopped.entries_charged, 0);
        }
    }

    #[test]
    fn token_hash_partial_eq_delegates_to_constant_time_eq() {
        let a = TokenHash::from_sha256(SHA256_ALPHA_V1);
        let a_copy = TokenHash::from_sha256(SHA256_ALPHA_V1);
        assert!(bool::from(a.ct_eq(&a_copy)));
        assert_eq!(a, a_copy);

        for flip_at in [0usize, 15, 31] {
            let mut bytes = SHA256_ALPHA_V1;
            bytes[flip_at] ^= 0x01;
            let b = TokenHash::from_sha256(bytes);
            assert!(!bool::from(a.ct_eq(&b)));
            assert_ne!(a, b);
        }
    }

    #[test]
    fn correct_token_returns_trusted_principal_and_exact_grants() {
        let bindings = [CredentialBinding::active(
            TokenHash::from_sha256(SHA256_ALPHA_V1),
            "trusted-agent-1",
            kb(1),
            CapabilitySet::of(&[Capability::Read, Capability::Propose]),
        )];
        let mut startup = TestLedger::generous();
        let verifier = CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut startup)
            .expect("valid bindings");
        assert_eq!(startup.bytes_charged, 0);
        assert_eq!(startup.entries_charged, 1);

        let mut ledger = TestLedger::generous();
        let grants = verifier
            .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut ledger)
            .expect("valid token must authenticate");
        assert_eq!(grants.principal(), "trusted-agent-1");
        assert_eq!(grants.kb(), kb(1));
        assert_eq!(
            grants.capabilities(),
            CapabilitySet::of(&[Capability::Read, Capability::Propose])
        );
        assert_eq!(ledger.bytes_charged, TOKEN_ALPHA_V1.len() as u64);
        assert_eq!(ledger.entries_charged, 1);
        // entry + 1 hash chunk + 1 binding row + exit = 4 checkpoints
        assert_eq!(ledger.checkpoints, 4);
    }

    #[test]
    fn wrong_empty_oversized_revoked_and_wrong_kb_refusals() {
        let bindings = [
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "alice",
                kb(1),
                CapabilitySet::of(&[Capability::Read]),
            ),
            CredentialBinding::revoked(
                TokenHash::from_sha256(SHA256_ALPHA_V2),
                "bob",
                kb(1),
                CapabilitySet::of(&ALL_CAPS),
            ),
        ];
        let limits = VerifierLimits {
            max_token_bytes: TOKEN_ALPHA_V1.len(),
            ..VerifierLimits::DEFAULT
        };
        let mut startup = TestLedger::generous();
        let verifier =
            CredentialVerifier::new(&bindings, limits, &mut startup).expect("valid config");

        // Wrong token -> single opaque Unauthenticated class.
        let mut ledger = TestLedger::generous();
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_WRONG, kb(1), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
        // Full table was walked (2 entries).
        assert_eq!(ledger.entries_charged, 2);

        // Empty token -> Unauthenticated without hashing or scanning, after entry checkpoint.
        let mut ledger = TestLedger::generous();
        assert_eq!(
            verifier.verify_bearer(b"", kb(1), &mut ledger).unwrap_err(),
            VerifyError::Unauthenticated
        );
        assert_eq!(ledger.checkpoints, 1);
        assert_eq!(ledger.bytes_charged, 0);
        assert_eq!(ledger.entries_charged, 0);

        // Exact max_token_bytes succeeds; max_token_bytes + 1 fails TokenTooLong BEFORE hashing.
        let mut ledger = TestLedger::generous();
        assert!(
            verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut ledger)
                .is_ok()
        );

        let mut oversized = TOKEN_ALPHA_V1.to_vec();
        oversized.push(b'x');
        let mut ledger = TestLedger::generous();
        assert_eq!(
            verifier
                .verify_bearer(&oversized, kb(1), &mut ledger)
                .unwrap_err(),
            VerifyError::TokenTooLong {
                len: oversized.len(),
                max: limits.max_token_bytes,
            }
        );
        assert_eq!(ledger.checkpoints, 1);
        assert_eq!(ledger.bytes_charged, 0);
        assert_eq!(ledger.entries_charged, 0);

        // Revoked token -> same Unauthenticated class (no revocation leak).
        let mut ledger = TestLedger::generous();
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_ALPHA_V2, kb(1), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );

        // Wrong KB -> same Unauthenticated class (no principal/KB leak).
        let mut ledger = TestLedger::generous();
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(2), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
    }

    #[test]
    fn same_token_explicit_per_kb_grants_and_other_kb_isolation() {
        let hash = TokenHash::from_sha256(SHA256_MULTI_KB);
        let bindings = [
            CredentialBinding::active(
                hash,
                "svc-kb1",
                kb(1),
                CapabilitySet::of(&[Capability::Read]),
            ),
            CredentialBinding::active(
                hash,
                "svc-kb2",
                kb(2),
                CapabilitySet::of(&[Capability::Propose, Capability::WriteAux]),
            ),
            CredentialBinding::revoked(hash, "svc-kb3", kb(3), CapabilitySet::of(&ALL_CAPS)),
        ];
        let mut startup = TestLedger::generous();
        let verifier = CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut startup)
            .expect("distinct KBs ok");
        let mut ledger = TestLedger::generous();

        let g1 = verifier
            .verify_bearer(TOKEN_MULTI_KB, kb(1), &mut ledger)
            .expect("kb1 active");
        assert_eq!(g1.principal(), "svc-kb1");
        assert!(g1.authorize(kb(1), Capability::Read, &mut ledger).is_ok());
        assert_eq!(
            g1.authorize(kb(1), Capability::Propose, &mut ledger),
            Err(AuthError::MissingCapability {
                capability: Capability::Propose
            })
        );
        assert_eq!(
            g1.authorize(kb(2), Capability::Read, &mut ledger),
            Err(AuthError::WrongKb {
                expected: kb(2),
                found: kb(1)
            })
        );

        let g2 = verifier
            .verify_bearer(TOKEN_MULTI_KB, kb(2), &mut ledger)
            .expect("kb2 active");
        assert_eq!(g2.principal(), "svc-kb2");
        assert!(
            g2.authorize(kb(2), Capability::Propose, &mut ledger)
                .is_ok()
        );
        assert!(
            g2.authorize(kb(2), Capability::WriteAux, &mut ledger)
                .is_ok()
        );
        assert_eq!(
            g2.authorize(kb(2), Capability::Read, &mut ledger),
            Err(AuthError::MissingCapability {
                capability: Capability::Read
            })
        );

        // Revoked on KB3 and unbound on KB4 both yield identical opaque Unauthenticated.
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_MULTI_KB, kb(3), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_MULTI_KB, kb(4), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
    }

    #[test]
    fn duplicate_and_invalid_config_refusals() {
        let h1 = TokenHash::from_sha256(SHA256_ALPHA_V1);

        // Duplicate active+active on same (token_hash, kb).
        let dup_active = [
            CredentialBinding::active(h1, "alice", kb(1), CapabilitySet::of(&[Capability::Read])),
            CredentialBinding::active(h1, "bob", kb(1), CapabilitySet::of(&[Capability::Admin])),
        ];
        assert_eq!(
            CredentialVerifier::new(
                &dup_active,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            )
            .unwrap_err(),
            ConfigError::DuplicateBinding {
                first_index: 0,
                second_index: 1,
                kb: kb(1),
            }
        );

        // Conflicting active+revoked ambiguity on same (token_hash, kb).
        let dup_revoked = [
            CredentialBinding::active(h1, "alice", kb(1), CapabilitySet::of(&[Capability::Read])),
            CredentialBinding::revoked(h1, "alice", kb(1), CapabilitySet::of(&[Capability::Read])),
        ];
        assert_eq!(
            CredentialVerifier::new(
                &dup_revoked,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            )
            .unwrap_err(),
            ConfigError::DuplicateBinding {
                first_index: 0,
                second_index: 1,
                kb: kb(1),
            }
        );

        // Duplicate revoked+revoked on same (token_hash, kb).
        let dup_both_revoked = [
            CredentialBinding::revoked(h1, "alice", kb(1), CapabilitySet::NONE),
            CredentialBinding::revoked(h1, "alice", kb(1), CapabilitySet::NONE),
        ];
        assert!(matches!(
            CredentialVerifier::new(
                &dup_both_revoked,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            ),
            Err(ConfigError::DuplicateBinding { .. })
        ));

        // Empty principal refused.
        let empty_principal = [CredentialBinding::active(
            h1,
            "",
            kb(1),
            CapabilitySet::of(&[Capability::Read]),
        )];
        assert_eq!(
            CredentialVerifier::new(
                &empty_principal,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            )
            .unwrap_err(),
            ConfigError::EmptyPrincipal { index: 0 }
        );

        // Principal with NUL byte within <= 120 bytes refused as PrincipalContainsNul.
        let nul_principal = [CredentialBinding::active(
            h1,
            "alice\0admin",
            kb(1),
            CapabilitySet::of(&[Capability::Read]),
        )];
        assert_eq!(
            CredentialVerifier::new(
                &nul_principal,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            )
            .unwrap_err(),
            ConfigError::PrincipalContainsNul { index: 0 }
        );

        // Principal exceeding max_principal_bytes (even with an embedded NUL byte) is rejected by
        // the length check BEFORE NUL traversal so principal-byte scanning stays bounded by <= 120.
        let mut long_with_nul = "p".repeat(MAX_PRINCIPAL_BYTES_CEILING);
        long_with_nul.push('\0');
        let too_long = [CredentialBinding::active(
            h1,
            &long_with_nul,
            kb(1),
            CapabilitySet::of(&[Capability::Read]),
        )];
        assert_eq!(
            CredentialVerifier::new(
                &too_long,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous()
            )
            .unwrap_err(),
            ConfigError::PrincipalTooLong {
                index: 0,
                len: MAX_PRINCIPAL_BYTES_CEILING + 1,
                max: MAX_PRINCIPAL_BYTES_CEILING,
            }
        );

        // Exceeding max_bindings refused with zero byte/entry admission.
        let two_distinct = [
            CredentialBinding::active(h1, "a", kb(1), CapabilitySet::NONE),
            CredentialBinding::active(h1, "a", kb(2), CapabilitySet::NONE),
        ];
        let small = VerifierLimits {
            max_bindings: 1,
            ..VerifierLimits::DEFAULT
        };
        let mut zero_budget = TestLedger::with_budget(0, 0);
        assert_eq!(
            CredentialVerifier::new(&two_distinct, small, &mut zero_budget).unwrap_err(),
            ConfigError::TooManyBindings { count: 2, max: 1 }
        );
        assert_eq!(zero_budget.checkpoints, 1);
        assert_eq!(zero_budget.bytes_charged, 0);
        assert_eq!(zero_budget.entries_charged, 0);

        // Invalid limits (zero or above ceiling) refused with zero byte/entry admission.
        for (bad, field, value, max) in invalid_limit_cases() {
            let mut led = TestLedger::with_budget(0, 0);
            assert_eq!(
                CredentialVerifier::new(&[], bad, &mut led).unwrap_err(),
                ConfigError::InvalidLimit { field, value, max },
                "expected InvalidLimit for {bad:?}"
            );
            assert_eq!(led.checkpoints, 1);
            assert_eq!(led.bytes_charged, 0);
            assert_eq!(led.entries_charged, 0);
        }
    }

    #[test]
    fn config_construction_ledger_entry_budget_and_control_stops() {
        // 4 distinct bindings -> 4 per-binding entries + 6 duplicate-pair entries = 10 entries,
        // 0 byte charges (never fabricated), and 1 + 4 + 6 + 1 = 12 checkpoints.
        let bindings = [
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "p1",
                kb(1),
                CapabilitySet::of(&[Capability::Read]),
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V2),
                "p2",
                kb(1),
                CapabilitySet::of(&[Capability::Propose]),
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_MULTI_KB),
                "p3",
                kb(1),
                CapabilitySet::of(&[Capability::Review]),
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ABC),
                "p4",
                kb(1),
                CapabilitySet::of(&[Capability::Admin]),
            ),
        ];

        // Exact budget with 0 bytes (proving no fabricated byte/allocation charge) and 10 entries.
        let mut exact = TestLedger::with_budget(0, 10);
        let verifier = CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut exact)
            .expect("0 bytes + 10 entries must succeed");
        assert_eq!(verifier.bindings().len(), 4);
        assert_eq!(exact.bytes_charged, 0);
        assert_eq!(exact.entries_charged, 10);
        assert_eq!(exact.checkpoints, 12);

        // Every entry allowance from 0..=9 fails during either per-binding (0..=3) or per-pair
        // (4..=9) validation, returning ConfigError::Refused intact.
        for allowance in 0..10u64 {
            let mut short = TestLedger::with_budget(0, allowance);
            assert_eq!(
                CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut short)
                    .unwrap_err(),
                ConfigError::Refused(Refusal::Budget(
                    BudgetError::new(BudgetUnit::Entries, 1, 0,)
                )),
                "entry budget {allowance} must refuse construction"
            );
            assert_eq!(short.entries_charged, allowance);
            assert_eq!(short.bytes_charged, 0);
        }

        // Control stop at EVERY checkpoint 1..=12 (entry, each row, each pair, exit) aborts.
        for cp in 1..=12 {
            let stop = if cp % 2 == 0 {
                ControlStop::DeadlineExceeded
            } else {
                ControlStop::Cancelled
            };
            let mut led = TestLedger::stopping_at(cp, stop);
            assert_eq!(
                CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut led).unwrap_err(),
                ConfigError::Refused(Refusal::Stopped(stop)),
                "stop at construction checkpoint {cp} must abort"
            );
            assert_eq!(led.checkpoints, cp);
            assert_eq!(led.bytes_charged, 0);
        }

        // Empty table runs both entry (1) and exit (2) checkpoints with 0 entries and 0 bytes.
        let mut empty_ok = TestLedger::with_budget(0, 0);
        let _ = CredentialVerifier::new(&[], VerifierLimits::DEFAULT, &mut empty_ok)
            .expect("empty table needs 0 entries");
        assert_eq!(empty_ok.checkpoints, 2);
        assert_eq!(empty_ok.entries_charged, 0);
        assert_eq!(empty_ok.bytes_charged, 0);

        for (cp, stop) in [
            (1, ControlStop::Cancelled),
            (2, ControlStop::DeadlineExceeded),
        ] {
            let mut led = TestLedger::stopping_at(cp, stop);
            assert_eq!(
                CredentialVerifier::new(&[], VerifierLimits::DEFAULT, &mut led).unwrap_err(),
                ConfigError::Refused(Refusal::Stopped(stop))
            );
            assert_eq!(led.checkpoints, cp);
        }
    }

    #[test]
    fn token_rotation_overlap_and_cutover_use_distinct_tokens() {
        let caps = CapabilitySet::of(&[Capability::Read, Capability::Propose]);
        // Phase 1 (overlap): both old (v1) and new (v2) tokens active for same (principal, kb).
        let overlap = [
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "agent-a",
                kb(7),
                caps,
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V2),
                "agent-a",
                kb(7),
                caps,
            ),
        ];
        let v_overlap = CredentialVerifier::new(
            &overlap,
            VerifierLimits::DEFAULT,
            &mut TestLedger::generous(),
        )
        .expect("overlap valid");
        let mut ledger = TestLedger::generous();
        let g1 = v_overlap
            .verify_bearer(TOKEN_ALPHA_V1, kb(7), &mut ledger)
            .expect("v1 works during overlap");
        let g2 = v_overlap
            .verify_bearer(TOKEN_ALPHA_V2, kb(7), &mut ledger)
            .expect("v2 works during overlap");
        assert_eq!(g1.principal(), "agent-a");
        assert_eq!(g2.principal(), "agent-a");

        // Phase 2 (cutover): v1 explicitly revoked, v2 remains active.
        let cutover = [
            CredentialBinding::revoked(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "agent-a",
                kb(7),
                caps,
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V2),
                "agent-a",
                kb(7),
                caps,
            ),
        ];
        let v_cutover = CredentialVerifier::new(
            &cutover,
            VerifierLimits::DEFAULT,
            &mut TestLedger::generous(),
        )
        .expect("cutover valid");
        assert_eq!(
            v_cutover
                .verify_bearer(TOKEN_ALPHA_V1, kb(7), &mut ledger)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
        let g_new = v_cutover
            .verify_bearer(TOKEN_ALPHA_V2, kb(7), &mut ledger)
            .expect("v2 remains active after v1 revoked");
        assert_eq!(g_new.principal(), "agent-a");
        assert!(
            g_new
                .authorize(kb(7), Capability::Propose, &mut ledger)
                .is_ok()
        );
    }

    #[test]
    fn all_five_capabilities_are_isolated_via_real_grants_authorize() {
        for &granted in &ALL_CAPS {
            let bindings = [CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "isolated-principal",
                kb(9),
                CapabilitySet::of(&[granted]),
            )];
            let verifier = CredentialVerifier::new(
                &bindings,
                VerifierLimits::DEFAULT,
                &mut TestLedger::generous(),
            )
            .expect("valid");
            let mut ledger = TestLedger::generous();
            let grants = verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(9), &mut ledger)
                .expect("verified");

            for &wanted in &ALL_CAPS {
                let auth = grants.authorize(kb(9), wanted, &mut ledger);
                if wanted == granted {
                    let ok = auth.expect("exact granted capability must be authorized");
                    assert_eq!(ok.principal(), "isolated-principal");
                    assert_eq!(ok.kb(), kb(9));
                    assert_eq!(ok.capability(), wanted);
                } else {
                    assert_eq!(
                        auth,
                        Err(AuthError::MissingCapability { capability: wanted }),
                        "granting {granted} must never imply {wanted}"
                    );
                }
            }
        }
    }

    #[test]
    fn token_hash_and_binding_debug_redact_hash_bytes() {
        let hash = TokenHash::from_sha256(SHA256_ALPHA_V1);
        let binding =
            CredentialBinding::active(hash, "alice", kb(1), CapabilitySet::of(&[Capability::Read]));

        let hash_dbg = format!("{hash:?}");
        assert_eq!(hash_dbg, "TokenHash([REDACTED])");

        let binding_dbg = format!("{binding:?}");
        assert!(
            binding_dbg.contains("TokenHash([REDACTED])"),
            "{binding_dbg}"
        );
        // Verify neither hex nor raw decimal fragments of the hash appear in Debug output.
        assert!(!binding_dbg.contains("eaa4"), "{binding_dbg}");
        assert!(!binding_dbg.contains("234, 164"), "{binding_dbg}");
        assert!(
            !binding_dbg.contains("sage-bearer-alpha-v1"),
            "{binding_dbg}"
        );
    }

    #[test]
    fn full_table_budget_and_control_stops_fail_before_returning_grant() {
        // Place the matching binding at index 0 of a 3-row table so early-exit bugs are exposed.
        let bindings = [
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V1),
                "first-row-match",
                kb(1),
                CapabilitySet::of(&[Capability::Admin]),
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_ALPHA_V2),
                "second-row",
                kb(1),
                CapabilitySet::of(&[Capability::Read]),
            ),
            CredentialBinding::active(
                TokenHash::from_sha256(SHA256_MULTI_KB),
                "third-row",
                kb(1),
                CapabilitySet::of(&[Capability::Propose]),
            ),
        ];
        let limits = VerifierLimits {
            hash_chunk_bytes: 8, // 20-byte token -> 3 hash chunks (8 + 8 + 4)
            ..VerifierLimits::DEFAULT
        };
        let verifier =
            CredentialVerifier::new(&bindings, limits, &mut TestLedger::generous()).expect("valid");
        let token_len = TOKEN_ALPHA_V1.len() as u64;

        // 1. Byte budget 1 byte short -> refused BEFORE hashing, 0 entries charged.
        let mut short_bytes = TestLedger::with_budget(token_len - 1, 10);
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut short_bytes)
                .unwrap_err(),
            VerifyError::Refused(Refusal::Budget(BudgetError::new(
                BudgetUnit::Bytes,
                token_len,
                token_len - 1,
            )))
        );
        assert_eq!(short_bytes.entries_charged, 0);

        // 2. Entry budget allows row 0 (match) and row 1, but NOT row 2 -> must FAIL before grant.
        let mut short_entries = TestLedger::with_budget(token_len, 2);
        assert_eq!(
            verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut short_entries)
                .unwrap_err(),
            VerifyError::Refused(Refusal::Budget(
                BudgetError::new(BudgetUnit::Entries, 1, 0,)
            ))
        );
        assert_eq!(short_entries.entries_charged, 2);

        // 3. Exact byte + entry budget (token_len bytes, 3 entries) succeeds.
        let mut exact = TestLedger::with_budget(token_len, 3);
        let ok = verifier
            .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut exact)
            .expect("exact full-table budget succeeds");
        assert_eq!(ok.principal(), "first-row-match");
        assert_eq!(exact.bytes_charged, token_len);
        assert_eq!(exact.entries_charged, 3);
        // Checkpoints: 1 (entry) + 3 (chunks) + 3 (rows) + 1 (exit) = 8.
        assert_eq!(exact.checkpoints, 8);

        // 4. Stop at EVERY single checkpoint (entry=1, mid-hash=2..=4, row0..2=5..=7, exit=8)
        // must fail BEFORE returning Grants.
        for cp in 1..=8 {
            let stop = if cp % 2 == 0 {
                ControlStop::DeadlineExceeded
            } else {
                ControlStop::Cancelled
            };
            let mut led = TestLedger::stopping_at(cp, stop);
            assert_eq!(
                verifier
                    .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut led)
                    .unwrap_err(),
                VerifyError::Refused(Refusal::Stopped(stop)),
                "stop at checkpoint {cp} must abort before grant"
            );
            assert_eq!(led.checkpoints, cp);
        }

        // 5. Empty token + entry stop -> Cancelled observed before Unauthenticated.
        let mut empty_stop = TestLedger::stopping_at(1, ControlStop::Cancelled);
        assert_eq!(
            verifier
                .verify_bearer(b"", kb(1), &mut empty_stop)
                .unwrap_err(),
            VerifyError::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );

        // 6. Empty binding table still executes entry, hash, and exit checkpoints.
        let empty_verifier =
            CredentialVerifier::new(&[], limits, &mut TestLedger::generous()).expect("empty valid");
        let mut empty_ok = TestLedger::generous();
        assert_eq!(
            empty_verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut empty_ok)
                .unwrap_err(),
            VerifyError::Unauthenticated
        );
        // 1 (entry) + 3 (chunks) + 0 (rows) + 1 (exit) = 5 checkpoints.
        assert_eq!(empty_ok.checkpoints, 5);
        let mut empty_exit_stop = TestLedger::stopping_at(5, ControlStop::DeadlineExceeded);
        assert_eq!(
            empty_verifier
                .verify_bearer(TOKEN_ALPHA_V1, kb(1), &mut empty_exit_stop)
                .unwrap_err(),
            VerifyError::Refused(Refusal::Stopped(ControlStop::DeadlineExceeded))
        );
    }
}
