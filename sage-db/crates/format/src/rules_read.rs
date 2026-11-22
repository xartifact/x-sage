//! Rules-blob read path: `storage-format.md` §7.4 and `storage-digests.md` §5.4.
//!
//! [`crate::rules`] is the *writer*: it produces canonical rules bytes from a profile
//! constant. That alone cannot satisfy load-time verification, because `163 rules_digest`
//! is data — a KB may hold rules written by an older build or a migration transaction, and
//! comparing only against "the constants in this binary" would miss a blob that was
//! replaced on disk while keeping its profile name.
//!
//! So this module does the four steps §5.4 requires, in order, on **borrowed** bytes:
//!
//! 1. parse the blob as canonical Rules encoding (`format_version`, profile, counts,
//!    lengths, ordering, closed enforcement domain, per-rule parameter layout);
//! 2. `raw_sha256(blob) == 166 rules_ref`;
//! 3. `digest("rules", blob) == 163 rules_digest`;
//! 4. blob's `profile` field `== 196 rules_profile`.
//!
//! Two hashes, never one standing in for the other: `rules_ref` is the undomained blob
//! path hash and `rules_digest` is the domain-separated semantic identity
//! (`storage-digests` §1). A reader that verified only one of them would pass a KB whose
//! blob name and semantic digest disagreed.
//!
//! ## The classification that matters
//!
//! §7.4 forbids the shortcut "these bytes differ from my built-in constants, so CORRUPT".
//! `166` and `163` already prove byte integrity and consistency with what was declared; what
//! remains is whether *this binary* can evaluate that rule set. Hence:
//!
//! | observation | class |
//! |---|---|
//! | not a legal Rules encoding, including a malformed parameter layout | `Corrupt(RulesEncodingInvalid)` |
//! | `raw_sha256(blob) != rules_ref` | `Corrupt(RulesRefHashMismatch)` |
//! | `digest("rules", blob) != rules_digest` | `Corrupt(RulesDigestMismatch)` |
//! | blob profile `!= rules_profile` | `Corrupt(RulesProfileMismatch)` |
//! | legal bytes, rule set this build cannot evaluate | `UnsupportedFormat(RulesNotSupported)` |
//!
//! The last row is why a well-formed blob whose template differs from
//! [`crate::rules::CanonicalRules`] is **not** corruption: the operator action is migrate,
//! not restore-from-backup (`storage-format` §9.2). This module never compares bytes to the
//! built-in encoding to decide a verdict; it compares them only to say *which* profile's
//! canonical form they equal, and an unequal-but-well-formed blob is unsupported.
//!
//! ## Complete rule bodies participate in the support comparison
//!
//! A rules configuration is the profile **plus every per-relation field plus every
//! validation rule's id, enforcement and parameters**. Comparing enforcement but not
//! parameters would accept a blob that, say, restricts R1 to a different relation set while
//! claiming to be `formal-v1` — two different "what may be derived" answers under one
//! verdict. So [`ParsedRules`] retains the parameter bytes, and
//! `ParsedRules::matches_builtin` compares them; that is a *content* difference and maps to
//! `Unsupported`, while a malformed parameter **layout** is `Corrupt`. (The comparison
//! method is intentionally not a doc link: it is private, and a public doc page cannot
//! link to a private item under `-D rustdoc::private_intra_doc_links`.)
//!
//! ## Resource discipline
//!
//! The blob is borrowed and nothing is copied out of it: names and parameter blocks are
//! `&'a str` / `&'a [u8]` slices into the caller's buffer, and the two fixed-size tables are
//! arrays, not `Vec`s. So there is no retained second copy to admit — the single charge is
//! the extent of the buffer being scanned, and it is taken **before** any byte is
//! interpreted. Bounds come from [`Limits`] (the normative field ceilings of
//! `storage-format` §1.3), not from constants invented here.

use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, Refusal, UnsupportedReason,
};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::relation::RuleProfile;

use crate::digest::{BoundedHashError, Domain, digest_bounded, raw_sha256_bounded};
use crate::rules::{CanonicalRules, Enforcement, RULES_FORMAT_VERSION, VALIDATION_IDS};
use crate::wire::{DecodeError, ReadError, Reader};

/// Number of relations the encoding must carry (§5.1 fixes it at 7).
const RELATION_COUNT: usize = 7;

/// Number of profile-global validation rules (§5.1 fixes it at 12).
const VALIDATION_COUNT: usize = 12;

/// The declared identities a blob must match, taken from the event that named it.
///
/// All three come from the *same* transaction (`166`, `163`, `196`), which is why a
/// mismatch between the blob and any of them is corruption rather than an unsupported
/// feature: the transaction contradicts itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclaredRules {
    /// `166 rules_ref`: the undomained SHA-256 that names the blob.
    pub rules_ref: [u8; 32],
    /// `163 rules_digest`: `digest("rules", canonical bytes)`.
    pub rules_digest: [u8; 32],
    /// `196 rules_profile`: 0 legacy-v1, 1 formal-v1.
    pub profile: RuleProfile,
}

/// A verified rules blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifiedRules {
    /// The profile the blob declares, already equal to [`DeclaredRules::profile`].
    pub profile: RuleProfile,
    /// The recomputed undomained hash, already equal to `rules_ref`.
    pub rules_ref: [u8; 32],
    /// The recomputed domain digest, already equal to `rules_digest`.
    pub rules_digest: [u8; 32],
}

/// One parsed relation template, as the bytes describe it.
///
/// Borrowed from the caller's blob: the name is a slice, not a copy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsedRelation<'a> {
    /// `id`, 0..=6.
    pub id: u16,
    /// The relation's wire name, as encoded.
    ///
    /// Part of the rule set, not decoration: §5.1 puts the name in the canonical bytes, so
    /// two blobs differing only in a name are different configurations.
    pub name: &'a str,
    /// Whether this relation is a definition edge.
    pub is_definition: u8,
    /// Whether R1 applies.
    pub transitive: u8,
    /// Whether R2 applies.
    pub symmetric: u8,
    /// Whether cycles are rejected.
    pub acyclic: u8,
    /// `0 unrestricted / 1 target<=source / 2 target<source`.
    pub layer_rule: u16,
    /// Whether this relation implies `depends-on`.
    pub implies_dependency: u8,
}

/// One parsed validation rule: identity, enforcement and its full parameter block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsedRule<'a> {
    /// Rule id, 1..=12.
    pub id: u16,
    /// `0 off / 1 on`.
    pub enforcement: u16,
    /// The parameter bytes exactly as encoded.
    ///
    /// Retained (by reference) because parameters are part of what the rule *means*: R1's
    /// relation list, V5's layer/threshold, the visit-order string. Their **layout** is
    /// validated during the parse; their **values** participate in the support comparison.
    pub params: &'a [u8],
}

/// A fully parsed rules blob.
///
/// Parsing is separated from verification so a caller can inspect a legal-but-unsupported
/// rule set without the verdict machinery, and so the parse itself has one definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParsedRules<'a> {
    /// `format_version`, already checked to be [`RULES_FORMAT_VERSION`].
    pub format_version: u16,
    /// The profile id as encoded, before it is compared to any declared value.
    pub profile_id: u16,
    /// The profile name as encoded, for diagnostics only. The id is authoritative.
    pub profile_name: &'a str,
    /// The seven relation templates, in ascending id order.
    pub relations: [ParsedRelation<'a>; RELATION_COUNT],
    /// The twelve validation rules, in ascending rule-id order.
    pub rules: [ParsedRule<'a>; VALIDATION_COUNT],
}

impl ParsedRules<'_> {
    /// The declared profile, when the id is one this build knows.
    #[must_use]
    pub const fn known_profile(&self) -> Option<RuleProfile> {
        RuleProfile::from_u16(self.profile_id)
    }

    /// The parsed rule with `id`, when present.
    #[must_use]
    pub fn rule(&self, id: u16) -> Option<&ParsedRule<'_>> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// The enforcement value of rule `id`, when present.
    #[must_use]
    pub fn enforcement_of(&self, id: u16) -> Option<Enforcement> {
        match self.rule(id)?.enforcement {
            0 => Some(Enforcement::Off),
            _ => Some(Enforcement::On),
        }
    }

    /// Whether these parsed bytes equal this build's canonical encoding for `profile`.
    ///
    /// Compares every value that changes behaviour: profile identity, each relation's fields
    /// and name, each rule's enforcement **and its parameters**. A blob that agrees on
    /// enforcement but not on parameters is a different rule set.
    fn matches_builtin(&self, profile: RuleProfile, ledger: &mut dyn SharedLedger) -> BuiltinMatch {
        let canonical = match CanonicalRules::new(profile).encode(ledger) {
            Ok(bytes) => bytes,
            Err(error) => {
                return match error {
                    crate::wire::WriteError::Budget(b) => BuiltinMatch::Refused(Refusal::Budget(b)),
                    crate::wire::WriteError::Stopped(s) => {
                        BuiltinMatch::Refused(Refusal::Stopped(s))
                    }
                    crate::wire::WriteError::Allocation { requested } => {
                        BuiltinMatch::Refused(Refusal::Allocation { requested })
                    }
                    crate::wire::WriteError::Data(_)
                    | crate::wire::WriteError::MissingRequiredField { .. }
                    | crate::wire::WriteError::Internal { .. } => {
                        BuiltinMatch::Format(FormatError::Internal(
                            sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
                        ))
                    }
                };
            }
        };
        // Re-parse the built-in bytes with this same parser, then compare decoded values.
        // The built-in encode is a fixed 381-byte buffer, so the ceiling passed here cannot
        // refuse it; passing the strictest legal limits keeps one parse definition.
        match parse_rules(&canonical, &Limits::V1, ledger) {
            FormatOutcome::Ok(other) => BuiltinMatch::Matches(
                other.format_version == self.format_version
                    && other.profile_id == self.profile_id
                    && other.profile_name == self.profile_name
                    && other.relations == self.relations
                    && other.rules == self.rules,
            ),
            FormatOutcome::Format(error) => BuiltinMatch::Format(error),
            FormatOutcome::Refused(refusal) => BuiltinMatch::Refused(refusal),
        }
    }
}

/// Result of comparing parsed bytes to the built-in encoding, keeping refusals separate.
enum BuiltinMatch {
    Matches(bool),
    Refused(Refusal),
    Format(FormatError),
}

/// Parse a blob as canonical Rules encoding without verifying any declared identity.
///
/// Validates, in §7.4's order: `format_version`, the profile name and id, the relation
/// count and each relation's id/name/closed field domains, then the validation count, each
/// rule id, its closed `enforcement` domain, and its parameter **layout** (§5.2). Trailing
/// bytes are rejected — a longer blob is not the encoding it claims to be.
///
/// Parameter values are not compared here: which relation ids R1 lists is content, and
/// content differences are [`UnsupportedReason::RulesNotSupported`] at
/// [`verify_rules`], not corruption. What is checked here is that the block *fits* its
/// declared shape, because a block that does not fit cannot be any legal rules encoding.
///
/// # Errors
///
/// `Corrupt(RulesEncodingInvalid)` for a structural violation; `Limit(..)` when a string or
/// list exceeds [`Limits`]; refusals for budget, cancellation and allocation.
pub fn parse_rules<'a>(
    blob: &'a [u8],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<ParsedRules<'a>> {
    // Empty input is an operation too: observe the caller's policy before deciding anything,
    // then refuse: no bytes is not a rules encoding (§5.1 fixes format_version as present).
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    // The bytes are borrowed and nothing is copied out of them, so the whole scan is what the
    // caller pays for. Admission precedes interpretation: a zero budget refuses before the
    // first field is read rather than after a partial walk.
    if let Err(error) = ledger.try_charge(blob.len() as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if blob.is_empty() {
        return encoding_invalid();
    }

    let mut reader = Reader::new(blob);

    let format_version = match reader.u16() {
        Ok(v) => v,
        Err(error) => return from_read_error(error),
    };
    if format_version != RULES_FORMAT_VERSION {
        // A future encoding version is not damage, but this reader cannot parse it *as this
        // version*, so it cannot be judged legal either. `Unsupported` is reserved for bytes
        // that parse but describe a rule set this build cannot evaluate.
        return encoding_invalid();
    }

    let profile_id = match reader.u16() {
        Ok(v) => v,
        Err(error) => return from_read_error(error),
    };
    // The profile id is closed: 0 legacy, 1 formal. An unknown id is still well-formed bytes,
    // so it is NOT a parse failure — `verify_rules` classifies it as unsupported. Parsing must
    // not smuggle that verdict in through `Corrupt`.
    // §5.1 calls the profile name "redundant but human-readable" and states no non-empty
    // domain, and §8.1 makes an empty `str` a legal encoding (only `encoded_len = 0` is
    // illegal). A zero-length name is therefore well-formed bytes carrying a different value:
    // the support comparison reports that as Unsupported, not the parser as Corrupt.
    let profile_name = match reader.str(limits.max_string_bytes) {
        Ok(v) => v,
        Err(error) => return from_read_error(error),
    };

    let relation_count = match reader.u16() {
        Ok(v) => v,
        Err(error) => return from_read_error(error),
    };
    if relation_count as usize != RELATION_COUNT {
        return encoding_invalid();
    }
    let mut relations: [Option<ParsedRelation<'a>>; RELATION_COUNT] = [None; RELATION_COUNT];
    for (index, slot) in relations.iter_mut().enumerate() {
        if let Err(stop) = ledger.checkpoint() {
            return FormatOutcome::Refused(Refusal::Stopped(stop));
        }
        let id = match reader.u16() {
            Ok(v) => v,
            Err(error) => return from_read_error(error),
        };
        // §5.1: relations are encoded in ascending relation-id order, ids 0..=6. Both the
        // order and the domain are part of the encoding, not a reader preference.
        if id as usize != index {
            return encoding_invalid();
        }
        // Same rule as the profile name: an empty relation name is legal shape, and a
        // different name is a different rule set (Unsupported), not malformed bytes.
        let name = match reader.str(limits.max_string_bytes) {
            Ok(name) => name,
            Err(error) => return from_read_error(error),
        };
        // ============================ FIELD ORDER, DO NOT REORDER ============================
        // §5.1's per-relation order is exactly:
        //     id:u16 | name:str | is_definition:u8 | transitive:u8 | symmetric:u8 |
        //     acyclic:u8 | layer_rule:u16 | implies_dependency:u8
        // `implies_dependency` sits AFTER `layer_rule`, not with the four flags. Appending a
        // new per-relation field to the flag run below (rather than at the end of the
        // relation record) consumes `layer_rule` as a flag and shifts every later field,
        // which stays *parseable* and yields a plausible but wrong rule set. Add new fields
        // at the end of this list, and extend the canonical encoder in the same commit.
        let (is_definition, transitive, symmetric, acyclic) = match read_four_flags(&mut reader) {
            Ok(values) => values,
            Err(FlagFailure::Invalid) => return encoding_invalid(),
            Err(FlagFailure::Read(error)) => return from_read_error(error),
        };
        let layer_rule = match reader.u16() {
            Ok(v) if v <= 2 => v,
            Ok(_) => return encoding_invalid(),
            Err(error) => return from_read_error(error),
        };
        let implies_dependency = match reader.u8() {
            Ok(v) if v <= 1 => v,
            Ok(_) => return encoding_invalid(),
            Err(error) => return from_read_error(error),
        };
        *slot = Some(ParsedRelation {
            id,
            name,
            is_definition,
            transitive,
            symmetric,
            acyclic,
            layer_rule,
            implies_dependency,
        });
    }

    let validation_count = match reader.u16() {
        Ok(v) => v,
        Err(error) => return from_read_error(error),
    };
    if validation_count as usize != VALIDATION_COUNT {
        return encoding_invalid();
    }
    let mut rules: [Option<ParsedRule<'a>>; VALIDATION_COUNT] = [None; VALIDATION_COUNT];
    for (index, expected_id) in VALIDATION_IDS.iter().enumerate() {
        if let Err(stop) = ledger.checkpoint() {
            return FormatOutcome::Refused(Refusal::Stopped(stop));
        }
        let id = match reader.u16() {
            Ok(v) => v,
            Err(error) => return from_read_error(error),
        };
        // Rule ids are a fixed ascending set (§5.2); order and membership are encoding.
        if id != *expected_id {
            return encoding_invalid();
        }
        let enforcement = match reader.u16() {
            Ok(v) => v,
            Err(error) => return from_read_error(error),
        };
        // §5.2: `enforcement` domain is 0 off / 1 on. An "off" rule stays in the bytes
        // precisely so it cannot be confused with an unknown rule.
        if enforcement > 1 {
            return encoding_invalid();
        }
        let params = match read_params(&mut reader, id, limits, ledger) {
            Ok(params) => params,
            Err(ParamFailure::Malformed) => return encoding_invalid(),
            Err(ParamFailure::Read(error)) => return from_read_error(error),
            Err(ParamFailure::Budget(budget)) => {
                return FormatOutcome::Refused(Refusal::Budget(budget));
            }
            Err(ParamFailure::Limit(limit)) => {
                return FormatOutcome::Format(FormatError::Limit(limit));
            }
            Err(ParamFailure::Stopped(stop)) => {
                return FormatOutcome::Refused(Refusal::Stopped(stop));
            }
        };
        rules[index] = Some(ParsedRule {
            id,
            enforcement,
            params,
        });
    }

    // A longer blob is not this encoding: trailing bytes would be authoritative data the
    // reader silently ignored.
    if !reader.is_empty() {
        return encoding_invalid();
    }

    FormatOutcome::Ok(ParsedRules {
        format_version,
        profile_id,
        profile_name,
        // Both tables were filled in full, in order, or the parse returned already.
        relations: core::array::from_fn(|index| {
            relations[index].expect("every relation slot is filled before this point")
        }),
        rules: core::array::from_fn(|index| {
            rules[index].expect("every rule slot is filled before this point")
        }),
    })
}

/// Verify a rules blob against the identities declared by the event that named it.
///
/// Order is §5.4's: parse first (a blob that is not legal encoding has no hashes worth
/// reporting), then the raw reference, then the domain digest, then the profile. Each step
/// keeps its own class, so an operator gets the action that matches the fault.
///
/// ## Cost
///
/// Three passes over the blob, each charging its own extent against the caller's ledger:
///
/// 1. the structural parse (`parse_rules`);
/// 2. `raw_sha256_bounded`, for the blob name;
/// 3. `digest_bounded(Domain::Rules)`, for the semantic digest.
///
/// They are separate passes rather than one prepaid charge because each is distinct work: the
/// parse interprets fields, and the two hashes use different prefixes and length framing so
/// they cannot share a hash state. Each is interruptible (`checkpoint`) and each refusal is a
/// [`Refusal`], never a bytes verdict.
///
/// # Errors
///
/// See the module table. Budget, cancellation and allocation arrive as [`Refusal`].
pub fn verify_rules(
    blob: &[u8],
    declared: &DeclaredRules,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<VerifiedRules> {
    let parsed = match parse_rules(blob, limits, ledger) {
        FormatOutcome::Ok(p) => p,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    // Step 2: the undomained content hash. This is the blob *name*, so it must equal the
    // declared reference byte for byte.
    //
    // Bounded and checkpointed rather than a plain scan: a large rules blob must be
    // interruptible and its work must be visible to the caller's ledger. This is a genuinely
    // separate pass from the parse above, and it pays for its own extent — the passes are
    // enumerated in this function's docs so the total is explicit rather than accidental.
    let rules_ref = match raw_sha256_bounded(blob, ledger) {
        Ok(value) => value,
        Err(refused) => return bounded_refusal(refused),
    };
    if rules_ref != declared.rules_ref {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RulesRefHashMismatch));
    }

    // Step 3: the domain-separated digest. Separate from step 2 on purpose: `raw_sha256` and
    // `digest("rules", ..)` are different functions and one never implies the other, and they
    // cannot share a hash state (different prefixes, different length framing).
    let rules_digest = match digest_bounded(Domain::Rules, blob, ledger) {
        Ok(value) => value,
        Err(refused) => return bounded_refusal(refused),
    };
    if rules_digest != declared.rules_digest {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RulesDigestMismatch));
    }

    // Step 4: the profile the bytes declare must be the one the header recorded. Compared
    // against the blob's OWN `196`, never against a default profile: §7.4 forbids deriving
    // CORRUPT from "differs from my constants", so a legacy-v1 blob whose header says legacy
    // is a supported configuration and must verify.
    // Compare the blob's encoded id against the header's `196` FIRST. §7.4's table lists
    // "blob profile field != 196" as CORRUPT unconditionally, so the check must not be
    // reachable only when the blob happens to name a profile this build knows: an unknown id
    // that also disagrees with the header is a contradiction inside one transaction, which is
    // corruption, not a support question.
    let declared_id = profile_id_of(declared.profile);
    if parsed.profile_id != declared_id {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RulesProfileMismatch));
    }
    // Only now, with the identity question settled, ask whether this build can evaluate the
    // rule set. Legal bytes for an unknown profile id are unsupported: migrate, do not
    // restore.
    let profile = match parsed.known_profile() {
        Some(profile) => profile,
        None => {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::RulesNotSupported,
            ));
        }
    };

    // Finally: are these bytes the built-in template for that profile, in every field that
    // changes behaviour — including each rule's parameters? A difference is NOT damage
    // (§7.4); it is a rule set this build cannot evaluate, and the action is migrate.
    match parsed.matches_builtin(profile, ledger) {
        BuiltinMatch::Matches(true) => {}
        BuiltinMatch::Matches(false) => {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::RulesNotSupported,
            ));
        }
        BuiltinMatch::Refused(refusal) => return FormatOutcome::Refused(refusal),
        BuiltinMatch::Format(error) => return FormatOutcome::Format(error),
    }

    FormatOutcome::Ok(VerifiedRules {
        profile,
        rules_ref,
        rules_digest,
    })
}

/// The persisted profile id for a profile (§5.1: 0 legacy-v1, 1 formal-v1).
///
/// `crate::rules` keeps the encoder's copy private, so this is the reader's single mapping.
/// `first_rule_id_matches_the_encoder`-style tests would not catch a drift here, so the
/// accepted-vector tests do: the two D10 blobs encode ids 0 and 1 and must verify.
const fn profile_id_of(profile: RuleProfile) -> u16 {
    match profile {
        RuleProfile::LegacyV1 => 0,
        RuleProfile::FormalV1 => 1,
    }
}

/// Why reading a validation rule's parameters failed.
enum ParamFailure {
    /// The block does not fit the layout §5.2 fixes for this rule id.
    Malformed,
    /// A string inside the block exceeded the caller's declared ceiling. Kept distinct from
    /// `Malformed` so rule 11's `str` cannot erase a `FORMAT_LIMIT` into corruption.
    Limit(LimitKind),
    /// The underlying read failed.
    Read(ReadError),
    /// The ledger refused the read: a refusal, never a bytes verdict.
    Budget(sage_db_types::ledger::BudgetError),
    /// The caller's policy asked to stop inside the parameter loop.
    Stopped(sage_db_types::query::ControlStop),
}

/// Read one rule's parameter block, validating its **layout** for `id`.
///
/// §5.1 prefixes every block with `params_len:u32`; §5.2 fixes six shapes:
///
/// | rule ids | shape | exact `params_len` |
/// |---|---|---|
/// | 1, 2, 3, 8, 9, 10 | `u32 n` then `n × u16` relation ids | `4 + 2n` |
/// | 6 | `u32 subject_count` then one `u16 relation_id` | `6` |
/// | 7 | `u8 source_layer`, `u16 relation_id`, `u8 min_targets` | `4` |
/// | 11 | `str` stable visit order | `4 + len` |
/// | 4, 5, 12 | empty | `0` |
///
/// The whole block is taken as **one borrowed slice** and interpreted by a sub-reader, so the
/// retained parameter bytes are a slice of the caller's blob: no copy, nothing to admit, and a
/// short block is a malformed layout rather than a truncated read discovered later.
///
/// A block that does not fit its shape is [`CorruptReason::RulesEncodingInvalid`] — the same
/// class as every other structural fault here. No separate `Limit` verdict is invented: the
/// document declares no rules-parameter ceiling, and `params_len` is a self-describing length
/// whose agreement with the block is an encoding question, not a caller-budget question.
fn read_params<'a>(
    reader: &mut Reader<'a>,
    id: u16,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<&'a [u8], ParamFailure> {
    let len = match reader.u32() {
        Ok(v) => v,
        Err(error) => return Err(ParamFailure::Read(error)),
    };

    // Fixed shapes first: an impossible length is refused before the block is consumed, so a
    // hostile length cannot become a large take.
    match id {
        6 if len != 6 => return Err(ParamFailure::Malformed),
        7 if len != 4 => return Err(ParamFailure::Malformed),
        4 | 5 | 12 if len != 0 => return Err(ParamFailure::Malformed),
        _ => {}
    }

    let block = match reader.take_exact(len as usize) {
        Ok(block) => block,
        Err(ReadError::Budget(budget)) => return Err(ParamFailure::Budget(budget)),
        Err(error) => return Err(ParamFailure::Read(error)),
    };

    // Interpret the block. Its length is known, so a mismatch here is a layout violation, not a
    // truncation. The string ceiling is the one normative bound the shape depends on (rule 11's
    // visit order is a `str` field, so the schema's string ceiling applies to it).
    let mut sub = Reader::new(block);
    match id {
        // `u32 n` + `n × u16`, each a relation id from the closed set 0..=6.
        // Rules 1, 2, 3, 8 and 9 carry RELATION-ID lists: their elements name relations, so the
        // closed set 0..=6 is a shape-level domain. Rule 10 is deliberately NOT in this group:
        // §5.2 row 10 is the annotation lattice (bottom < contested < hypothetical < confirmed),
        // whose elements are confidence levels, not relations. Its `[0,1,2]` is the formal-v1
        // VALUE; the format states no element domain for it, so an element above 6 must fall
        // through to the support comparison like rules 6 and 7 — not be called malformed.
        1 | 2 | 3 | 8 | 9 => {
            let count = match sub.u32() {
                Ok(v) => v,
                Err(_) => return Err(ParamFailure::Malformed),
            };
            // ORDER MATTERS, and it is shape/impossibility BEFORE the ceiling.
            //
            // A count the block cannot satisfy (say `n = 1000` in a block holding 3 ids) is
            // not a legal encoding under ANY declared ceiling, so it must stay CORRUPT.
            // Checking the ceiling first would report `Limit(ListItems)` for those bytes and
            // hide real corruption behind a "retry with a bigger budgets" answer.
            let expected = (count as u64)
                .checked_mul(2)
                .ok_or(ParamFailure::Malformed)?;
            if sub.remaining() as u64 != expected {
                return Err(ParamFailure::Malformed);
            }
            // Now the ceiling. A caller-declared LIST limit behaves exactly as a string ceiling
            // does: the same bytes are legal under `Limits::V1` and over-allowance under a
            // tightened `max_list_items`, so this is FORMAT_LIMIT.
            // `tag: None` for the same reason as `LimitKind::String`: no registered field tag
            // exists for these lists, and a rule id is not a field tag.
            if count > limits.max_list_items {
                return Err(ParamFailure::Limit(LimitKind::ListItems {
                    tag: None,
                    count,
                    max: limits.max_list_items,
                }));
            }
            // (C) The list is a retained table, so its elements are admitted as ENTRIES, and the
            // loop honours cancellation per element. `field::read_str_list` is the precedent.
            if let Err(error) = ledger.try_charge_entries(u64::from(count)) {
                return Err(ParamFailure::Budget(error));
            }
            for _ in 0..count {
                if let Err(stop) = ledger.checkpoint() {
                    return Err(ParamFailure::Stopped(stop));
                }
                let relation = match sub.u16() {
                    Ok(v) => v,
                    Err(_) => return Err(ParamFailure::Malformed),
                };
                if relation >= RELATION_COUNT as u16 {
                    return Err(ParamFailure::Malformed);
                }
            }
        }
        // Rule 10: same `u32 count + count × u16` shape as the relation lists, but the elements
        // are annotation-lattice entries with no stated domain, so they are read and left
        // UNFILTERED. Whether they equal this build's template is `matches_builtin`'s question,
        // and a difference there is `Unsupported`, never `Corrupt`.
        10 => {
            let count = match sub.u32() {
                Ok(v) => v,
                Err(_) => return Err(ParamFailure::Malformed),
            };
            // Impossibility first, then the caller's ceiling — the same order as the relation
            // lists, for the same reason: a count the block cannot hold is corrupt at any limit.
            let expected = (count as u64)
                .checked_mul(2)
                .ok_or(ParamFailure::Malformed)?;
            if sub.remaining() as u64 != expected {
                return Err(ParamFailure::Malformed);
            }
            if count > limits.max_list_items {
                return Err(ParamFailure::Limit(LimitKind::ListItems {
                    tag: None,
                    count,
                    max: limits.max_list_items,
                }));
            }
            if let Err(error) = ledger.try_charge_entries(u64::from(count)) {
                return Err(ParamFailure::Budget(error));
            }
            for _ in 0..count {
                if let Err(stop) = ledger.checkpoint() {
                    return Err(ParamFailure::Stopped(stop));
                }
                // Read for position only: no domain check, by design.
                if sub.u16().is_err() {
                    return Err(ParamFailure::Malformed);
                }
            }
        }

        // `u32 subject_count` + `u16 relation_id`.
        //
        // `subject_count` is a SCALAR cardinality, not a count of following elements: the block
        // is a fixed 6 bytes whatever its value. Its `1` in §5.2 is the formal-v1 TEMPLATE
        // value, and the support comparison below rejects a different one as
        // `Unsupported` — so treating a different value as a malformed *shape* here would
        // conflate "this build's template" with "any legal encoding". Only the relation id has
        // a shape-level domain, because relations are a closed set.
        6 => {
            let _subject_count = match sub.u32() {
                Ok(v) => v,
                Err(_) => return Err(ParamFailure::Malformed),
            };
            match sub.u16() {
                Ok(relation) if relation < RELATION_COUNT as u16 => {}
                _ => return Err(ParamFailure::Malformed),
            }
        }
        // `u8 source_layer` + `u16 relation_id` + `u8 min_targets`.
        //
        // `source_layer` is a semantic layer (0..=3, a closed domain the type system also
        // knows) and `relation_id` names a closed set, so both are shape-level. `min_targets`
        // is a `u8` threshold with NO stated lower bound beyond its own type: §5.2's `1` is the
        // formal-v1 template value, and a different value is a different rule set (Unsupported),
        // not malformed bytes.
        7 => {
            let source_layer = match sub.u8() {
                Ok(v) => v,
                Err(_) => return Err(ParamFailure::Malformed),
            };
            let relation = match sub.u16() {
                Ok(v) => v,
                Err(_) => return Err(ParamFailure::Malformed),
            };
            // `min_targets` is read and NOT range-checked: see the comment above.
            if sub.u8().is_err() {
                return Err(ParamFailure::Malformed);
            }
            if source_layer > 3 || relation >= RELATION_COUNT as u16 {
                return Err(ParamFailure::Malformed);
            }
        }
        // `str` stable visit order, under the schema's string ceiling.
        //
        // Any `str` that fits the block is a legal shape, including the empty one: §5.2's
        // exact text is the formal-v1 TEMPLATE value, so a different text (or an empty one) is a
        // different rule set, which the support comparison reports as `Unsupported`. Only a
        // block that does not decode as a `str` at all is malformed here.
        11 => match sub.str(limits.max_string_bytes) {
            Ok(_) => {}
            Err(ReadError::Data(DecodeError::StringTooLong { len, max })) => {
                return Err(ParamFailure::Limit(LimitKind::String {
                    tag: None,
                    len,
                    max,
                }));
            }
            Err(_) => return Err(ParamFailure::Malformed),
        },
        4 | 5 | 12 => {}
        // Unreachable: the caller checked `id` against `VALIDATION_IDS`. A conservative
        // malformed verdict rather than a panic, because a future id added to that table
        // without a layout here must not become an abort.
        _ => return Err(ParamFailure::Malformed),
    }

    if !sub.is_empty() {
        return Err(ParamFailure::Malformed);
    }
    Ok(block)
}

/// Read the four consecutive single-byte relation flags.
///
/// One helper rather than four near-identical matches: each is a closed `0/1` domain, and
/// stating that once keeps the rule in a single place. `implies_dependency` is deliberately
/// NOT read here — it follows `layer_rule` in the encoding (§5.1).
fn read_four_flags(reader: &mut Reader<'_>) -> Result<(u8, u8, u8, u8), FlagFailure> {
    let mut read = || -> Result<u8, FlagFailure> {
        match reader.u8() {
            Ok(v) if v <= 1 => Ok(v),
            Ok(_) => Err(FlagFailure::Invalid),
            Err(error) => Err(FlagFailure::Read(error)),
        }
    };
    Ok((read()?, read()?, read()?, read()?))
}

/// Why reading the four single-byte relation flags failed.
enum FlagFailure {
    /// A flag was outside `0..=1`, which no legal encoding produces.
    Invalid,
    /// The underlying read failed.
    Read(ReadError),
}

/// Map a bounded-hash failure: a refusal stays a refusal, never a bytes verdict.
fn bounded_refusal<T>(error: BoundedHashError) -> FormatOutcome<T> {
    match error {
        BoundedHashError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        BoundedHashError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
    }
}

fn encoding_invalid<T>() -> FormatOutcome<T> {
    FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RulesEncodingInvalid))
}

/// Map a wire read failure, keeping refusals as refusals.
///
/// Refusals are exhaustive; every structural `Data` failure inside this encoding is one
/// class, so the data arm is a catch-all rather than an enumeration that would need editing
/// every time the wire enum grows.
fn from_read_error<T>(error: ReadError) -> FormatOutcome<T> {
    match error {
        ReadError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        ReadError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
        ReadError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
        // A string that was READ SUCCESSFULLY but exceeds the caller's declared ceiling is
        // §9's FORMAT_LIMIT, not damage: the same bytes are legal under the normative
        // `Limits::V1` and only the caller's allowance is smaller. This is the one
        // `Data` arm that is not corruption, which is why it is matched by its own variant
        // rather than by "any error from a string read".
        //
        // `tag: None` on purpose: `LimitKind::String` carries a FIELD tag, and a rules blob has
        // no registered field tag for a profile name, a relation name or rule 11's parameter
        // string. Passing a rule id there would put one identifier in another's slot.
        ReadError::Data(DecodeError::StringTooLong { len, max }) => {
            FormatOutcome::Format(FormatError::Limit(LimitKind::String {
                tag: None,
                len,
                max,
            }))
        }
        // Truncation, bad UTF-8, a NUL in a string, trailing bytes: each means these bytes
        // cannot be a legal Rules encoding, and stays Corrupt even under a tightened ceiling.
        ReadError::Data(_) => encoding_invalid(),
        // A schema-level refusal about an event kind cannot arise here: this module never
        // reads an event.
        ReadError::Uninterpretable(_) => encoding_invalid(),
    }
}
