//! The canonical shared carrier for one prepared commit.
//!
//! # What this module is
//!
//! A fully prepared, structurally validated transaction handed from the preparing layer to the
//! store. The store **persists** it; it does not decide whether the change is *allowed*. Every
//! item here is data: there is no constructor that validates, no allocation helper and no
//! policy. Admission, ordering and encoding belong to their owners — `format` decides bytes,
//! `store` decides durability, the kernel decides permission (P2).
//!
//! # Why one shared carrier rather than one per crate
//!
//! Two real consumers need the same shape: the store's commit path and the format encoder's
//! input path. A second, structurally identical type in either crate would be a second source of
//! truth for a single hand-off, and the two would drift the first time a field was added. The
//! declarations therefore live in this crate, which both may depend on, and `sage-db-format`
//! re-exports them instead of redeclaring them.
//!
//! # The deliberate absence of an outcome field
//!
//! There is **no** `outcome` field. The outcome *is* one of [`OwnedEvent`]s — exactly one
//! `RequestOutcomeRecorded` (event tag 192) — and the format requires that event's `176`/`177`/
//! `178` payload to be byte-equal to the transaction header, with `181 == 192`. A second field
//! would be a second, contradictable source for the same bytes.
//!
//! # Why no conversion to the encoder's header type lives here
//!
//! [`OwnedEvent`] and [`RequestCanonVersion`] are here so a consumer can name them without
//! depending on the codec crate — `RequestCanonVersion` is canonical here even though
//! [`PreparedTransaction`] derives it rather than storing it. A conversion **into** the encoder's header type cannot live
//! here: this crate has no dependencies (the edge is `store -> format -> types`), so naming a
//! format type in an inherent method would invert that edge. The move happens in the store as a
//! free function over [`PreparedTransaction`], which consumes the owned `String`s and
//! [`Extensions`] exactly once.
//!
//! # Ownership and moving
//!
//! The carrier owns its two strings and its [`Extensions`] map so a caller can hand over values
//! it already built. Nothing in this module clones, reserves or charges; keeping it free of
//! those is what lets the consuming conversion be the only place a copy could appear.

use crate::identity::KbUuid;
use crate::persist::Extensions;
use crate::relation::RuleProfile;

/// Request-canonicalisation version of a transaction header.
///
/// A closed enum rather than a raw `u16`, so a caller cannot pass an unregistered value and the
/// two legal cases have names. It is **only** the closed domain `0`/`1`: it does not make the
/// version/sentinel join impossible in a generic header, because a header can still pair
/// `NonRequest` with a non-genesis version or with a non-zero payload digest. That join is checked
/// where the header is built and validated, not expressed by this type.
///
/// Declared here, not in `sage-db-format`, so a consumer can name it without depending on the
/// codec crate; the format crate re-exports this one. A [`PreparedTransaction`] does not carry it:
/// the header value is derived from `version`, so this type exists to name the derived value, not
/// to be stored as a redundant copy. The wire values are the format's: `0`
/// non-request, `1` canonical v1. An unregistered value is a format-level refusal, never a third
/// variant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RequestCanonVersion {
    /// Genesis: no caller request. Derived from `version == 1`, never the authority.
    NonRequest = 0,
    /// Caller request canonical form v1.
    V1 = 1,
}

impl RequestCanonVersion {
    /// The wire value.
    ///
    /// Exists because the header encoder writes this value directly; keeping the mapping here
    /// means the numeric correspondence has exactly one definition.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }
}

/// One event as owned bytes: an attempt-lifetime storage slot.
///
/// Field values are owned because the source may be a decoded record that goes out of scope
/// before the commit finishes. Field order is **not** validated here: an event is only
/// meaningful once an encoder has judged it, and a second opinion in this type would be a second
/// authority. Encoders expect `fields` in ascending tag order.
///
/// `event_seq` is never a field: it is the position of this event in
/// [`PreparedTransaction::events`], defined by appearance rather than stored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OwnedEvent {
    /// Registered event tag for this format major.
    pub tag: u16,
    /// Fields with their exact value bytes; field tags are unique within one event.
    pub fields: Vec<(u16, Vec<u8>)>,
}

/// What the transaction claims about the rules in force.
///
/// A binding, not a configuration: the *content* lives in a blob. The store verifies
/// `SHA-256(blob) == rules_ref` and that the domain-separated rules digest of the blob equals
/// `rules_digest`; it never compares against compiled constants, which would make a rules change
/// indistinguishable from a code change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RulesBinding {
    /// The profile this transaction was evaluated with (header tag 196).
    ///
    /// Closed [`RuleProfile`] rather than a raw `u16`: an unregistered profile is a format
    /// refusal, so carrying the enum keeps that domain closed at the carrier boundary.
    pub profile: RuleProfile,
    /// Claimed canonical rules digest (header tag 163), to be recomputed from the blob.
    pub rules_digest: [u8; 32],
    /// Blob holding the canonical rule bytes (**record** tag 166). Read and hashed, never trusted.
    ///
    /// Tag 166 is a record field (Genesis / RuleConfig), not a transaction-header slot; the header
    /// carries tag 163 and the profile, and the blob is reached through this record-level ref.
    pub rules_ref: [u8; 32],
}

/// One blob this transaction references, declared before publication.
///
/// The declared set is a **superset** of the blob references the format enumerates from the
/// events: re-declaring an already-present blob is legal, while referencing one that nobody
/// makes durable is refused. A superset may therefore leave durable orphans, which are reported
/// and never treated as committed references.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BlobDeclaration {
    /// Raw, undomained SHA-256 of the blob bytes.
    ///
    /// This is the one digest in the format that carries no domain prefix: the blob name *is*
    /// the plain content hash.
    pub sha256: [u8; 32],
    /// Length in bytes, as **verified** rather than predicted.
    ///
    /// The preparing layer states the length it measured from the bytes it holds, and the store
    /// re-measures before publishing. It is never a default `0`: a zero here declares a
    /// genuinely empty blob, not an unknown size.
    pub declared_len: u64,
    /// Where the bytes come from.
    pub source: BlobSource,
}

/// Where a declared blob's bytes come from.
///
/// Two roles and no third: either this attempt supplies the bytes, or the bytes are expected to
/// already be in place and must be verified before reuse. There is deliberately no reader trait
/// and no lazy source, because both would move I/O policy into a data carrier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BlobSource {
    /// The bytes this attempt writes to staging and then publishes.
    ///
    /// Owned, because the caller's buffer need not outlive preparation. An empty vector is a
    /// legal *supplied value* for a zero-length blob; it is never a placeholder for "unknown".
    Provide(Vec<u8>),
    /// The bytes are expected to already be **present**, at the declared digest.
    ///
    /// Carries no bytes: the store opens the existing file, verifies its length and hash, and
    /// refuses on mismatch instead of silently re-declaring a different digest.
    ///
    /// This variant does **not** mean the bytes are already durable. Presence is not durability:
    /// the store must still verify the actual inode and synchronize it — and its directory —
    /// before any durable claim, exactly as for a freshly written blob. A reused inode that was
    /// never synced is not durable, and treating "already there" as "already safe" is how a commit
    /// comes to claim more than the platform gave it.
    Existing,
}

/// A fully prepared, structurally validated transaction.
///
/// The store persists it; it does not judge it. Genesis is identified by `version == 1` together
/// with `prev_version == None` and the all-zero `payload_digest` sentinel. Those three values must
/// agree, and the carrier states them rather than deriving them so a wrong combination is visible
/// instead of impossible to express.
///
/// The header's canonicalisation version is **not** a field here: it is derived, not carried.
/// `version == 1` implies [`RequestCanonVersion::NonRequest`] and every later version implies
/// [`RequestCanonVersion::V1`], so a stored copy could only ever agree or contradict. Deriving it
/// removes the class of contradiction rather than validating against it. The payload sentinel is
/// still *checked* by the format; a wrong one is refused, never repaired.
///
/// No field here is bounded or validated by this type. The format owns the byte-level bounds
/// (for example the 120-byte principal and 128-byte request-id limits) and enforces them where
/// the bytes are produced; duplicating a bound here would create a second authority that could
/// disagree with the encoder.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PreparedTransaction {
    /// This transaction's version. Must be `current + 1`, and `1` for genesis.
    pub version: u64,
    /// The previous version; `None` exactly for genesis.
    pub prev_version: Option<u64>,
    /// 16-byte transaction id. Must equal the `<tid>` component of the transaction file name.
    pub tx_id: [u8; 16],
    /// The KB this transaction belongs to.
    pub kb_uuid: KbUuid,
    /// Commit time, UTC microseconds, written by the committer. A replay never re-reads a clock.
    pub created_at: i64,
    /// Executor of the request; genesis uses `system`.
    ///
    /// Opaque: never a token and never a permission claim. The format bound (120 bytes) is the
    /// only bound, and it is enforced by the encoder.
    pub principal_id: String,
    /// Caller-chosen request identifier: the request component of the idempotency key.
    ///
    /// The format bound is 128 bytes — deliberately wider than the principal bound, because an
    /// idempotency key component may legitimately be longer than a principal name.
    pub request_id: String,
    /// Digest of the caller's request frame. All-zero is the genesis sentinel and nothing else.
    pub payload_digest: [u8; 32],
    /// Whole-KB optimistic-concurrency expectation: when present it must equal `prev_version`.
    pub expect_version: Option<u64>,
    /// Advisory annotations for the transaction header, moved rather than copied at encode time.
    pub extensions: Extensions,
    /// Events in commit order. Event tags may repeat; `event_seq` is the index; after-images only.
    pub events: Vec<OwnedEvent>,
    /// Blobs to make durable: a superset of the references the format enumerates.
    pub blobs: Vec<BlobDeclaration>,
    /// The rules binding in force.
    pub rules: RulesBinding,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_version_codes_match_the_header_wire_values() {
        // The only value assertion kept: the encoder writes `code()` directly, so this pins the
        // one definition of the numeric correspondence. Derivation and the version/sentinel join
        // are proved by the store conversion against encode output, not by mirrors here.
        assert_eq!(RequestCanonVersion::NonRequest.code(), 0);
        assert_eq!(RequestCanonVersion::V1.code(), 1);
    }
}
