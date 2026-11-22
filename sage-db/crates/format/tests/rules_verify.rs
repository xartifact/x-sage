//! Rules-blob verification: `storage-format.md` §7.4, `storage-digests.md` §5.4.
//!
//! Fixture bytes live in `fixtures/rules-verify/` and come from an independent Python oracle
//! (`rules_oracle.py`, hand-written from §5.1/§5.2) that asserts the two accepted D10 profile
//! vectors before writing anything. The expected **verdicts** are a Rust table below: a
//! mis-typed class is a compile error, and this crate needs no third-party parser to read its
//! own fixtures.
//!
//! Nothing here is derived from [`crate::rules::CanonicalRules::encode`] except
//! [`production_encoder_matches_the_oracle_bytes`], which exists precisely to compare the
//! encoder against the oracle rather than against itself.
//!
//! The four-step order under test, and the class each step owns:
//! parse → `raw_sha256 == 166` → `digest("rules") == 163` → blob profile `== 196`.
//! A well-formed blob that is simply not this build's template must stay `Unsupported`, never
//! `Corrupt` (§7.4 forbids that shortcut).

#![allow(clippy::indexing_slicing)]

use sage_db_format::digest::{Domain, digest, raw_sha256};
use sage_db_format::rules_read::{DeclaredRules, parse_rules, verify_rules};
use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, Refusal, UnsupportedReason,
};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::relation::RuleProfile;

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// Refuses the first byte charge: proves a refusal reaches the caller from the deepest path.
struct NoBytes;
impl SharedLedger for NoBytes {
    fn try_charge(&mut self, b: u64) -> Result<(), BudgetError> {
        Err(BudgetError::new(BudgetUnit::Bytes, b, 0))
    }
    fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

struct Halt;
impl SharedLedger for Halt {
    fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn blob(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rules-verify")
        .join(name);
    // A missing mandatory input must fail, never skip: these are the whole evidence basis.
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("missing mandatory fixture {}: {e}", path.display()))
}

/// How a fixture is expected to be classified. Spelling the expectation as a type means a
/// wrong entry cannot be silently mistyped the way a JSON string can.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// A canonical encoding this build implements.
    Supported,
    /// Legal Rules encoding, but a rule set this build cannot evaluate.
    Unsupported,
    /// Not a legal Rules encoding.
    Malformed,
    /// Legal bytes whose profile id contradicts the declared `196`.
    ProfileMismatch,
}

/// Every fixture, with the class the oracle's construction implies.
///
/// The three "unsupported" rows are the §7.4 distinction: well-formed bytes whose template is
/// not this build's. `rules.bad.*` each break exactly one encoding rule.
const CASES: &[(&str, Class)] = &[
    ("rules.formal.bin", Class::Supported),
    ("rules.legacy.bin", Class::Supported),
    // Parses, but its id cannot equal any declared 196 (which is 0 or 1), so §7.4's
    // profile-binding row makes it CORRUPT rather than a support question.
    ("rules.unknown-profile-id.bin", Class::ProfileMismatch),
    ("rules.formal-rule12-off.bin", Class::Unsupported),
    ("rules.formal-altered-relation-name.bin", Class::Unsupported),
    ("rules.bad.format-version.bin", Class::Malformed),
    ("rules.bad.relation-count.bin", Class::Malformed),
    ("rules.bad.relation-order.bin", Class::Malformed),
    ("rules.bad.enforcement-domain.bin", Class::Malformed),
    ("rules.bad.validation-count.bin", Class::Malformed),
    ("rules.bad.trailing.bin", Class::Malformed),
    ("rules.bad.truncated.bin", Class::Malformed),
    ("rules.bad.zero-profile-name.bin", Class::Malformed),
    // rule 6 `subject_count` and rule 7 `min_targets` are SCALAR values with no shape-level
    // domain beyond their own integer type: §5.2's 1 is the formal-v1 TEMPLATE value, so a
    // different legal value is a different rule set (Unsupported), not malformed bytes.
    ("rules.formal-rule6-subject-count-2.bin", Class::Unsupported),
    ("rules.formal-v5-min-targets-0.bin", Class::Unsupported),
    // rule 10 is the ANNOTATION LATTICE, not a relation list: an element past the
    // relation count is a legal shape with a different value.
    ("rules.formal-rule10-lattice-7.bin", Class::Unsupported),
    ("rules.bad.rule7-length.bin", Class::Malformed),
    ("rules.bad.rule6-params-len-5.bin", Class::Malformed),
    ("rules.bad.rule6-params-len-7.bin", Class::Malformed),
    ("rules.bad.rule4-nonempty.bin", Class::Malformed),
    (
        "rules.bad.rule1-relation-out-of-domain.bin",
        Class::Malformed,
    ),
    ("rules.bad.empty.bin", Class::Malformed),
];

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The declared identities for a blob: its own hashes, which is what "the bytes match what the
/// event declared" means. Recomputed here rather than stored in a fixture, because this is a
/// recomputation and not an expectation.
fn declared_for(bytes: &[u8], profile: RuleProfile) -> DeclaredRules {
    DeclaredRules {
        rules_ref: raw_sha256(bytes),
        rules_digest: digest(Domain::Rules, bytes),
        profile,
    }
}

fn describe<T>(outcome: &FormatOutcome<T>) -> String {
    match outcome {
        FormatOutcome::Ok(_) => "Ok".to_string(),
        FormatOutcome::Format(e) => format!("Format({e:?})"),
        FormatOutcome::Refused(r) => format!("Refused({r:?})"),
    }
}

fn expect_corrupt<T>(outcome: FormatOutcome<T>, want: CorruptReason, what: &str) {
    match outcome {
        FormatOutcome::Format(FormatError::Corrupt(got)) if got == want => {}
        other => panic!(
            "{what}: expected Corrupt({want:?}), got {}",
            describe(&other)
        ),
    }
}

// ---------------------------------------------------------------------------
// Positive: the two accepted D10 profiles, verified end to end
// ---------------------------------------------------------------------------

/// The accepted vectors from `storage-digests.md` §7.3, quoted as literals so this test cannot
/// drift with the fixtures it is checking.
const ACCEPTED: &[(&str, RuleProfile, &str, &str)] = &[
    (
        "rules.formal.bin",
        RuleProfile::FormalV1,
        "04342325f9c6733f4dc100ee8d310b1d862f82f894b23589c303d87f25654f88",
        "19fe486994acf70283c1055ffa8a27057fe7b764146b1c9c7ac0b3a3f3176ded",
    ),
    (
        "rules.legacy.bin",
        RuleProfile::LegacyV1,
        "b6f92f3c3806be34b9343cae9524e8b7796395942e2d7d21e99ad1b960a2ec16",
        "b4f0366e9ba49f5984d0f10e142c8ce6854d5b321ebe89065878156b88aef741",
    ),
];

#[test]
fn both_accepted_profiles_verify_and_match_the_independent_hashes() {
    for (name, profile, want_ref, want_digest) in ACCEPTED {
        let bytes = blob(name);
        assert_eq!(bytes.len(), 381, "{name}: the accepted vectors are 381 B");

        // The fixture really is the accepted vector, before any production code runs.
        assert_eq!(hex_of(&raw_sha256(&bytes)), *want_ref, "{name}: raw ref");
        assert_eq!(
            hex_of(&digest(Domain::Rules, &bytes)),
            *want_digest,
            "{name}: domain digest"
        );

        let declared = DeclaredRules {
            rules_ref: raw_sha256(&bytes),
            rules_digest: digest(Domain::Rules, &bytes),
            profile: *profile,
        };
        let verified = match verify_rules(&bytes, &declared, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(v) => v,
            other => panic!("{name}: {}", describe(&other)),
        };
        assert_eq!(verified.profile, *profile);
        assert_eq!(hex_of(&verified.rules_ref), *want_ref);
        assert_eq!(hex_of(&verified.rules_digest), *want_digest);
    }
    // The two profiles hash differently, so the values are discriminating rather than equal.
    assert_ne!(ACCEPTED[0].2, ACCEPTED[1].2);
    assert_ne!(ACCEPTED[0].3, ACCEPTED[1].3);
}

/// **This is the test that proves a `legacy-v1` blob verifies `Ok`.**
///
/// §7.4 forbids deriving `Corrupt` from "differs from my constants", and the step-4 profile
/// comparison is against the blob's **own** declared profile (`196`), not against
/// `RuleProfile::default_for_new_kb()`. A legacy blob whose `196` says legacy is a supported
/// rule set and must verify: the acceptance criterion is "the blob agrees with the profile the
/// transaction recorded", not "the blob matches the newest profile".
#[test]
fn legacy_profile_verifies_ok_because_the_comparison_is_against_its_own_declared_profile() {
    let bytes = blob("rules.legacy.bin");
    let legacy = DeclaredRules {
        rules_ref: raw_sha256(&bytes),
        rules_digest: digest(Domain::Rules, &bytes),
        profile: RuleProfile::LegacyV1,
    };
    // The exact same bytes, declared as the newer profile, must NOT verify: that is the
    // header contradicting its own blob, which is step 4's `RulesProfileMismatch`.
    let claimed_formal = DeclaredRules {
        profile: RuleProfile::FormalV1,
        ..legacy
    };

    match verify_rules(&bytes, &legacy, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(v) => assert_eq!(v.profile, RuleProfile::LegacyV1),
        other => panic!(
            "a legacy-v1 blob declared as legacy must verify Ok, got {}",
            describe(&other)
        ),
    }
    expect_corrupt(
        verify_rules(&bytes, &claimed_formal, &Limits::V1, &mut Open),
        CorruptReason::RulesProfileMismatch,
        "legacy blob declared as formal",
    );

    // And the default profile is genuinely the other one, so the assertion above is not
    // vacuously true because both agree.
    assert_eq!(RuleProfile::default_for_new_kb(), RuleProfile::FormalV1);
    assert_eq!(RuleProfile::default_for_migration(), RuleProfile::LegacyV1);
}

/// The production encoder and the independent oracle must agree, or one of the two accepted
/// values is describing a different encoding than the crate writes.
#[test]
fn production_encoder_matches_the_oracle_bytes() {
    for (name, profile, want_ref, want_digest) in ACCEPTED {
        let oracle = blob(name);
        let produced = sage_db_format::rules::CanonicalRules::new(*profile)
            .encode(&mut Open)
            .expect("encode");
        assert_eq!(
            hex_of(&produced),
            hex_of(&oracle),
            "{name}: crate encoding must equal the independently written bytes"
        );
        assert_eq!(hex_of(&raw_sha256(&produced)), *want_ref);
        assert_eq!(hex_of(&digest(Domain::Rules, &produced)), *want_digest);
    }
}

#[test]
fn every_fixture_classifies_exactly_as_declared() {
    for (name, class) in CASES {
        let bytes = blob(name);
        let mut ledger = Open;
        let parsed = parse_rules(&bytes, &Limits::V1, &mut ledger);
        let parses = matches!(parsed, FormatOutcome::Ok(_));
        assert_eq!(
            parses,
            !matches!(class, Class::Malformed),
            "{name} ({class:?}): parse result {}",
            describe(&parsed)
        );
        if *class == Class::Malformed {
            expect_corrupt(
                parse_rules(&bytes, &Limits::V1, &mut Open),
                CorruptReason::RulesEncodingInvalid,
                name,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Unsupported: legal bytes that are not this build's template (§7.4's forbidden shortcut)
// ---------------------------------------------------------------------------

/// This is the row §7.4 exists to protect. Each blob is well-formed and its declared hashes
/// match its own bytes, so integrity is proven; what differs is whether *this build* can
/// evaluate the rule set. Reporting `Corrupt` would tell an operator to restore a backup
/// because they need an upgrade.
#[test]
fn legal_but_non_builtin_rule_sets_are_unsupported_never_corrupt() {
    let unsupported = [
        // formal-v1 with rule 12 switched off: a different, legal rule set
        ("rules.formal-rule12-off.bin", RuleProfile::FormalV1),
        // formal-v1 shape with a different relation name string
        (
            "rules.formal-altered-relation-name.bin",
            RuleProfile::FormalV1,
        ),
        // rule 6 scalar cardinality 1 -> 2: legal shape (still a fixed 6-byte block)
        (
            "rules.formal-rule6-subject-count-2.bin",
            RuleProfile::FormalV1,
        ),
        // rule 7 scalar threshold 1 -> 0: legal u8, no stated lower bound beyond the type
        ("rules.formal-v5-min-targets-0.bin", RuleProfile::FormalV1),
    ];
    for (name, profile) in unsupported {
        let bytes = blob(name);
        let declared = declared_for(&bytes, profile);

        // The declared hashes match the bytes, so neither hash step can be what refuses this.
        assert_eq!(raw_sha256(&bytes), declared.rules_ref);
        assert_eq!(digest(Domain::Rules, &bytes), declared.rules_digest);

        match verify_rules(&bytes, &declared, &Limits::V1, &mut Open) {
            FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::RulesNotSupported,
            )) => {}
            other => panic!(
                "{name}: a legal non-builtin rule set must be Unsupported(RulesNotSupported), got {}",
                describe(&other)
            ),
        }
        // And "well-formed" means the parser itself accepts it.
        assert!(
            matches!(
                parse_rules(&bytes, &Limits::V1, &mut Open),
                FormatOutcome::Ok(_)
            ),
            "{name}: must parse"
        );
    }
}

/// A blob whose bytes differ from the built-in template but which claims a profile this build
/// knows must be `Unsupported`, not `Corrupt` — the exact shortcut §7.4 names.
#[test]
fn template_difference_is_not_corruption() {
    let name = "rules.formal-rule12-off.bin";
    let bytes = blob(name);
    let declared = declared_for(&bytes, RuleProfile::FormalV1);

    let outcome = verify_rules(&bytes, &declared, &Limits::V1, &mut Open);
    let described = describe(&outcome);
    for forbidden in [
        "RulesRefHashMismatch",
        "RulesDigestMismatch",
        "RulesProfileMismatch",
        "RulesEncodingInvalid",
    ] {
        assert!(
            !described.contains(forbidden),
            "the blob matches its own declared hashes and is legal; got {described}"
        );
    }
    assert!(described.contains("RulesNotSupported"), "{described}");
}

// ---------------------------------------------------------------------------
// Hash steps keep their own classes and are never collapsed
// ---------------------------------------------------------------------------

#[test]
fn raw_reference_and_domain_digest_are_separate_checks() {
    let bytes = blob("rules.formal.bin");
    let good = declared_for(&bytes, RuleProfile::FormalV1);

    let mut wrong_ref = good;
    wrong_ref.rules_ref[0] ^= 0xFF;
    expect_corrupt(
        verify_rules(&bytes, &wrong_ref, &Limits::V1, &mut Open),
        CorruptReason::RulesRefHashMismatch,
        "wrong rules_ref",
    );

    // Correct raw ref, wrong domain digest: proves step 3 is not implied by step 2.
    let mut wrong_digest = good;
    wrong_digest.rules_digest[31] ^= 0xFF;
    expect_corrupt(
        verify_rules(&bytes, &wrong_digest, &Limits::V1, &mut Open),
        CorruptReason::RulesDigestMismatch,
        "wrong rules_digest",
    );

    // The two functions really differ on the same bytes, so neither can stand in for the other.
    assert_ne!(
        raw_sha256(&bytes),
        digest(Domain::Rules, &bytes),
        "raw and domain hashes are different functions"
    );
}

#[test]
fn declared_profile_must_equal_the_blob_profile() {
    let bytes = blob("rules.formal.bin");
    let wrong = DeclaredRules {
        profile: RuleProfile::LegacyV1,
        ..declared_for(&bytes, RuleProfile::LegacyV1)
    };
    expect_corrupt(
        verify_rules(&bytes, &wrong, &Limits::V1, &mut Open),
        CorruptReason::RulesProfileMismatch,
        "profile disagreement",
    );
}

/// The four corruption classes must be reachable and distinct; a single "rules mismatch" would
/// make a migration look like a damaged disk.
#[test]
fn the_four_corruption_classes_are_distinct() {
    let good_blob = blob("rules.formal.bin");
    let good = declared_for(&good_blob, RuleProfile::FormalV1);

    let ref_bad = DeclaredRules {
        rules_ref: [0x11; 32],
        ..good
    };
    let digest_bad = DeclaredRules {
        rules_digest: [0x22; 32],
        ..good
    };
    let profile_bad = DeclaredRules {
        profile: RuleProfile::LegacyV1,
        ..good
    };
    let malformed = blob("rules.bad.trailing.bin");

    let observed = [
        describe(&verify_rules(&good_blob, &ref_bad, &Limits::V1, &mut Open)),
        describe(&verify_rules(
            &good_blob,
            &digest_bad,
            &Limits::V1,
            &mut Open,
        )),
        describe(&verify_rules(
            &good_blob,
            &profile_bad,
            &Limits::V1,
            &mut Open,
        )),
        describe(&verify_rules(&malformed, &good, &Limits::V1, &mut Open)),
    ];
    for (index, one) in observed.iter().enumerate() {
        for other in &observed[index + 1..] {
            assert_ne!(one, other, "two classes collapsed onto {one}");
        }
    }
    assert!(observed[0].contains("RulesRefHashMismatch"));
    assert!(observed[1].contains("RulesDigestMismatch"));
    assert!(observed[2].contains("RulesProfileMismatch"));
    assert!(observed[3].contains("RulesEncodingInvalid"));
}

// ---------------------------------------------------------------------------
// Refusals and control stay refusals, including on empty input
// ---------------------------------------------------------------------------

#[test]
fn refusals_are_never_reported_as_a_class() {
    let bytes = blob("rules.formal.bin");
    let declared = declared_for(&bytes, RuleProfile::FormalV1);

    match verify_rules(&bytes, &declared, &Limits::V1, &mut NoBytes) {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        other => panic!(
            "zero budget must be Refused(Budget), got {}",
            describe(&other)
        ),
    }
    match verify_rules(&bytes, &declared, &Limits::V1, &mut Halt) {
        FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled)) => {}
        other => panic!(
            "cancellation must be Refused(Stopped), got {}",
            describe(&other)
        ),
    }
    match parse_rules(&bytes, &Limits::V1, &mut NoBytes) {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        other => panic!(
            "parse must also preserve a refusal, got {}",
            describe(&other)
        ),
    }
    match parse_rules(&bytes, &Limits::V1, &mut Halt) {
        FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled)) => {}
        other => panic!("parse must observe cancellation, got {}", describe(&other)),
    }
}

/// Empty input is an operation: the caller's policy is observed before the bytes are judged, and
/// the judgement itself is invalid encoding rather than an empty success.
#[test]
fn empty_input_observes_control_and_is_invalid_encoding() {
    let mut ledger = Halt;
    assert_eq!(
        describe(&parse_rules(&[], &Limits::V1, &mut ledger)),
        describe(&FormatOutcome::<()>::Refused(Refusal::Stopped(
            ControlStop::Cancelled
        ))),
        "an empty blob must still observe cancellation"
    );
    expect_corrupt(
        parse_rules(&[], &Limits::V1, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "empty input",
    );
    let declared = DeclaredRules {
        rules_ref: [0u8; 32],
        rules_digest: [0u8; 32],
        profile: RuleProfile::FormalV1,
    };
    expect_corrupt(
        verify_rules(&[], &declared, &Limits::V1, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "empty input through verify",
    );
}

// ---------------------------------------------------------------------------
// Parsed details: the parse reports what the bytes say, not what the build wishes
// ---------------------------------------------------------------------------

#[test]
fn parse_reports_seven_relations_and_twelve_enforcement_values() {
    // ParsedRules borrows the blob, so the buffer must outlive it.
    let formal_bytes = blob("rules.formal.bin");
    let parsed = match parse_rules(&formal_bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(p) => p,
        other => panic!("{}", describe(&other)),
    };
    assert_eq!(parsed.format_version, 1);
    assert_eq!(parsed.profile_id, 1);
    assert_eq!(parsed.profile_name, "formal-v1");
    assert_eq!(parsed.known_profile(), Some(RuleProfile::FormalV1));
    assert_eq!(parsed.relations.len(), 7);
    for (index, relation) in parsed.relations.iter().enumerate() {
        assert_eq!(relation.id as usize, index, "ascending relation ids");
    }
    assert_eq!(parsed.rules.len(), 12);

    // §5.3: under formal-v1 the three definition relations are target<source (2) and imply
    // dependency; depends-on does neither.
    for relation in &parsed.relations[..3] {
        assert_eq!(relation.layer_rule, 2, "formal-v1 definition layer rule");
        assert_eq!(relation.implies_dependency, 1);
    }
    assert_eq!(
        parsed.relations[3].layer_rule, 0,
        "depends-on is unrestricted"
    );
    assert_eq!(parsed.relations[3].implies_dependency, 0);

    let legacy_bytes = blob("rules.legacy.bin");
    let legacy = match parse_rules(&legacy_bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(p) => p,
        other => panic!("{}", describe(&other)),
    };
    // §5.2: rules 3, 6, 7 and 9 are off under legacy-v1 and on under formal-v1.
    for id in [3u16, 6, 7, 9] {
        assert_eq!(
            parsed.enforcement_of(id),
            Some(sage_db_format::rules::Enforcement::On)
        );
        assert_eq!(
            legacy.enforcement_of(id),
            Some(sage_db_format::rules::Enforcement::Off)
        );
    }
    assert_eq!(
        parsed.enforcement_of(12),
        Some(sage_db_format::rules::Enforcement::On)
    );
}

#[test]
fn malformed_classifications_name_one_broken_rule() {
    for (name, class) in CASES {
        if *class != Class::Malformed {
            continue;
        }
        let bytes = blob(name);
        match parse_rules(&bytes, &Limits::V1, &mut Open) {
            FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RulesEncodingInvalid)) => {}
            other => panic!(
                "{name}: expected invalid encoding, got {}",
                describe(&other)
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// G-1: parameters are part of the rule set (§5.1/§5.2)
//
// Each blob below is a LEGAL encoding whose declared hashes match its own bytes; only a
// parameter VALUE differs from the built-in template. That is a different configuration of
// "what may be derived", so `Ok` would be wrong (it claims "this build supports this rule
// set") and `Corrupt` would also be wrong (the bytes are legal). The only correct verdict is
// Unsupported.
// ---------------------------------------------------------------------------

/// Altered-but-legal parameter values across every §5.2 shape that carries content.
#[test]
fn altered_parameter_values_are_unsupported_not_ok() {
    let cases = [
        // rule 1: R1 relation list [0,1,3] -> [0,1,4]
        "rules.formal-r1-params-altered.bin",
        // rule 10: annotation lattice [0,1,2] -> [0,1,0] (drops `confirmed`)
        "rules.formal-r10-params-altered.bin",
        // rule 7: V5 min_targets 1 -> 2
        "rules.formal-v5-params-altered.bin",
        // rule 11: stable visit order, one byte changed
        "rules.formal-r11-visit-order-altered.bin",
    ];
    for name in cases {
        let bytes = blob(name);
        // The declaration is recomputed from these very bytes, so both hash bindings pass and
        // the verdict can only come from the support comparison.
        let declared = declared_for(&bytes, RuleProfile::FormalV1);
        assert_eq!(raw_sha256(&bytes), declared.rules_ref, "{name}: ref binds");
        assert_eq!(
            digest(Domain::Rules, &bytes),
            declared.rules_digest,
            "{name}: digest binds"
        );

        // It must PARSE: a value difference is not a layout violation.
        assert!(
            matches!(
                parse_rules(&bytes, &Limits::V1, &mut Open),
                FormatOutcome::Ok(_)
            ),
            "{name}: a changed value must still parse"
        );

        let outcome = verify_rules(&bytes, &declared, &Limits::V1, &mut Open);
        let described = describe(&outcome);
        assert!(
            described.contains("RulesNotSupported"),
            "{name}: a different rule set must be Unsupported, got {described}"
        );
        assert!(
            !described.contains("Ok"),
            "{name}: Ok would claim this build supports a rule set it does not"
        );
        assert!(
            !described.contains("RulesEncodingInvalid"),
            "{name}: legal bytes must not be called invalid encoding"
        );
    }

    // Control: the same bytes with the original parameters ARE supported, so the four cases
    // above differ only in the parameter value.
    let control = blob("rules.formal.bin");
    let declared = declared_for(&control, RuleProfile::FormalV1);
    assert!(
        matches!(
            verify_rules(&control, &declared, &Limits::V1, &mut Open),
            FormatOutcome::Ok(_)
        ),
        "the unaltered formal-v1 template must verify Ok"
    );
}

/// The parameter *values* are retained, not just their length: a caller can read them back.
#[test]
fn parsed_rules_retains_parameter_bytes() {
    let bytes = blob("rules.formal.bin");
    let parsed = match parse_rules(&bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(p) => p,
        other => panic!("{}", describe(&other)),
    };
    // rule 1 = `u32 3` then relation ids 0, 1, 3.
    let r1 = parsed.rule(1).expect("rule 1 present");
    assert_eq!(r1.id, 1);
    assert_eq!(r1.enforcement, 1);
    assert_eq!(r1.params.len(), 10);
    assert_eq!(&r1.params[..4], &3u32.to_le_bytes());
    let (pairs, rest) = r1.params[4..].as_chunks::<2>();
    assert!(rest.is_empty());
    assert_eq!(
        pairs
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect::<Vec<_>>(),
        vec![0, 1, 3]
    );

    // rule 11's visit-order string is retained verbatim.
    let r11 = parsed.rule(11).expect("rule 11 present");
    let text = &r11.params[4..];
    assert_eq!(text, b"relation_id,target_id,assertion_id");

    // rule 4 is registered empty and must retain an empty block.
    let r4 = parsed.rule(4).expect("rule 4 present");
    assert!(r4.params.is_empty());
}

/// A parameter block that does not fit its §5.2 shape is CORRUPT, not unsupported: no value
/// assignment could make it legal.
#[test]
fn malformed_parameter_layouts_are_corrupt() {
    for name in [
        // rule 7 declared with 5 bytes instead of 4
        "rules.bad.rule7-length.bin",
        // rule 4 is registered empty but carries two bytes
        "rules.bad.rule4-nonempty.bin",
        // rule 1 lists relation id 7, outside the closed set 0..=6
        "rules.bad.rule1-relation-out-of-domain.bin",
        // a real shape difference: rule 6's declared params_len is 5 (and 7) while the
        // encoded block is 6 bytes. This is length-vs-shape, not a template value.
        "rules.bad.rule6-params-len-5.bin",
        "rules.bad.rule6-params-len-7.bin",
    ] {
        let bytes = blob(name);
        expect_corrupt(
            parse_rules(&bytes, &Limits::V1, &mut Open),
            CorruptReason::RulesEncodingInvalid,
            name,
        );
    }
}

/// G-1 boundary, as the coordinator refined it: a SCALAR parameter value is not a shape.
///
/// §5.2 fixes `subject_count = 1` and `min_targets = 1` for the **formal-v1 template**. Those
/// are template values, so an encoding that carries a different legal value is a different rule
/// set — `Unsupported`, reachable only because the parser no longer treats the builtin value as
/// a structural requirement. A genuine shape violation (a block that cannot fit its layout)
/// stays `Corrupt`.
#[test]
fn scalar_parameter_values_are_unsupported_while_shapes_stay_corrupt() {
    // `subject_count = 2` with `relation = 2`: fixed 6-byte block, so the SHAPE is legal.
    let bytes = blob("rules.formal-rule6-subject-count-2.bin");
    assert_eq!(bytes.len(), 381, "a scalar change does not move the length");
    let declared = declared_for(&bytes, RuleProfile::FormalV1);
    assert!(
        matches!(
            parse_rules(&bytes, &Limits::V1, &mut Open),
            FormatOutcome::Ok(_)
        ),
        "a legal scalar value must parse"
    );
    let outcome = verify_rules(&bytes, &declared, &Limits::V1, &mut Open);
    assert!(
        describe(&outcome).contains("RulesNotSupported"),
        "subject_count 2 must be Unsupported, got {}",
        describe(&outcome)
    );

    // `min_targets = 0`: a legal `u8`, and the document states no lower bound beyond the type.
    let bytes = blob("rules.formal-v5-min-targets-0.bin");
    let declared = declared_for(&bytes, RuleProfile::FormalV1);
    assert!(matches!(
        parse_rules(&bytes, &Limits::V1, &mut Open),
        FormatOutcome::Ok(_)
    ));
    let outcome = verify_rules(&bytes, &declared, &Limits::V1, &mut Open);
    assert!(
        describe(&outcome).contains("RulesNotSupported"),
        "min_targets 0 must be Unsupported, got {}",
        describe(&outcome)
    );

    // The two fixtures above must NOT be the same bytes: a name that asserts a shape difference
    // while carrying a value difference would claim coverage it does not have (t67 caught
    // exactly that). Assert distinctness here so the failure is local to this test.
    let value_only = blob("rules.formal-rule6-subject-count-2.bin");
    for shape_name in [
        "rules.bad.rule6-params-len-5.bin",
        "rules.bad.rule6-params-len-7.bin",
    ] {
        let shape = blob(shape_name);
        assert_ne!(
            value_only, shape,
            "{shape_name}: a shape-difference fixture must not duplicate a value-only fixture"
        );
        // A shape difference is Corrupt, and it is *length* that differs, not the value.
        expect_corrupt(
            parse_rules(&shape, &Limits::V1, &mut Open),
            CorruptReason::RulesEncodingInvalid,
            shape_name,
        );
    }

    // Shape is a different question: a rule 6 block that is not 6 bytes cannot be legal at any
    // value, so it stays Corrupt.
    let mut short = blob("rules.formal.bin");
    let r6: Vec<u8> = [
        &6u16.to_le_bytes()[..],
        &1u16.to_le_bytes()[..],
        &6u32.to_le_bytes()[..],
        &1u32.to_le_bytes()[..],
        &2u16.to_le_bytes()[..],
    ]
    .concat();
    let at = short
        .windows(r6.len())
        .position(|w| w == r6)
        .expect("rule 6 block present");
    short[at + 4..at + 8].copy_from_slice(&5u32.to_le_bytes());
    expect_corrupt(
        parse_rules(&short, &Limits::V1, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "rule 6 block length 5",
    );
}

// ---------------------------------------------------------------------------
// Ceilings are LIMIT, and identical bytes can be both legal and over-allowance
//
// §9: FORMAT_LIMIT is "structurally valid but over a declared ceiling". The same blob is `Ok`
// under the normative `Limits::V1` and over-allowance under a tightened one, so a ceiling
// breach cannot be corruption — and equally, tightening a ceiling must never convert an
// encoding fault into a limit.
// ---------------------------------------------------------------------------

fn tight_string(bytes: u32) -> Limits {
    let mut limits = Limits::V1;
    limits.max_string_bytes = bytes;
    limits
}

#[test]
fn string_ceilings_are_limit_not_corruption() {
    let bytes = blob("rules.formal.bin");

    // The same bytes are legal under the normative ceilings.
    assert!(matches!(
        parse_rules(&bytes, &Limits::V1, &mut Open),
        FormatOutcome::Ok(_)
    ));

    // Below the 9-byte profile name: the limit must survive as Limit, not become Corrupt.
    match parse_rules(&bytes, &tight_string(4), &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::String { tag: None, .. })) => {}
        other => panic!(
            "profile-name ceiling breach must be Limit, got {}",
            describe(&other)
        ),
    }

    // Low enough to bite rule 11's 34-byte visit order INSIDE a parameter block: this is the
    // path that used to erase the limit into `Malformed`, so it is asserted separately.
    match parse_rules(&bytes, &tight_string(16), &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::String { tag: None, .. })) => {}
        other => panic!(
            "a ceiling breach inside rule 11's params must be Limit, got {}",
            describe(&other)
        ),
    }
}

#[test]
fn list_ceilings_are_consulted_before_iteration() {
    // The D10 formal-v1 R1 list holds 3 relation ids. Under `max_list_items = 2` the very same
    // legal bytes are over-allowance, so this must be Limit — and it must be decided before the
    // loop, not discovered while walking it.
    let bytes = blob("rules.formal.bin");
    let mut limits = Limits::V1;
    limits.max_list_items = 2;
    match parse_rules(&bytes, &limits, &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::ListItems {
            tag: None,
            count,
            max,
        })) => {
            assert_eq!(count, 3, "the R1 list length is 3");
            assert_eq!(max, 2);
        }
        other => panic!(
            "list ceiling breach must be Limit(ListItems), got {}",
            describe(&other)
        ),
    }

    // A ceiling that admits the blob's LARGEST list leaves it legal. Four, not three: rules 8
    // and 9 (V6) carry four relation ids, so a limit of 3 would refuse those instead.
    let mut roomy = Limits::V1;
    roomy.max_list_items = 4;
    assert!(matches!(
        parse_rules(&bytes, &roomy, &mut Open),
        FormatOutcome::Ok(_)
    ));
}

/// Tightening a ceiling must not launder corruption into a limit, or a limit into corruption.
#[test]
fn config_independence_keeps_corruption_corrupt() {
    // Invalid UTF-8 in the profile name: §1 forbids non-UTF-8. The ceiling must be generous
    // enough to ADMIT the 9-byte name, because a reader cannot validate the encoding of bytes
    // it refused to read — under a tighter ceiling the length refusal legitimately comes first.
    let limits = Limits::V1;
    let mut bad_utf8 = blob("rules.formal.bin");
    let at = bad_utf8
        .windows(9)
        .position(|w| w == b"formal-v1")
        .expect("profile name");
    bad_utf8[at] = 0xFF;
    expect_corrupt(
        parse_rules(&bad_utf8, &limits, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "invalid UTF-8",
    );

    // A NUL byte in a string: forbidden by the canonical string form.
    let mut nul = blob("rules.formal.bin");
    nul[at] = 0x00;
    expect_corrupt(
        parse_rules(&nul, &limits, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "NUL in string",
    );

    // A declared length that disagrees with the block is a shape fault, not a ceiling.
    expect_corrupt(
        parse_rules(
            &blob("rules.bad.rule6-params-len-7.bin"),
            &limits,
            &mut Open,
        ),
        CorruptReason::RulesEncodingInvalid,
        "params_len beyond block",
    );

    // Truncation.
    let mut cut = blob("rules.formal.bin");
    cut.truncate(cut.len() - 10);
    expect_corrupt(
        parse_rules(&cut, &limits, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "truncated",
    );
}

/// An empty name is legal shape (§8.1: only `encoded_len = 0` is illegal, not an empty string),
/// so a zero-length name must parse and then be a *different* rule set — never corruption.
#[test]
fn zero_length_names_are_legal_shape_but_a_different_rule_set() {
    fn replace_with_zero_length(bytes: &[u8], needle: &[u8]) -> Vec<u8> {
        let prefix = (needle.len() as u32).to_le_bytes();
        let mut pattern = prefix.to_vec();
        pattern.extend_from_slice(needle);
        let at = bytes
            .windows(pattern.len())
            .position(|w| w == pattern)
            .expect("field present");
        let mut out = Vec::new();
        out.extend_from_slice(&bytes[..at]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&bytes[at + pattern.len()..]);
        out
    }

    let base = blob("rules.formal.bin");
    for (name, needle) in [
        ("profile name", &b"formal-v1"[..]),
        ("relation name", b"is-a"),
    ] {
        let zero = replace_with_zero_length(&base, needle);
        assert_eq!(
            zero.len(),
            base.len() - needle.len(),
            "{name}: only the name shrank"
        );
        match parse_rules(&zero, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(_) => {}
            other => panic!(
                "zero-length {name} is legal shape, got {}",
                describe(&other)
            ),
        }
        let declared = declared_for(&zero, RuleProfile::FormalV1);
        let outcome = verify_rules(&zero, &declared, &Limits::V1, &mut Open);
        assert!(
            describe(&outcome).contains("RulesNotSupported"),
            "zero-length {name} is a different rule set, got {}",
            describe(&outcome)
        );
    }

    // The distinction that must not be lost: a zero-length params BLOCK is illegal
    // (`encoded_len = 0`), unlike an empty string inside a valid block.
    let mut zero_block = base.clone();
    let header: Vec<u8> = [
        &11u16.to_le_bytes()[..],
        &1u16.to_le_bytes()[..],
        &38u32.to_le_bytes()[..],
    ]
    .concat();
    let at = zero_block
        .windows(header.len())
        .position(|w| w == header)
        .expect("rule 11 header");
    zero_block.splice(at + 4..at + 8, 38u32.to_le_bytes());
    zero_block.drain(at + 8..at + 8 + 38);
    zero_block.splice(at + 4..at + 8, 0u32.to_le_bytes());
    expect_corrupt(
        parse_rules(&zero_block, &Limits::V1, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "zero-length params block",
    );
}

/// The final G-1 domain mismatch (REPORT-15): rule 10 shares the `u32 count + u16[]` SHAPE with
/// the relation-id lists but has **no relation-id domain** — it is the annotation lattice
/// (`bottom < contested < hypothetical < confirmed`), and §5.2 states no element domain for it.
///
/// So an element past `RELATION_COUNT` is a legal shape carrying a different value: it must reach
/// the support comparison and come back `Unsupported`. Rule 8 is the positive contrast — it IS a
/// relation-id list, so the same element value there is malformed.
#[test]
fn rule_10_lattice_has_no_relation_domain_but_rule_8_does() {
    for name in [
        "rules.formal-rule10-lattice-7.bin",
        "rules.formal-rule10-lattice-100.bin",
    ] {
        let bytes = blob(name);
        // Declarations recomputed from the blob's own bytes, so both bindings pass and the
        // verdict can only come from the support comparison — not from a hash failing first.
        let declared = declared_for(&bytes, RuleProfile::FormalV1);
        assert_eq!(raw_sha256(&bytes), declared.rules_ref);
        assert_eq!(digest(Domain::Rules, &bytes), declared.rules_digest);

        assert!(
            matches!(
                parse_rules(&bytes, &Limits::V1, &mut Open),
                FormatOutcome::Ok(_)
            ),
            "{name}: a lattice entry past the relation count is a legal shape"
        );
        let outcome = verify_rules(&bytes, &declared, &Limits::V1, &mut Open);
        let described = describe(&outcome);
        assert!(
            described.contains("RulesNotSupported"),
            "{name}: must reach support comparison and be Unsupported, got {described}"
        );
        assert!(
            !described.contains("RulesEncodingInvalid"),
            "{name}: the relation domain must NOT be applied to rule 10"
        );
    }

    // Contrast: rule 8 is V6's acyclicity list and its elements ARE relation ids.
    let bytes = blob("rules.bad.rule8-relation-out-of-domain.bin");
    expect_corrupt(
        parse_rules(&bytes, &Limits::V1, &mut Open),
        CorruptReason::RulesEncodingInvalid,
        "rule 8 relation id 7",
    );

    // And the two D10 profiles remain the supported pair.
    for (name, profile) in [
        ("rules.formal.bin", RuleProfile::FormalV1),
        ("rules.legacy.bin", RuleProfile::LegacyV1),
    ] {
        let bytes = blob(name);
        let declared = declared_for(&bytes, profile);
        assert!(
            matches!(
                verify_rules(&bytes, &declared, &Limits::V1, &mut Open),
                FormatOutcome::Ok(_)
            ),
            "{name}: accepted profile must still verify Ok"
        );
    }
}
