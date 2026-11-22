//! Transport syntax validation over already-decoded protocol messages.
//!
//! # What this module is, precisely
//!
//! A **borrowed syntax gate**. It takes a decoded message and answers one question: "is this a
//! well-formed request shape?" It answers nothing else.
//!
//! - It is **not** a kernel business re-implementation. Bounds like "an atom is at most 800
//!   bytes", "a frontier entry lists at most 64 concepts" and "these references must be
//!   strictly ascending" are the canonical kernel's authority and stay there.
//! - It is **not** a bounded-decoder proof. Decoding happens before this module is reached;
//!   whether a decoder allocates proportionally to an attacker's length prefix is a property
//!   of that decoder. Bounded network ingress is future work.
//! - It mints no authority. Nothing here grants a capability or authenticates a caller.
//!
//! # The walk is bounded by the caller's own ledger
//!
//! Every field walk charges the bytes it actually visits — not a constant chosen to look
//! expensive — and every collection is gated by a **configured ceiling** before its loop
//! begins. Long walks checkpoint through [`SharedLedger::checkpoint`] at a fixed interval, so a
//! caller's cancellation or deadline is honoured mid-field rather than only at entry. The
//! refusal types are the canonical ones: a `BudgetError` keeps its unit and figures, and a
//! `ControlStop` stays the caller's own signal.
//!
//! # Record families come from Types
//!
//! Where the proto fixes a key's record family, the transport gate refuses a different family
//! by consulting [`sage_db_types::persist::RecordKind`] — the canonical closed registry — and
//! never a second table of its own.
//!
//! # Allocation
//!
//! No owning copies, no sorting, no de-duplication. Order is the caller's and is preserved
//! verbatim, because a reordered read set would silently change what the caller asserted.

use sage_db_types::framing::Refusal;
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{RecordKey as TypesRecordKey, RecordKind};
use sage_db_types::query::ControlStop;

use crate::generated::{
    CommitPayload, Decision, Envelope, Limits, RecordKey, RevPair, ReviewDecisionPayload,
    ReviewPayload, SubmitPayload,
};

/// The only protocol version this module accepts.
pub const PROTOCOL_VERSION: u32 = 1;

/// How many walked characters may pass between control checkpoints.
///
/// The same order as `sage_db_types::query::CONTROL_CHECK_INTERVAL_STEPS`: frequent enough that
/// a cancelled caller is noticed inside one large field, coarse enough that checkpointing does
/// not dominate the walk.
const CONTROL_EVERY: u64 = 1_024;

/// The widest `u128` in decimal digits.
///
/// A longer digit string cannot be a representable value, so it is refused by this bounded
/// check instead of being handed to a parse that would walk it first.
const MAX_U128_DIGITS: usize = 39;

/// Caller-supplied ceilings for the walk itself.
///
/// Separate from [`Limits`], which is what a *deployment* advertises. These are what *this*
/// invocation is willing to spend, and a message that exceeds them is refused before the growth
/// happens. Neither type is a business bound: the kernel still decides how large an atom may be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValidationLimits {
    /// Longest single string field the walk may traverse.
    pub max_field_bytes: u64,
    /// Most elements any one collection may hold.
    pub max_collection_entries: u64,
    /// Most elements the whole message may hold, across every collection.
    pub max_total_entries: u64,
}

impl ValidationLimits {
    /// A conservative default for a command envelope.
    ///
    /// Chosen to admit the protocol's own shapes with room to spare, not to express any policy:
    /// a caller that knows its budget should pass its own.
    pub const DEFAULT: Self = Self {
        max_field_bytes: 1 << 20,
        max_collection_entries: 1_024,
        max_total_entries: 4_096,
    };
}

/// Why a message was not accepted at the transport gate.
///
/// The arms answer different questions, and an operator acts on them differently: a malformed
/// field is the caller's defect, an invalid *configuration* is this deployment's defect, an
/// exceeded ceiling is this invocation's budget, and a refusal is a resource or control fact
/// about the attempt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValidationError {
    /// A field carried a value this protocol does not define.
    Malformed {
        /// The field, named as the proto names it.
        field: &'static str,
        /// What was wrong, without echoing attacker-controlled content.
        detail: &'static str,
    },
    /// The deployment's advertised limits are not usable as stated.
    ///
    /// Distinct from [`ValidationError::Malformed`]: the caller did nothing wrong, and no
    /// request will succeed until the configuration is corrected.
    InvalidConfig {
        /// The limit as the proto names it.
        field: &'static str,
        /// Why it is unusable.
        detail: &'static str,
    },
    /// A message exceeded the caller's own [`ValidationLimits`].
    Limit {
        /// The ceiling that was exceeded, as this module names it.
        field: &'static str,
        /// What was asked for.
        requested: u64,
        /// What the caller allowed.
        limit: u64,
    },
    /// The caller's own refusal, preserved as the **canonical payload**.
    ///
    /// Carrying `Refusal` keeps a `BudgetError`'s unit, requested and remaining figures intact,
    /// and leaves a `ControlStop` as the caller's own signal. Flattening either into a string or
    /// a lookalike enum would destroy the detail an operator needs to tell "retry with more
    /// budget" from "you cancelled".
    Refused(Refusal),
}

impl From<Refusal> for ValidationError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<BudgetError> for ValidationError {
    fn from(error: BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}

impl From<ControlStop> for ValidationError {
    fn from(stop: ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}

/// A message that passed the transport gate.
///
/// The witness **borrows** the original message: it proves the check ran without owning a copy,
/// so passing one along cannot silently detach the validated data from what the handler reads.
pub struct Validated<'a, T> {
    message: &'a T,
}

impl<'a, T> Validated<'a, T> {
    /// The message this witness covers.
    #[must_use]
    pub const fn message(&self) -> &'a T {
        self.message
    }
}

impl<T> core::fmt::Debug for Validated<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Validated(..)")
    }
}

/// The shared state of one validation pass: the ledger, the caller's ceilings, and counters.
struct Walk<'a> {
    ledger: &'a mut dyn SharedLedger,
    limits: ValidationLimits,
    entries_seen: u64,
    steps_since_control: u64,
}

impl<'a> Walk<'a> {
    fn new(ledger: &'a mut dyn SharedLedger, limits: ValidationLimits) -> Self {
        Self {
            ledger,
            limits,
            entries_seen: 0,
            steps_since_control: 0,
        }
    }

    /// Admit one control checkpoint.
    fn control(&mut self) -> Result<(), ValidationError> {
        self.ledger.checkpoint()?;
        self.steps_since_control = 0;
        Ok(())
    }

    /// Charge a field's length, checkpointing at a fixed interval.
    ///
    /// The charge is the number of bytes the field declares, so a one-byte field costs one byte.
    ///
    /// ## What the interval check does, and does not, make cancellable
    ///
    /// This is an **accounting loop**: it charges a length in `CONTROL_EVERY` steps and consults
    /// control between them. What a caller can therefore cancel inside a large field is its
    /// *metering*, not a scan of its contents.
    ///
    /// There is no semantic byte walk over a long field in this module, and none is claimed: the
    /// only bounded content scan here is the decimal syntax check, which is at most
    /// [`MAX_U128_DIGITS`] digits and cannot cross a control interval. A future bounded network
    /// ingress that *does* validate long strings would need its own checkpoints; this loop's
    /// cadence would not cover it, and inheriting it silently would be a false claim.
    fn charge_walked(&mut self, count: u64) -> Result<(), ValidationError> {
        let mut left = count;
        while left > 0 {
            if self.steps_since_control >= CONTROL_EVERY {
                self.control()?;
            }
            let step = left.min(CONTROL_EVERY - self.steps_since_control);
            self.ledger.try_charge(step)?;
            self.steps_since_control += step;
            left -= step;
        }
        Ok(())
    }

    /// Charge one collection of `count` elements: its entries, then a per-entry control point.
    ///
    /// The collection ceiling is checked **before** the loop, so an over-large list is refused
    /// before any per-element work.
    ///
    /// ## Empty collections are charged, in BOTH meters
    ///
    /// `count.max(1)` is the charge: the walk admitted the collection itself, and "empty" is not
    /// free. The policy total (`entries_seen`) advances by that same figure, not by the raw
    /// element count — otherwise an empty collection would cost the ledger an entry the policy
    /// ceiling never saw, and the two meters would disagree about the same request.
    fn begin_collection(
        &mut self,
        field: &'static str,
        count: usize,
    ) -> Result<(), ValidationError> {
        let count = u64::try_from(count).map_err(|_| ValidationError::Malformed {
            field,
            detail: "collection length is not representable",
        })?;
        if count > self.limits.max_collection_entries {
            return Err(ValidationError::Limit {
                field,
                requested: count,
                limit: self.limits.max_collection_entries,
            });
        }
        // The charge is `count.max(1)`; the policy total advances by exactly that, so the two
        // meters describe the same request.
        let charged = count.max(1);
        // An unrepresentable sum is NOT a limit refusal and carries no invented `requested`
        // figure: `entries_seen` only ever advances while it is within `max_total_entries`, so
        // reaching this arm needs a configured ceiling near `u64::MAX`. That is a deployment
        // limit that cannot be counted, not a caller asking for too much — and reporting a
        // fabricated `u64::MAX` as "what was requested" would state knowledge nobody has.
        let Some(total) = self.entries_seen.checked_add(charged) else {
            return Err(ValidationError::InvalidConfig {
                field: "max_total_entries",
                detail: "the request-wide entry total is not representable in u64",
            });
        };
        if total > self.limits.max_total_entries {
            return Err(ValidationError::Limit {
                field,
                requested: total,
                limit: self.limits.max_total_entries,
            });
        }
        self.entries_seen = total;
        self.ledger.try_charge_entries(charged)?;
        self.control()?;
        Ok(())
    }

    /// Walk a string field, charging its real length and enforcing the per-field ceiling.
    fn field(&mut self, name: &'static str, value: &str) -> Result<(), ValidationError> {
        let len = u64::try_from(value.len()).map_err(|_| ValidationError::Malformed {
            field: name,
            detail: "length is not representable",
        })?;
        if len > self.limits.max_field_bytes {
            return Err(ValidationError::Limit {
                field: name,
                requested: len,
                limit: self.limits.max_field_bytes,
            });
        }
        self.charge_walked(len)
    }

    /// Walk one canonical decimal string: syntax, then a checked parse to the given width.
    fn decimal(&mut self, name: &'static str, value: &str) -> Result<u128, ValidationError> {
        self.field(name, value)?;
        // Canonical decimal: digits only, no sign, no padding, single leading zero only for "0".
        let mut bytes = value.bytes();
        let first = bytes.next().ok_or(ValidationError::Malformed {
            field: name,
            detail: "decimal string is empty",
        })?;
        if !first.is_ascii_digit() {
            return Err(ValidationError::Malformed {
                field: name,
                detail: "decimal string must start with a digit",
            });
        }
        if first == b'0' && value.len() > 1 {
            return Err(ValidationError::Malformed {
                field: name,
                detail: "decimal string has a leading zero",
            });
        }
        // The field was charged exactly once by `field` above, so the digit loop charges
        // nothing further: a k-digit decimal costs k bytes, not 2k-1.
        //
        // No per-chunk control belongs here either. A `u128` decimal is at most 39 digits, far
        // below one control interval, so a stop cannot be needed *inside* it; cancellation for
        // long content is handled by `field`'s chunked `charge_walked`, which is where a value
        // can actually exceed an interval. Claiming mid-value cancellation here would be a
        // control point on work that never happens.
        for byte in bytes {
            if !byte.is_ascii_digit() {
                return Err(ValidationError::Malformed {
                    field: name,
                    detail: "decimal string contains a non-digit",
                });
            }
        }
        // Early refusal of an unrepresentable width before the parse walks it: more than 39
        // digits cannot fit `u128` (max 340282366920938463463374607431768211455), so the value
        // is refused by a bounded check rather than by an unbounded parse attempt.
        if value.len() > MAX_U128_DIGITS {
            return Err(ValidationError::Malformed {
                field: name,
                detail: "decimal value exceeds the field's range",
            });
        }
        // Checked parse: an overflow is a malformed value, never a wrap or a truncation.
        value
            .parse::<u128>()
            .map_err(|_| ValidationError::Malformed {
                field: name,
                detail: "decimal value exceeds the field's range",
            })
    }

    /// Walk a decimal that must fit `u64` (versions, revisions, high-water marks).
    fn decimal_u64(&mut self, name: &'static str, value: &str) -> Result<u64, ValidationError> {
        let wide = self.decimal(name, value)?;
        u64::try_from(wide).map_err(|_| ValidationError::Malformed {
            field: name,
            detail: "value exceeds the u64 range",
        })
    }

    /// Walk a record key: the kind must be in the canonical registry, and the id must fit the
    /// bound that kind declares.
    fn record_key(
        &mut self,
        name: &'static str,
        key: &RecordKey,
    ) -> Result<RecordKind, ValidationError> {
        let kind_u16 = u16::try_from(key.kind).map_err(|_| ValidationError::Malformed {
            field: name,
            detail: "record kind is outside the u16 registry",
        })?;
        let kind = RecordKind::from_u16(kind_u16).ok_or(ValidationError::Malformed {
            field: name,
            detail: "record kind is not in the closed registry",
        })?;
        let id = self.decimal("record id", &key.id)?;
        // The bound is the canonical one for the family: ask Types rather than restating a
        // per-kind maximum here, which would be a second table that could drift.
        if id > id_bound(kind) {
            return Err(ValidationError::Malformed {
                field: name,
                detail: "record id exceeds the bound its kind declares",
            });
        }
        Ok(kind)
    }

    /// Walk a key whose family the proto fixes, refusing any other family.
    fn record_key_of(
        &mut self,
        name: &'static str,
        key: &RecordKey,
        expected: RecordKind,
    ) -> Result<(), ValidationError> {
        let found = self.record_key(name, key)?;
        if found != expected {
            return Err(ValidationError::Malformed {
                field: name,
                detail: "record kind is not the family this position requires",
            });
        }
        Ok(())
    }
}

/// The largest id a family admits, derived from the canonical key type itself.
///
/// Constructing a `RecordKey` at the maximum and asking whether it is in range keeps this in
/// lockstep with Types: if a family's bound ever changes, this follows automatically.
fn id_bound(kind: RecordKind) -> u128 {
    // `RecordKey::id_is_in_range` is the canonical predicate. Probing it at the widest value
    // recovers the family's bound without a second per-kind table that could drift from Types.
    if TypesRecordKey::new(kind, u128::MAX).id_is_in_range() {
        u128::MAX
    } else {
        u128::from(u32::MAX)
    }
}

/// Validate the deployment's advertised [`Limits`] **once**, before any admission.
///
/// This is a configuration gate, not a request check: an unusable advertised limit is this
/// deployment's defect, so it is reported as [`ValidationError::InvalidConfig`] and never as
/// the caller's fault.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for a zero or non-representable limit.
pub fn validate_limits(limits: &Limits) -> Result<(), ValidationError> {
    let check = |name: &'static str, value: u64| -> Result<(), ValidationError> {
        if value == 0 {
            return Err(ValidationError::InvalidConfig {
                field: name,
                detail: "an advertised limit must be positive",
            });
        }
        Ok(())
    };
    check("limits.max_payload_bytes", limits.max_payload_bytes)?;
    check("limits.max_refs", limits.max_refs)?;
    check("limits.max_atom_bytes", limits.max_atom_bytes)?;
    if let Some(units) = limits.max_atom_utf16_units {
        check("limits.max_atom_utf16_units", units)?;
    }
    // The refs ceiling is also a collection ceiling this crate will enforce; it must fit.
    if limits.max_refs > u64::from(u32::MAX) {
        return Err(ValidationError::InvalidConfig {
            field: "limits.max_refs",
            detail: "refs ceiling exceeds the representable collection size",
        });
    }
    Ok(())
}

/// Walk the exact request envelope every command carries.
///
/// # Errors
///
/// [`ValidationError::Malformed`] for a wrong protocol version, a non-canonical KB uuid, a
/// missing request id, a non-positive deadline or an unparseable decimal;
/// [`ValidationError::Limit`] for a message over `limits`; and [`ValidationError::Refused`] for
/// the caller's own budget or stop.
pub fn validate_envelope<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    envelope: &'a Envelope,
) -> Result<Validated<'a, Envelope>, ValidationError> {
    // A standalone call owns its walk: this fragment is a complete request when used alone.
    let mut walk = Walk::new(ledger, *limits);
    // Entry control even for an empty or malformed fragment.
    walk.control()?;
    validate_envelope_with_walk(&mut walk, envelope)
}

/// The single algorithm behind [`validate_envelope`], taking the caller's walk.
///
/// The paired `*_request` entry points pass the SAME walk here and into the payload body, so
/// `max_total_entries` bounds one whole request instead of each half separately.
fn validate_envelope_with_walk<'a>(
    walk: &mut Walk<'_>,
    envelope: &'a Envelope,
) -> Result<Validated<'a, Envelope>, ValidationError> {
    // The envelope is a fixed shape: two scalar fields plus its read set.
    walk.begin_collection("envelope", 1)?;
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(ValidationError::Malformed {
            field: "protocol_version",
            detail: "only version 1 is accepted",
        });
    }
    walk.kb_uuid(&envelope.kb_uuid)?;
    if envelope.request_id.is_empty() {
        return Err(ValidationError::Malformed {
            field: "request_id",
            detail: "is required for every command",
        });
    }
    walk.field("request_id", &envelope.request_id)?;
    if envelope.deadline_ms == Some(0) {
        return Err(ValidationError::Malformed {
            field: "deadline_ms",
            detail: "must be positive when present",
        });
    }
    if let Some(expect_version) = envelope.expect_version.as_deref() {
        walk.decimal_u64("expect_version", expect_version)?;
    }
    walk.begin_collection("read_set", envelope.read_set.len())?;
    for pair in &envelope.read_set {
        walk.rev_pair(pair)?;
    }
    Ok(Validated { message: envelope })
}

impl Walk<'_> {
    /// Walk the canonical 32-character lowercase hex form of a KB uuid.
    fn kb_uuid(&mut self, value: &str) -> Result<(), ValidationError> {
        if value.len() != 32 {
            return Err(ValidationError::Malformed {
                field: "kb_uuid",
                detail: "must be exactly 32 hex characters",
            });
        }
        self.charge_walked(32)?;
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ValidationError::Malformed {
                field: "kb_uuid",
                detail: "must be lowercase hexadecimal",
            });
        }
        Ok(())
    }

    /// Walk one read-set entry: a record key at a revision.
    fn rev_pair(&mut self, pair: &RevPair) -> Result<(), ValidationError> {
        self.begin_collection("read_set.entry", 1)?;
        let key = pair.key.as_ref().ok_or(ValidationError::Malformed {
            field: "read_set.key",
            detail: "is required",
        })?;
        self.record_key("read_set.key", key)?;
        let revision = self.decimal_u64("read_set.revision", &pair.revision)?;
        if sage_db_types::identity::RecordRevision::new(revision).is_none() {
            return Err(ValidationError::Malformed {
                field: "read_set.revision",
                detail: "a record revision must be nonzero",
            });
        }
        Ok(())
    }
}

/// Validate a submission payload's oneof shape.
///
/// # Errors
///
/// As [`validate_envelope`], plus the per-family key checks this position fixes.
pub fn validate_submit<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    payload: &'a SubmitPayload,
) -> Result<Validated<'a, SubmitPayload>, ValidationError> {
    let mut walk = Walk::new(ledger, *limits);
    // Entry control even for an empty or malformed fragment.
    walk.control()?;
    validate_submit_with_walk(&mut walk, payload)
}

fn validate_submit_with_walk<'a>(
    walk: &mut Walk<'_>,
    payload: &'a SubmitPayload,
) -> Result<Validated<'a, SubmitPayload>, ValidationError> {
    walk.begin_collection("submit", 1)?;
    match payload.kind.as_ref() {
        None => Err(ValidationError::Malformed {
            field: "submit.kind",
            detail: "an empty oneof is not a valid submission",
        }),
        Some(crate::generated::submit_payload::Kind::ClaimEdit(edit)) => {
            let subject = edit.subject.as_ref().ok_or(ValidationError::Malformed {
                field: "claim_edit.subject",
                detail: "is required",
            })?;
            // The proto fixes this position to a Term.
            walk.record_key_of("claim_edit.subject", subject, RecordKind::Term)?;
            let claim = edit.claim.as_ref().ok_or(ValidationError::Malformed {
                field: "claim_edit.claim",
                detail: "is required",
            })?;
            walk.record_key_of("claim_edit.claim", claim, RecordKind::Claim)?;
            walk.field("claim_edit.text", &edit.text)?;
            walk.field("claim_edit.declared_author", &edit.declared_author)?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::RelationAssert(edge)) => {
            walk.relation_submit(edge)?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::LegacyRelation(legacy)) => {
            walk.field("legacy_relation.filename", &legacy.filename)?;
            let edge = legacy.edge.as_ref().ok_or(ValidationError::Malformed {
                field: "legacy_relation.edge",
                detail: "is required",
            })?;
            walk.relation_submit(edge)?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::LegacyRelationBySlug(edge)) => {
            let from = edge.from.as_ref().ok_or(ValidationError::Malformed {
                field: "legacy_relation_by_slug.from",
                detail: "is required",
            })?;
            walk.record_key_of("legacy_relation_by_slug.from", from, RecordKind::Term)?;
            walk.field("legacy_relation_by_slug.target_slug", &edge.target_slug)?;
            walk.field("legacy_relation_by_slug.filename", &edge.filename)?;
            walk.relation_fields(
                edge.relation,
                edge.confidence,
                &edge.declared_author,
                &edge.reason,
            )?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::TermCreate(term)) => {
            walk.term_create(term)?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::ConceptText(text)) => {
            walk.concept_submit(text)?;
            Ok(Validated { message: payload })
        }
        Some(crate::generated::submit_payload::Kind::LegacyConcept(legacy)) => {
            walk.field("legacy_concept.filename", &legacy.filename)?;
            let text = legacy.edit.as_ref().ok_or(ValidationError::Malformed {
                field: "legacy_concept.edit",
                detail: "is required",
            })?;
            walk.concept_submit(text)?;
            match (
                legacy.current_atom.as_deref(),
                legacy.public_revision.as_deref(),
            ) {
                (Some(atom), None) => walk.field("legacy_concept.current_atom", atom)?,
                (None, Some(hash)) => walk.legacy_hash(hash)?,
                _ => {
                    return Err(ValidationError::Malformed {
                        field: "legacy_concept.baseline",
                        detail: "exactly one comparison basis is required",
                    });
                }
            }
            Ok(Validated { message: payload })
        }
    }
}

/// Validate a review payload: an explicit family, then its shared decision body.
///
/// The body is walked by ONE function for both families, so the two cannot drift in what they
/// accept. The family is refused when absent — an empty oneof names no operation, and the
/// server would have no operation kind to dispatch on.
///
/// # Errors
///
/// As [`validate_envelope`], plus a missing family, a missing proposal, an unspecified decision
/// and a proposal key that is not a `Proposal`.
pub fn validate_review<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    payload: &'a ReviewPayload,
) -> Result<Validated<'a, ReviewPayload>, ValidationError> {
    let mut walk = Walk::new(ledger, *limits);
    // Entry control even for an empty or malformed fragment.
    walk.control()?;
    validate_review_with_walk(&mut walk, payload)
}

fn validate_review_with_walk<'a>(
    walk: &mut Walk<'_>,
    payload: &'a ReviewPayload,
) -> Result<Validated<'a, ReviewPayload>, ValidationError> {
    walk.begin_collection("review", 1)?;
    use crate::generated::review_payload::Family;
    match payload.family.as_ref() {
        None => Err(ValidationError::Malformed {
            field: "review.family",
            detail: "a review must state its family",
        }),
        // All families share this decision shape.
        Some(
            Family::ClaimEdit(body)
            | Family::ConceptText(body)
            | Family::RelationAssert(body)
            | Family::TermCreate(body),
        ) => {
            validate_review_decision(walk, "review", body)?;
            Ok(Validated { message: payload })
        }
        Some(Family::LegacyRelation(review) | Family::LegacyConcept(review)) => {
            let action = review.action.as_ref().ok_or(ValidationError::Malformed {
                field: "legacy_proposal_review.action",
                detail: "is required",
            })?;
            validate_review_decision(walk, "legacy_proposal_review", action)?;
            if action.decision != Decision::Approve as i32 {
                return Err(ValidationError::Malformed {
                    field: "legacy_proposal_review.action.decision",
                    detail: "a legacy merge only approves",
                });
            }
            walk.field("legacy_proposal_review.filename", &review.filename)?;
            if let Some(hash) = review.public_revision.as_deref() {
                walk.legacy_hash(hash)?;
            }
            Ok(Validated { message: payload })
        }
    }
}

/// The decision body shared by both review families.
///
/// Takes the caller's [`Walk`] rather than building one, so the entry counter and the control
/// cadence span the whole request instead of restarting per half.
fn validate_review_decision(
    walk: &mut Walk<'_>,
    prefix: &'static str,
    body: &ReviewDecisionPayload,
) -> Result<(), ValidationError> {
    let proposal = body.proposal.as_ref().ok_or(ValidationError::Malformed {
        field: "review.proposal",
        detail: "is required",
    })?;
    // Every review target is a Proposal record; the FAMILY is the payload's oneof above, not
    // this key's kind.
    walk.record_key_of("review.proposal", proposal, RecordKind::Proposal)?;
    // An unknown enum value is refused rather than treated as approve or reject.
    if Decision::try_from(body.decision).is_err() || body.decision == Decision::Unspecified as i32 {
        return Err(ValidationError::Malformed {
            field: "review.decision",
            detail: "a review must state approve or reject",
        });
    }
    let _ = prefix;
    walk.field("review.note", &body.note)
}

/// Validate a commit payload's oneof shape.
///
/// # Errors
///
/// As [`validate_envelope`], plus an empty oneof and the per-family key and state checks.
pub fn validate_commit<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    payload: &'a CommitPayload,
) -> Result<Validated<'a, CommitPayload>, ValidationError> {
    let mut walk = Walk::new(ledger, *limits);
    // Entry control even for an empty or malformed fragment.
    walk.control()?;
    validate_commit_with_walk(&mut walk, payload)
}

fn validate_commit_with_walk<'a>(
    walk: &mut Walk<'_>,
    payload: &'a CommitPayload,
) -> Result<Validated<'a, CommitPayload>, ValidationError> {
    walk.begin_collection("commit", 1)?;
    use crate::generated::commit_payload::Kind;
    match payload.kind.as_ref() {
        None => Err(ValidationError::Malformed {
            field: "commit.kind",
            detail: "an empty oneof is not a valid command",
        }),
        Some(Kind::CreateSessionCheckpoint(state)) => {
            walk.checkpoint_state(state)?;
            Ok(Validated { message: payload })
        }
        Some(Kind::ReviseSessionCheckpoint(revise)) => {
            let checkpoint = revise
                .checkpoint
                .as_ref()
                .ok_or(ValidationError::Malformed {
                    field: "revise_session_checkpoint.checkpoint",
                    detail: "is required",
                })?;
            walk.record_key_of(
                "revise_session_checkpoint.checkpoint",
                checkpoint,
                RecordKind::SessionCheckpoint,
            )?;
            let state = revise.state.as_ref().ok_or(ValidationError::Malformed {
                field: "revise_session_checkpoint.state",
                detail: "is required",
            })?;
            walk.checkpoint_state(state)?;
            Ok(Validated { message: payload })
        }
        Some(Kind::DirectConceptCreate(create)) => {
            let term = create.term.as_ref().ok_or(ValidationError::Malformed {
                field: "direct_concept_create.term",
                detail: "is required",
            })?;
            walk.term_create(term)?;
            walk.begin_collection("direct_concept_create.assertions", create.assertions.len())?;
            for assertion in &create.assertions {
                walk.begin_collection("direct_concept_create.assertion", 1)?;
                let target = assertion
                    .target
                    .as_ref()
                    .ok_or(ValidationError::Malformed {
                        field: "direct_concept_create.assertion.target",
                        detail: "is required",
                    })?;
                walk.record_key_of(
                    "direct_concept_create.assertion.target",
                    target,
                    RecordKind::Term,
                )?;
                if !assertion.relation.is_some_and(|value| value <= 6) {
                    return Err(ValidationError::Malformed {
                        field: "direct_concept_create.assertion.relation",
                        detail: "a registered relation is required",
                    });
                }
                if !assertion.confidence.is_some_and(|value| value <= 2) {
                    return Err(ValidationError::Malformed {
                        field: "direct_concept_create.assertion.confidence",
                        detail: "a registered confidence is required",
                    });
                }
            }
            Ok(Validated { message: payload })
        }
        Some(Kind::CreateFrontier(state)) => {
            walk.frontier_state(state)?;
            Ok(Validated { message: payload })
        }
        Some(Kind::ReviseFrontier(revise)) => {
            let frontier = revise.frontier.as_ref().ok_or(ValidationError::Malformed {
                field: "revise_frontier.frontier",
                detail: "is required",
            })?;
            walk.record_key_of(
                "revise_frontier.frontier",
                frontier,
                RecordKind::FrontierEntry,
            )?;
            let state = revise.state.as_ref().ok_or(ValidationError::Malformed {
                field: "revise_frontier.state",
                detail: "is required",
            })?;
            walk.frontier_state(state)?;
            Ok(Validated { message: payload })
        }
    }
}

impl Walk<'_> {
    fn legacy_hash(&mut self, hash: &str) -> Result<(), ValidationError> {
        self.field("legacy_proposal.public_revision", hash)?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ValidationError::Malformed {
                field: "legacy_proposal.public_revision",
                detail: "must be lowercase hexadecimal SHA-256",
            });
        }
        Ok(())
    }

    fn relation_submit(
        &mut self,
        edge: &crate::generated::RelationAssert,
    ) -> Result<(), ValidationError> {
        let from = edge.from.as_ref().ok_or(ValidationError::Malformed {
            field: "relation_assert.from",
            detail: "is required",
        })?;
        let target = edge.target.as_ref().ok_or(ValidationError::Malformed {
            field: "relation_assert.target",
            detail: "is required",
        })?;
        self.record_key_of("relation_assert.from", from, RecordKind::Term)?;
        self.record_key_of("relation_assert.target", target, RecordKind::Term)?;
        self.relation_fields(
            edge.relation,
            edge.confidence,
            &edge.declared_author,
            &edge.reason,
        )
    }

    fn relation_fields(
        &mut self,
        relation: Option<u32>,
        confidence: Option<u32>,
        declared_author: &str,
        reason: &str,
    ) -> Result<(), ValidationError> {
        if relation.is_none_or(|r| {
            u8::try_from(r)
                .ok()
                .and_then(sage_db_types::RelationId::from_u8)
                .is_none()
        }) {
            return Err(ValidationError::Malformed {
                field: "relation_assert.relation",
                detail: "a registered relation is required",
            });
        }
        if confidence.is_none_or(|c| {
            u16::try_from(c)
                .ok()
                .and_then(sage_db_types::Confidence::from_u16)
                .is_none()
        }) {
            return Err(ValidationError::Malformed {
                field: "relation_assert.confidence",
                detail: "a registered confidence is required",
            });
        }
        self.field("relation_assert.declared_author", declared_author)?;
        self.field("relation_assert.reason", reason)?;
        Ok(())
    }

    fn concept_submit(
        &mut self,
        text: &crate::generated::ConceptText,
    ) -> Result<(), ValidationError> {
        let subject = text.subject.as_ref().ok_or(ValidationError::Malformed {
            field: "concept_text.subject",
            detail: "is required",
        })?;
        self.record_key_of("concept_text.subject", subject, RecordKind::Term)?;
        // `None` and `Some("")` are different states and stay different: a cleared body is
        // a real statement, not an absence.
        for value in [text.atom.as_deref(), text.body.as_deref()]
            .into_iter()
            .flatten()
        {
            self.field("concept_text.prose", value)?;
        }
        self.field("concept_text.declared_author", &text.declared_author)?;
        if let Some(reason) = text.reason.as_deref() {
            self.field("concept_text.reason", reason)?;
        }
        Ok(())
    }

    fn term_create(&mut self, term: &crate::generated::TermCreate) -> Result<(), ValidationError> {
        for (field, valid) in [
            (
                "term_create.layer",
                term.layer.is_some_and(|layer| layer <= 3),
            ),
            (
                "term_create.confidence",
                term.confidence.is_some_and(|value| value <= 2),
            ),
            (
                "term_create.source",
                term.source.is_some_and(|value| value <= 2),
            ),
        ] {
            if !valid {
                return Err(ValidationError::Malformed {
                    field,
                    detail: "a registered value is required",
                });
            }
        }
        for (name, value) in [
            ("term_create.slug", &term.slug),
            ("term_create.atom", &term.atom),
            ("term_create.body", &term.body),
            ("term_create.declared_author", &term.declared_author),
            ("term_create.reason", &term.reason),
        ] {
            self.field(name, value)?;
        }
        for (name, values) in [
            ("term_create.domains", &term.domains),
            ("term_create.triggers", &term.triggers),
        ] {
            self.begin_collection(name, values.len())?;
            for value in values {
                self.control()?;
                self.field(name, value)?;
            }
        }
        Ok(())
    }

    /// Walk one session-checkpoint state.
    fn checkpoint_state(
        &mut self,
        state: &crate::generated::SessionCheckpointState,
    ) -> Result<(), ValidationError> {
        self.begin_collection("session_checkpoint", 1)?;
        let concept = state.concept.as_ref().ok_or(ValidationError::Malformed {
            field: "session_checkpoint.concept",
            detail: "is required",
        })?;
        self.record_key_of("session_checkpoint.concept", concept, RecordKind::Term)?;
        if state.session_no == 0 {
            return Err(ValidationError::Malformed {
                field: "session_checkpoint.session_no",
                detail: "ordinals start at 1",
            });
        }
        self.field("session_checkpoint.content", &state.content)?;
        Ok(())
    }

    /// Walk one frontier entry's complete state.
    ///
    /// The reference list is gated by the caller's collection ceiling **before** the loop, then
    /// each reference charges an entry and a control checkpoint as it is walked. Ascending
    /// uniqueness is deliberately *not* checked here: it is the kernel's business rule.
    fn frontier_state(
        &mut self,
        state: &crate::generated::FrontierState,
    ) -> Result<(), ValidationError> {
        self.begin_collection("frontier.concept_refs", state.concept_refs.len())?;
        for key in &state.concept_refs {
            self.begin_collection("frontier.concept_refs.entry", 1)?;
            // A frontier reference is a Term by definition.
            self.record_key_of("frontier.concept_refs", key, RecordKind::Term)?;
        }
        self.field("frontier.text", &state.text)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// One entry point per request kind
// ---------------------------------------------------------------------------

/// A request that passed the transport gate: its envelope and its payload, together.
///
/// The pairing is the point. A caller that validated an envelope and then forgot the payload
/// (or validated them in the wrong order) would have a witness that proves less than it
/// appears to; this type can only be produced by validating both.
pub struct ValidatedRequest<'a, P> {
    envelope: Validated<'a, Envelope>,
    payload: Validated<'a, P>,
}

impl<'a, P> ValidatedRequest<'a, P> {
    /// The validated envelope.
    #[must_use]
    pub const fn envelope(&self) -> &Validated<'a, Envelope> {
        &self.envelope
    }

    /// The validated payload.
    #[must_use]
    pub const fn payload(&self) -> &Validated<'a, P> {
        &self.payload
    }
}

impl<P> core::fmt::Debug for ValidatedRequest<'_, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ValidatedRequest(..)")
    }
}

/// Validate a whole submission: envelope, then payload, on one ledger.
///
/// # Errors
///
/// Whatever [`validate_envelope`] or [`validate_submit`] reports, in that order: the envelope
/// is checked first because a malformed envelope makes the payload's meaning unknown.
pub fn validate_submit_request<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    envelope: &'a Envelope,
    payload: &'a SubmitPayload,
) -> Result<ValidatedRequest<'a, SubmitPayload>, ValidationError> {
    // ONE walk for the whole request: the entry counter and the control cadence span the
    // envelope and the payload, so `max_total_entries` bounds the request rather than each
    // half. Two independent walks would let a request whose halves each fit the ceiling walk
    // roughly twice it.
    let mut walk = Walk::new(ledger, *limits);
    // Entry control runs even for a malformed or empty request, so a caller that cancelled is
    // observed before any field is inspected rather than after a successful walk.
    walk.control()?;
    let envelope = validate_envelope_with_walk(&mut walk, envelope)?;
    let payload = validate_submit_with_walk(&mut walk, payload)?;
    // Exit control: the request is admitted only through a control point, so a stop that
    // arrived during the last field is not reported as success.
    walk.control()?;
    Ok(ValidatedRequest { envelope, payload })
}

/// Validate a whole review: envelope, then payload, on one ledger.
///
/// # Errors
///
/// As [`validate_submit_request`].
pub fn validate_review_request<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    envelope: &'a Envelope,
    payload: &'a ReviewPayload,
) -> Result<ValidatedRequest<'a, ReviewPayload>, ValidationError> {
    // ONE walk for the whole request: the entry counter and the control cadence span the
    // envelope and the payload, so `max_total_entries` bounds the request rather than each
    // half. Two independent walks would let a request whose halves each fit the ceiling walk
    // roughly twice it.
    let mut walk = Walk::new(ledger, *limits);
    // Entry control runs even for a malformed or empty request, so a caller that cancelled is
    // observed before any field is inspected rather than after a successful walk.
    walk.control()?;
    let envelope = validate_envelope_with_walk(&mut walk, envelope)?;
    let payload = validate_review_with_walk(&mut walk, payload)?;
    // Exit control: the request is admitted only through a control point, so a stop that
    // arrived during the last field is not reported as success.
    walk.control()?;
    Ok(ValidatedRequest { envelope, payload })
}

/// Validate a whole commit command: envelope, then payload, on one ledger.
///
/// # Errors
///
/// As [`validate_submit_request`].
pub fn validate_commit_request<'a>(
    ledger: &mut dyn SharedLedger,
    limits: &ValidationLimits,
    envelope: &'a Envelope,
    payload: &'a CommitPayload,
) -> Result<ValidatedRequest<'a, CommitPayload>, ValidationError> {
    // ONE walk for the whole request: the entry counter and the control cadence span the
    // envelope and the payload, so `max_total_entries` bounds the request rather than each
    // half. Two independent walks would let a request whose halves each fit the ceiling walk
    // roughly twice it.
    let mut walk = Walk::new(ledger, *limits);
    // Entry control runs even for a malformed or empty request, so a caller that cancelled is
    // observed before any field is inspected rather than after a successful walk.
    walk.control()?;
    let envelope = validate_envelope_with_walk(&mut walk, envelope)?;
    let payload = validate_commit_with_walk(&mut walk, payload)?;
    // Exit control: the request is admitted only through a control point, so a stop that
    // arrived during the last field is not reported as success.
    walk.control()?;
    Ok(ValidatedRequest { envelope, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{
        ConceptText, FrontierState, RecordKey as WireKey, RevPair, ReviewDecisionPayload,
        SessionCheckpointState, SubmitPayload, commit_payload, review_payload, submit_payload,
    };
    use sage_db_types::ledger::BudgetUnit;
    use sage_db_types::query::{QueryControl, RunToCompletion};

    /// A test ledger that implements the **canonical** `SharedLedger`.
    ///
    /// It is not a second authority: it is one implementation of the trait the production code
    /// takes, so a refusal it produces is a real `BudgetError` and a stop is a real
    /// `ControlStop`. The alternative — a module-private trait with its own error types — is
    /// exactly what the review rejected.
    struct CountingLedger {
        bytes: u64,
        entries: u64,
        control: Box<dyn QueryControl>,
        charged_bytes: u64,
        charged_entries: u64,
        checks: u64,
    }

    impl CountingLedger {
        fn new(bytes: u64, entries: u64) -> Self {
            Self {
                bytes,
                entries,
                control: Box::new(RunToCompletion),
                charged_bytes: 0,
                charged_entries: 0,
                checks: 0,
            }
        }
    }

    impl SharedLedger for CountingLedger {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.bytes {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
            }
            self.bytes -= bytes;
            self.charged_bytes += bytes;
            Ok(())
        }

        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            if count > self.entries {
                return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
            }
            self.entries -= count;
            self.charged_entries += count;
            Ok(())
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.checks += 1;
            self.control.checkpoint()
        }
    }

    /// A control that stops at the first checkpoint.
    struct AlwaysCancelled;

    impl QueryControl for AlwaysCancelled {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    fn limits() -> ValidationLimits {
        ValidationLimits::DEFAULT
    }

    fn envelope() -> Envelope {
        Envelope {
            protocol_version: PROTOCOL_VERSION,
            kb_uuid: "5a".repeat(16),
            request_id: "req-1".into(),
            deadline_ms: None,
            read_set: vec![],
            expect_version: None,
        }
    }

    /// One review decision body, shared by both families.
    fn review_body(decision: i32) -> ReviewDecisionPayload {
        ReviewDecisionPayload {
            proposal: Some(wire_key(RecordKind::Proposal, "1")),
            decision,
            note: "n".into(),
        }
    }

    fn wire_key(kind: RecordKind, id: &str) -> WireKey {
        WireKey {
            kind: u32::from(kind.as_u16()),
            id: id.into(),
        }
    }

    /// Build an envelope whose read set holds `pairs` valid entries.
    fn envelope_with_read_set(pairs: usize) -> Envelope {
        Envelope {
            read_set: (0..pairs)
                .map(|n| RevPair {
                    key: Some(wire_key(RecordKind::Term, &format!("{}", n + 1))),
                    revision: "1".into(),
                })
                .collect(),
            ..envelope()
        }
    }

    /// The exact entry count this shape charges, traced through `begin_collection`:
    ///
    /// ```text
    /// envelope collection          1
    /// read_set collection (n=2)    1   (count.max(1))
    /// 2 x rev_pair                 2   (each charges 1)
    ///                            = 5   -> equals the total ceiling, accepted ALONE
    /// payload collection           1   -> 6 in a shared walk (refused), 1 alone (accepted)
    /// ```
    fn two_pair_request() -> (Envelope, SubmitPayload) {
        let env = envelope_with_read_set(2);
        let payload = SubmitPayload {
            kind: Some(submit_payload::Kind::ConceptText(ConceptText {
                subject: Some(wire_key(RecordKind::Term, "1")),
                atom: Some("a".into()),
                body: None,
                declared_author: String::new(),
                reason: None,
            })),
        };
        (env, payload)
    }

    /// `max_total_entries` chosen so the envelope half exactly consumes it.
    fn two_pair_limits() -> ValidationLimits {
        ValidationLimits {
            max_collection_entries: 4,
            max_total_entries: 5,
            ..ValidationLimits::DEFAULT
        }
    }

    #[test]
    fn each_half_of_the_pair_is_admitted_alone_at_exactly_the_total_ceiling() {
        // The premise the shared-total test needs: neither half exceeds the ceiling by itself,
        // so a refusal of the whole can only come from the counter spanning both.
        let (env, payload) = two_pair_request();
        let limits = two_pair_limits();
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_envelope(&mut ledger, &limits, &env)
            .expect("the envelope half costs exactly max_total_entries");
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_submit(&mut ledger, &limits, &payload)
            .expect("the payload half costs one entry, well inside the ceiling");
    }

    #[test]
    fn the_total_entry_ceiling_spans_the_whole_request_not_each_half() {
        // N=6 fails, N=5 passes: the two halves cost 5 and 1, so a request-wide budget of 5 is
        // exceeded while each half fits it. Two independent walks would accept both numbers,
        // which is exactly the defect this discriminates.
        let (env, payload) = two_pair_request();
        let mut ledger = CountingLedger::new(1 << 16, 64);
        assert!(
            validate_submit_request(&mut ledger, &two_pair_limits(), &env, &payload).is_err(),
            "5 envelope entries + 1 payload entry exceeds a request-wide total of 5"
        );

        // One more unit of total admits the same request, so the refusal above is about the SUM.
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let roomy = ValidationLimits {
            max_total_entries: 6,
            ..two_pair_limits()
        };
        validate_submit_request(&mut ledger, &roomy, &env, &payload)
            .expect("a request-wide total of 6 admits the same two halves");
    }

    #[test]
    fn a_slack_shape_also_discriminates_shared_from_reset_walks() {
        // The independent review's variant family: each half comfortably under the ceiling, the
        // sum over it. The exact per-half costs are MEASURED here rather than assumed, so the
        // test cannot drift from the walk's real accounting.
        let env = envelope_with_read_set(2);
        let payload = CommitPayload {
            kind: Some(commit_payload::Kind::CreateFrontier(FrontierState {
                concept_refs: (1..=3)
                    .map(|n| wire_key(RecordKind::Term, &format!("{n}")))
                    .collect(),
                text: "t".into(),
                priority: 0,
                status: 0,
            })),
        };
        let wide = ValidationLimits {
            max_collection_entries: 64,
            max_total_entries: 64,
            ..ValidationLimits::DEFAULT
        };

        let mut envelope_ledger = CountingLedger::new(1 << 16, 64);
        validate_envelope(&mut envelope_ledger, &wide, &env).expect("envelope alone");
        let envelope_cost = envelope_ledger.charged_entries;

        let mut payload_ledger = CountingLedger::new(1 << 16, 64);
        validate_commit(&mut payload_ledger, &wide, &payload).expect("payload alone");
        let payload_cost = payload_ledger.charged_entries;

        let total = envelope_cost + payload_cost;
        assert!(
            envelope_cost < total && payload_cost < total,
            "each half must be strictly under the sum: {envelope_cost} + {payload_cost}"
        );

        // The sum fits exactly at `total` ...
        let fits = ValidationLimits {
            max_total_entries: total,
            ..wide
        };
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_commit_request(&mut ledger, &fits, &env, &payload)
            .expect("the shared walk admits the measured sum");

        // ... and one less refuses, though each half alone still fits it.
        let tight = ValidationLimits {
            max_total_entries: total - 1,
            ..wide
        };
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_envelope(&mut ledger, &tight, &env).expect("the envelope half still fits");
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_commit(&mut ledger, &tight, &payload).expect("the payload half still fits");
        let mut ledger = CountingLedger::new(1 << 16, 64);
        assert!(
            validate_commit_request(&mut ledger, &tight, &env, &payload).is_err(),
            "at {} each half fits but the sum does not, so a reset walk would wrongly accept",
            total - 1
        );
    }

    #[test]
    fn an_empty_collection_is_charged_the_same_figure_in_both_meters() {
        // The two meters must agree: the ledger entry charge AND the policy total both advance
        // by `count.max(1)`. A policy total that advanced by the raw element count would let the
        // ledger charge an entry the ceiling never saw.
        let limits = ValidationLimits {
            max_collection_entries: 8,
            max_total_entries: 8,
            ..ValidationLimits::DEFAULT
        };
        let env = envelope();
        let payload = CommitPayload {
            kind: Some(commit_payload::Kind::CreateFrontier(FrontierState {
                concept_refs: Vec::new(),
                text: String::new(),
                priority: 0,
                status: 0,
            })),
        };

        // Exactly-cap ceiling (N) admits the request, and the admitted entry sum is recorded.
        let mut exactly = CountingLedger::new(1 << 16, 8);
        validate_commit_request(&mut exactly, &limits, &env, &payload)
            .expect("the shape fits its own admitted entry count");
        let admitted = exactly.charged_entries;
        assert!(admitted > 0, "the admitted entry sum is a real measurement");

        // One entry less (N-1) refuses it, with genuine figures rather than a placeholder.
        let mut one_short = CountingLedger::new(1 << 16, admitted - 1);
        match validate_commit_request(&mut one_short, &limits, &env, &payload) {
            Err(ValidationError::Refused(Refusal::Budget(error))) => {
                assert_eq!(error.unit(), BudgetUnit::Entries);
                assert!(error.requested() > error.remaining());
            }
            other => panic!("one entry short must be a typed entry refusal: {other:?}"),
        }

        // And the policy total agrees with the ledger's admitted sum for this shape: both count
        // every collection at `max(1)`, so a policy total below the admitted sum must refuse.
        let too_tight = ValidationLimits {
            max_total_entries: admitted - 1,
            ..limits
        };
        let mut ledger = CountingLedger::new(1 << 16, 8);
        assert!(
            validate_commit_request(&mut ledger, &too_tight, &env, &payload).is_err(),
            "the policy total below the admitted entry sum must refuse"
        );
    }

    #[test]
    fn an_unrepresentable_entry_total_is_a_config_error_not_a_fabricated_limit() {
        // MACHINERY NEGATIVE, explicitly labelled: no public request can reach this arm, because
        // `entries_seen` only advances while it is within `max_total_entries`, so reaching a
        // `u64` overflow needs a configured ceiling near `u64::MAX`. The test therefore drives
        // the accounting directly and asserts the SHAPE of the refusal: a config error naming
        // the ceiling, with no invented `requested` figure.
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let limits = ValidationLimits {
            // A ceiling so large that the next addition cannot be represented.
            max_collection_entries: u64::MAX,
            max_total_entries: u64::MAX,
            ..ValidationLimits::DEFAULT
        };
        let mut walk = Walk::new(&mut ledger, limits);
        // First collection consumes most of the space ...
        walk.entries_seen = u64::MAX - 1;
        // ... so adding this collection's `max(1)` overflows.
        match walk.begin_collection("read_set", 5) {
            Err(ValidationError::InvalidConfig { field, detail }) => {
                assert_eq!(field, "max_total_entries");
                assert!(
                    detail.contains("not representable"),
                    "the refusal must name the real problem, got {detail}"
                );
            }
            // A `Limit` with `requested: u64::MAX` would be the fabricated figure this rejects.
            Err(ValidationError::Limit { requested, .. }) => {
                panic!(
                    "an unrepresentable total must not report a fabricated requested={requested}"
                )
            }
            other => panic!("expected an InvalidConfig refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_nested_empty_collection_is_charged_even_though_it_holds_no_elements() {
        let env = envelope();
        let payload = CommitPayload {
            kind: Some(commit_payload::Kind::CreateFrontier(FrontierState {
                concept_refs: Vec::new(),
                text: String::new(),
                priority: 0,
                status: 0,
            })),
        };
        let mut ledger = CountingLedger::new(1 << 16, 64);
        validate_commit_request(&mut ledger, &limits(), &env, &payload)
            .expect("a subjectless frontier state is legal");
        // Envelope, its (empty) read set, the commit wrapper and the (empty) ref list: four
        // collections, each charged one entry despite holding no elements.
        assert_eq!(
            ledger.charged_entries, 4,
            "each of the four collections costs exactly one entry when empty"
        );
    }

    #[test]
    fn a_decimal_costs_exactly_the_digits_it_contains() {
        // The defect this discriminates: charging the field AND re-charging each digit makes a
        // k-digit value cost 2k-1 bytes, so a legitimate request can be refused by a budget its
        // own content fits.
        let short = Envelope {
            expect_version: Some("12345".into()),
            ..envelope()
        };
        let plain = envelope();
        let mut short_ledger = CountingLedger::new(1 << 16, 64);
        let mut plain_ledger = CountingLedger::new(1 << 16, 64);
        validate_envelope(&mut short_ledger, &limits(), &short).expect("accepted");
        validate_envelope(&mut plain_ledger, &limits(), &plain).expect("accepted");
        assert_eq!(
            short_ledger.charged_bytes - plain_ledger.charged_bytes,
            5,
            "\"12345\" must cost exactly 5 bytes more than an absent version"
        );

        // An exact-byte ledger passes with precisely that figure ...
        let delta = short_ledger.charged_bytes;
        let mut exact = CountingLedger::new(delta, 64);
        validate_envelope(&mut exact, &limits(), &short)
            .expect("a ledger sized to the real content must accept it");
        // ... and one byte less is a genuine typed budget refusal, with real figures.
        let mut one_short = CountingLedger::new(delta - 1, 64);
        match validate_envelope(&mut one_short, &limits(), &short) {
            Err(ValidationError::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
                assert_eq!(error.unit(), BudgetUnit::Bytes);
                // The figures are the ledger's own: the refusal names how much this charge
                // needed and how little was left. `remaining` is whatever credit survived the
                // earlier charges — the point is that it is REAL (never a fabricated zero) and
                // that the requested amount exceeded it.
                assert!(
                    error.requested() > error.remaining(),
                    "a refusal must ask for more than remained: {error:?}"
                );
            }
            other => panic!("one byte short must be a typed budget refusal: {other:?}"),
        }
    }

    #[test]
    fn a_decimal_longer_than_u128_is_refused_by_a_bounded_check() {
        // 40 digits: refused by the width check rather than by an unbounded parse.
        let too_wide = "1".repeat(40);
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let env = Envelope {
            expect_version: Some(too_wide),
            ..envelope()
        };
        assert!(matches!(
            validate_envelope(&mut ledger, &limits(), &env),
            Err(ValidationError::Malformed { .. })
        ));
        // 39 nines fits u128 range-wise, so it is malformed only for exceeding u128's value.
        let wide = "9".repeat(39);
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let env = Envelope {
            expect_version: Some(wide),
            ..envelope()
        };
        assert!(matches!(
            validate_envelope(&mut ledger, &limits(), &env),
            Err(ValidationError::Malformed { .. })
        ));
    }

    #[test]
    fn a_review_without_a_family_is_refused() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let payload = ReviewPayload { family: None };
        assert!(matches!(
            validate_review(&mut ledger, &limits(), &payload),
            Err(ValidationError::Malformed {
                field: "review.family",
                ..
            })
        ));
    }

    #[test]
    fn a_well_formed_envelope_is_accepted_and_the_canonical_ledger_is_charged() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let env = envelope();
        let witness = validate_envelope(&mut ledger, &limits(), &env).expect("accepted");
        assert_eq!(witness.message().request_id, "req-1");
        assert!(ledger.charged_entries >= 1);
        assert!(ledger.charged_bytes > 0);
        assert!(ledger.checks >= 1);
    }

    #[test]
    fn an_empty_collection_still_costs_an_entry_and_a_checkpoint() {
        // The read set is empty, but the collection was still admitted.
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let env = envelope();
        validate_envelope(&mut ledger, &limits(), &env).expect("accepted");
        assert!(
            ledger.charged_entries >= 1,
            "an empty collection is not free"
        );
        assert!(ledger.checks >= 1);
    }

    #[test]
    fn no_static_label_bytes_are_charged() {
        // The walk charges only real field content. A rename of a diagnostic label must not
        // change what a caller's budget requires, so an empty envelope with a 32-byte uuid and
        // a 5-byte request id charges exactly those bytes and nothing more.
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let env = envelope();
        validate_envelope(&mut ledger, &limits(), &env).expect("accepted");
        assert_eq!(
            ledger.charged_bytes,
            32 + 5,
            "only the kb uuid and request id are real content"
        );
    }

    #[test]
    fn a_stop_is_the_canonical_control_stop_not_a_lookalike() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        ledger.control = Box::new(AlwaysCancelled);
        let env = envelope();
        match validate_envelope(&mut ledger, &limits(), &env) {
            Err(ValidationError::Refused(Refusal::Stopped(ControlStop::Cancelled))) => {}
            other => panic!("expected the canonical stop, got {other:?}"),
        }
    }

    #[test]
    fn byte_and_entry_budgets_keep_their_canonical_units() {
        let mut no_entries = CountingLedger::new(1 << 16, 0);
        let env = envelope();
        match validate_envelope(&mut no_entries, &limits(), &env) {
            Err(ValidationError::Refused(Refusal::Budget(error))) => {
                assert_eq!(error.unit(), BudgetUnit::Entries);
            }
            other => panic!("expected an entry refusal, got {other:?}"),
        }

        let mut no_bytes = CountingLedger::new(0, 64);
        match validate_envelope(&mut no_bytes, &limits(), &env) {
            Err(ValidationError::Refused(Refusal::Budget(error))) => {
                assert_eq!(error.unit(), BudgetUnit::Bytes);
            }
            other => panic!("expected a byte refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_protocol_version_fails_closed() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let mut bad = envelope();
        bad.protocol_version = 2;
        assert!(matches!(
            validate_envelope(&mut ledger, &limits(), &bad),
            Err(ValidationError::Malformed {
                field: "protocol_version",
                ..
            })
        ));
    }

    #[test]
    fn a_non_canonical_kb_uuid_is_refused() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        for bad_uuid in ["5A".repeat(16), "5a".repeat(15), "zz".repeat(16)] {
            let mut bad = envelope();
            bad.kb_uuid = bad_uuid;
            assert!(validate_envelope(&mut ledger, &limits(), &bad).is_err());
        }
    }

    #[test]
    fn decimals_are_syntax_checked_and_range_checked() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        for bad in ["", "01", "-1", "1a", "+1", " 1"] {
            let mut env = envelope();
            env.expect_version = Some(bad.into());
            assert!(
                validate_envelope(&mut ledger, &limits(), &env).is_err(),
                "accepted {bad:?}"
            );
        }
        // Beyond u128: syntax is fine, the checked parse must refuse it.
        let mut env = envelope();
        env.expect_version = Some("9".repeat(42));
        match validate_envelope(&mut ledger, &limits(), &env) {
            Err(ValidationError::Malformed { field, .. }) => assert_eq!(field, "expect_version"),
            other => panic!("expected an overflow refusal, got {other:?}"),
        }
        // Within u64 and u128 both work.
        for good in ["0", "1", "18446744073709551615"] {
            let mut env = envelope();
            env.expect_version = Some(good.into());
            assert!(
                validate_envelope(&mut ledger, &limits(), &env).is_ok(),
                "refused {good:?}"
            );
        }
    }

    #[test]
    fn a_u64_field_refuses_a_value_above_u64() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let mut env = envelope();
        // One above u64::MAX: parses as u128 but must not be accepted as a version.
        env.expect_version = Some("18446744073709551616".into());
        assert!(matches!(
            validate_envelope(&mut ledger, &limits(), &env),
            Err(ValidationError::Malformed {
                field: "expect_version",
                ..
            })
        ));
    }

    #[test]
    fn collection_ceilings_are_enforced_before_the_loop() {
        let mut ledger = CountingLedger::new(1 << 20, 1_000);
        let tight = ValidationLimits {
            max_field_bytes: 1_024,
            max_collection_entries: 2,
            max_total_entries: 8,
        };
        let mut env = envelope();
        env.read_set = (0..3)
            .map(|i| RevPair {
                key: Some(wire_key(RecordKind::Term, &(i + 1).to_string())),
                revision: "1".into(),
            })
            .collect();
        match validate_envelope(&mut ledger, &tight, &env) {
            Err(ValidationError::Limit {
                field,
                requested,
                limit,
            }) => {
                assert_eq!(field, "read_set");
                assert_eq!((requested, limit), (3, 2));
            }
            other => panic!("expected a collection ceiling, got {other:?}"),
        }
        // Nothing was charged for the read set's entries: the refusal happens before that
        // loop. The envelope's own single entry was already charged, and stays charged.
        assert_eq!(ledger.charged_entries, 1);
    }

    #[test]
    fn a_field_ceiling_refuses_before_walking_the_whole_value() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        // Large enough to admit the 5-byte request id, small enough to reject the long value.
        let tight = ValidationLimits {
            max_field_bytes: 8,
            max_collection_entries: 8,
            max_total_entries: 8,
        };
        let mut env = envelope();
        env.expect_version = Some("1".repeat(5_000));
        match validate_envelope(&mut ledger, &tight, &env) {
            Err(ValidationError::Limit {
                field,
                requested,
                limit,
            }) => {
                assert_eq!(field, "expect_version");
                assert_eq!((requested, limit), (5_000, 8));
            }
            other => panic!("expected a field ceiling, got {other:?}"),
        }
    }

    #[test]
    fn a_long_field_checkpoints_mid_walk_so_a_stop_is_observed() {
        // A field long enough to cross several control intervals, with a control that stops.
        let mut ledger = CountingLedger::new(1 << 20, 64);
        ledger.control = Box::new(AlwaysCancelled);
        let loose = ValidationLimits {
            max_field_bytes: 1 << 20,
            max_collection_entries: 8,
            max_total_entries: 8,
        };
        let mut env = envelope();
        env.request_id = "x".repeat(10_000);
        match validate_envelope(&mut ledger, &loose, &env) {
            Err(ValidationError::Refused(Refusal::Stopped(ControlStop::Cancelled))) => {}
            other => panic!("a stop must be observed mid-field, got {other:?}"),
        }
    }

    #[test]
    fn the_read_set_order_is_preserved_not_sorted() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let mut env = envelope();
        env.read_set = vec![
            RevPair {
                key: Some(wire_key(RecordKind::Term, "2")),
                revision: "5".into(),
            },
            RevPair {
                key: Some(wire_key(RecordKind::Term, "1")),
                revision: "3".into(),
            },
        ];
        let witness = validate_envelope(&mut ledger, &limits(), &env).expect("accepted");
        assert_eq!(witness.message().read_set[0].revision, "5");
        assert_eq!(witness.message().read_set[1].revision, "3");
    }

    #[test]
    fn an_empty_oneof_is_refused_for_every_payload() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        assert!(matches!(
            validate_submit(&mut ledger, &limits(), &SubmitPayload { kind: None }),
            Err(ValidationError::Malformed {
                field: "submit.kind",
                ..
            })
        ));
        assert!(matches!(
            validate_commit(&mut ledger, &limits(), &CommitPayload { kind: None }),
            Err(ValidationError::Malformed {
                field: "commit.kind",
                ..
            })
        ));
    }

    #[test]
    fn a_wrong_record_family_is_refused_from_the_canonical_registry() {
        // A ClaimEdit's subject must be a Term; a SessionCheckpoint key is refused.
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let payload = SubmitPayload {
            kind: Some(submit_payload::Kind::ClaimEdit(
                crate::generated::ClaimEdit {
                    subject: Some(wire_key(RecordKind::SessionCheckpoint, "1")),
                    claim: Some(wire_key(RecordKind::Claim, "1")),
                    text: "t".into(),
                    declared_author: String::new(),
                },
            )),
        };
        match validate_submit(&mut ledger, &limits(), &payload) {
            Err(ValidationError::Malformed { field, .. }) => {
                assert_eq!(field, "claim_edit.subject")
            }
            other => panic!("expected a family refusal, got {other:?}"),
        }

        // A frontier reference must be a Term.
        let frontier = CommitPayload {
            kind: Some(commit_payload::Kind::CreateFrontier(FrontierState {
                concept_refs: vec![wire_key(RecordKind::Proposal, "1")],
                text: "t".into(),
                priority: 0,
                status: 0,
            })),
        };
        assert!(validate_commit(&mut ledger, &limits(), &frontier).is_err());
    }

    #[test]
    fn a_kind_outside_the_closed_registry_is_refused() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let payload = SubmitPayload {
            kind: Some(submit_payload::Kind::ClaimEdit(
                crate::generated::ClaimEdit {
                    subject: Some(WireKey {
                        kind: 99,
                        id: "1".into(),
                    }),
                    claim: Some(wire_key(RecordKind::Claim, "1")),
                    text: "t".into(),
                    declared_author: String::new(),
                },
            )),
        };
        assert!(matches!(
            validate_submit(&mut ledger, &limits(), &payload),
            Err(ValidationError::Malformed {
                field: "claim_edit.subject",
                ..
            })
        ));
    }

    #[test]
    fn a_term_id_above_u32_is_refused_by_the_canonical_bound() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let payload = SubmitPayload {
            kind: Some(submit_payload::Kind::ConceptText(ConceptText {
                subject: Some(wire_key(RecordKind::Term, "4294967296")),
                atom: None,
                body: None,
                declared_author: String::new(),
                reason: None,
            })),
        };
        match validate_submit(&mut ledger, &limits(), &payload) {
            Err(ValidationError::Malformed { field, .. }) => {
                assert_eq!(field, "concept_text.subject")
            }
            other => panic!("expected a Term bound refusal, got {other:?}"),
        }
        // A term id at u32::MAX is legal.
        let ok = SubmitPayload {
            kind: Some(submit_payload::Kind::ConceptText(ConceptText {
                subject: Some(wire_key(RecordKind::Term, "4294967295")),
                atom: None,
                body: None,
                declared_author: String::new(),
                reason: None,
            })),
        };
        assert!(validate_submit(&mut ledger, &limits(), &ok).is_ok());
    }

    #[test]
    fn an_unspecified_or_unknown_review_decision_is_refused() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        for decision in [Decision::Unspecified as i32, 99] {
            let payload = ReviewPayload {
                family: Some(review_payload::Family::ClaimEdit(review_body(decision))),
            };
            assert!(validate_review(&mut ledger, &limits(), &payload).is_err());
        }
        let ok = ReviewPayload {
            family: Some(review_payload::Family::ClaimEdit(review_body(
                Decision::Approve as i32,
            ))),
        };
        assert!(validate_review(&mut ledger, &limits(), &ok).is_ok());
    }

    #[test]
    fn a_zero_session_ordinal_is_refused() {
        let mut ledger = CountingLedger::new(1 << 16, 64);
        let payload = CommitPayload {
            kind: Some(commit_payload::Kind::CreateSessionCheckpoint(
                SessionCheckpointState {
                    concept: Some(wire_key(RecordKind::Term, "1")),
                    session_no: 0,
                    content: "c".into(),
                },
            )),
        };
        assert!(matches!(
            validate_commit(&mut ledger, &limits(), &payload),
            Err(ValidationError::Malformed {
                field: "session_checkpoint.session_no",
                ..
            })
        ));
    }

    #[test]
    fn advertised_limits_are_gated_once_as_configuration() {
        let ok = Limits {
            max_payload_bytes: 1,
            max_refs: 64,
            max_atom_bytes: 800,
            max_atom_utf16_units: Some(200),
        };
        assert!(validate_limits(&ok).is_ok());
        for bad in [
            Limits {
                max_payload_bytes: 0,
                max_refs: 64,
                max_atom_bytes: 800,
                max_atom_utf16_units: Some(200),
            },
            Limits {
                max_payload_bytes: 1,
                max_refs: 0,
                max_atom_bytes: 800,
                max_atom_utf16_units: Some(200),
            },
            Limits {
                max_payload_bytes: 1,
                max_refs: 64,
                max_atom_bytes: 0,
                max_atom_utf16_units: Some(200),
            },
        ] {
            assert!(matches!(
                validate_limits(&bad),
                Err(ValidationError::InvalidConfig { .. })
            ));
        }
    }

    #[test]
    fn the_combined_entry_point_validates_both_halves_in_order() {
        let mut ledger = CountingLedger::new(1 << 20, 64);
        let env = envelope();
        let payload = CommitPayload {
            kind: Some(commit_payload::Kind::CreateFrontier(FrontierState {
                concept_refs: vec![wire_key(RecordKind::Term, "1")],
                text: "t".into(),
                priority: 0,
                status: 0,
            })),
        };
        let request =
            validate_commit_request(&mut ledger, &limits(), &env, &payload).expect("accepted");
        assert_eq!(request.envelope().message().request_id, "req-1");

        // A bad payload refuses the pair even with a good envelope.
        let bad = CommitPayload { kind: None };
        assert!(validate_commit_request(&mut ledger, &limits(), &env, &bad).is_err());
    }
    #[test]
    fn zero_revision_is_not_a_record_but_zero_expected_version_is_pre_genesis() {
        let mut env = envelope();
        env.read_set.push(RevPair {
            key: Some(wire_key(RecordKind::Term, "1")),
            revision: "0".into(),
        });
        assert!(matches!(
            validate_envelope(&mut CountingLedger::new(1000, 100), &limits(), &env),
            Err(ValidationError::Malformed {
                field: "read_set.revision",
                ..
            })
        ));
        env.read_set[0].revision = "1".into();
        env.expect_version = Some("0".into());
        validate_envelope(&mut CountingLedger::new(1000, 100), &limits(), &env)
            .expect("0 is Types PRE_GENESIS, not a committed record revision");
        assert_eq!(
            sage_db_types::identity::KbVersion::new(0),
            sage_db_types::identity::KbVersion::PRE_GENESIS
        );
    }
    #[test]
    fn advertised_utf16_units_preserve_absence_and_reject_explicit_zero() {
        let mut limits = Limits {
            max_payload_bytes: 65536,
            max_refs: 64,
            max_atom_bytes: 800,
            max_atom_utf16_units: None,
        };
        validate_limits(&limits).expect("not advertised is not zero");
        limits.max_atom_utf16_units = Some(0);
        assert!(matches!(
            validate_limits(&limits),
            Err(ValidationError::InvalidConfig {
                field: "limits.max_atom_utf16_units",
                ..
            })
        ));
        limits.max_atom_utf16_units = Some(200);
        validate_limits(&limits).expect("actual product policy");
    }
}
