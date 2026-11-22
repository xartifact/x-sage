//! Full-record fixtures: builders for complete persistent records, independent of any encoder.
//!
//! The point of this module is stated by the phase contract and by P0's own scoping note: the P0
//! `TermMeta` and `AssertionRecord` are **inference projections**, not complete records. A harness
//! that only compares graph rows cannot notice a lost body, a dropped audit field, or a missing
//! legacy provenance value. So the fixtures here build *whole* records — every field the persisted
//! type declares — and the assertions built on them compare whole records.
//!
//! Two rules keep this honest:
//!
//! * **No encoder in the loop.** These builders construct values directly; expected bytes, when a
//!   test needs them, must come from somewhere other than the encoder under test.
//! * **Fields are set to values that would be lost if forgotten.** Bodies are non-empty, audit
//!   fields are `Some`, `extensions` are populated, legacy provenance is present. A fixture with
//!   empty or default values would pass even if the encoder dropped the field entirely.

use sage_db_types::confidence::Confidence;
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull as PersistAssertion, CommonFields, Extensions, RecordKey,
    RecordKind, TermRecord as PersistTerm, TermSource,
};
use sage_db_types::relation::RelationId;

/// Values that are easy to lose and therefore worth setting explicitly.
///
/// Each constant is non-default in a way an encoder could plausibly drop: a body that is not empty,
/// a recorded actor, a reason, an updated timestamp, a populated extension map.
pub const BODY_BYTES: &[u8] = b"# Term body\n\nA complete record, not a projection.\n";

/// Build common fields with every optional present.
#[must_use]
pub fn common_full() -> CommonFields {
    CommonFields {
        recorded_by: Some("reviewer@example".to_owned()),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some("initial import".to_owned()),
        created_at: 1_700_000_000_000_000,
        updated_at: Some(1_700_000_100_000_000),
    }
}

/// Build common fields representing a legacy import: declared origin, no verified actor.
#[must_use]
pub fn common_legacy() -> CommonFields {
    CommonFields {
        recorded_by: None,
        actor_origin: ActorOrigin::LegacyDeclared,
        reason: None,
        created_at: 1_600_000_000_000_000,
        updated_at: None,
    }
}

/// Extensions with two entries, deliberately not in sorted order in declaration terms.
#[must_use]
pub fn extensions_full() -> Extensions {
    // `from_canonical` requires strictly ascending keys, so the list is sorted here rather than
    // relying on insertion order. `legacy.dimensions` < `legacy.provider` in byte order.
    Extensions::from_canonical(vec![
        ("legacy.dimensions".to_owned(), b"1536".to_vec()),
        ("legacy.provider".to_owned(), b"acme".to_vec()),
    ])
    .expect("canonical extensions")
}

/// A complete active term: **every** field the persisted type declares is populated.
///
/// The legacy fields and `extensions` are present on purpose. They are exactly the fields an
/// encoder forgets, and the ones the P0 projections do not carry at all.
#[must_use]
pub fn term_full() -> PersistTerm {
    PersistTerm {
        key: RecordKey::term(1),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: "fixture-term".to_owned(),
        atom: "A complete term used to prove nothing is dropped".to_owned(),
        layer: SemanticLayer::L1,
        domains: vec!["product".to_owned(), "testing".to_owned()],
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["fixture".to_owned(), "complete-record".to_owned()],
        body_ref: {
            let mut r = [0u8; 32];
            r[0] = 0xB0;
            r
        },
        common: common_full(),
        legacy_id: Some("legacy-term-1".to_owned()),
        legacy_source_path: Some("concepts/L1/fixture-term.md".to_owned()),
        legacy_content_digest: Some([0xC1u8; 32]),
        legacy_source_file_digest: Some([0xC2u8; 32]),
        legacy_source_revision: Some([0xC3u8; 32]),
        extensions: extensions_full(),
        changed_fields: None,
    }
}

/// The same term after a revision: higher revision, changed atom, `changed_fields` recorded.
#[must_use]
pub fn term_revised() -> PersistTerm {
    let mut t = term_full();
    t.revision = RecordRevision::new(2).expect("revision 2");
    t.atom = "A revised atom".to_owned();
    t.common.updated_at = Some(1_700_000_200_000_000);
    // `changed_fields` lists the tags this revision changed: atom (17) and updated_at (9).
    t.changed_fields = Some(vec![9, 17]);
    t
}

/// A complete assertion with its own identity, revision, actor and declaration order.
#[must_use]
pub fn assertion_full() -> PersistAssertion {
    PersistAssertion {
        key: RecordKey::new(RecordKind::Assertion, 1),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(1),
        relation: RelationId::IsA,
        target: RecordKey::term(2),
        confidence: Confidence::Hypothetical,
        declaration_order: 7,
        common: common_full(),
        legacy_id: Some("legacy-assertion-1".to_owned()),
        legacy_content_digest: Some([0xD1u8; 32]),
        extensions: extensions_full(),
        changed_fields: None,
    }
}

/// A legacy-imported assertion: declared origin only, no verified actor.
#[must_use]
pub fn assertion_legacy() -> PersistAssertion {
    let mut a = assertion_full();
    a.common = common_legacy();
    a.legacy_id = Some("legacy-assertion-2".to_owned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full-record fixtures must actually be full: every optional present.
    #[test]
    fn fixtures_populate_the_fields_easy_to_lose() {
        let term = term_full();
        assert!(term.common.recorded_by.is_some(), "verified actor present");
        assert!(term.common.reason.is_some(), "reason present");
        assert!(term.common.updated_at.is_some(), "updated_at present");
        assert!(!term.slug.is_empty());
        assert!(!term.atom.is_empty());
        assert!(!term.domains.is_empty());
        assert!(!term.triggers.is_empty());
        assert_ne!(term.body_ref, [0u8; 32], "body reference is set");
        assert!(!term.retired);

        let assertion = assertion_full();
        assert!(assertion.common.recorded_by.is_some());
        assert_eq!(assertion.declaration_order, 7);
        assert_eq!(assertion.revision, RecordRevision::FIRST);
    }

    /// The legacy fixtures must differ from the full ones, so a test can tell them apart.
    #[test]
    fn legacy_fixtures_are_distinguishable_from_authenticated_ones() {
        let full = term_full();
        let mut legacy = term_full();
        legacy.common = common_legacy();
        assert_ne!(full.common, legacy.common);
        assert!(legacy.common.recorded_by.is_none());
        assert_eq!(legacy.common.actor_origin, ActorOrigin::LegacyDeclared);
    }

    /// A revision must differ from its predecessor in more than one observable way.
    #[test]
    fn revised_term_differs_from_the_first_revision() {
        let first = term_full();
        let second = term_revised();
        assert_ne!(first.revision, second.revision);
        assert_ne!(first.atom, second.atom);
        assert_ne!(first.common.updated_at, second.common.updated_at);
    }

    /// Extensions must be non-empty and carry both declared keys.
    #[test]
    fn extensions_carry_populated_values() {
        let ext = extensions_full();
        assert_eq!(ext.get("legacy.provider"), Some(b"acme".as_slice()));
        assert_eq!(ext.get("legacy.dimensions"), Some(b"1536".as_slice()));
    }

    /// Bodies must be non-empty; an empty body fixture would hide a dropped blob reference.
    #[test]
    fn body_bytes_are_not_empty() {
        assert!(!BODY_BYTES.is_empty());
    }
}

#[cfg(test)]
mod loss_tests {
    use super::*;

    /// A "field was dropped" check that would pass on projections must fail here.
    ///
    /// The P0 `TermMeta` carries only id + layer and `AssertionRecord` only
    /// id/revision/endpoints/confidence. Both would compare equal for two terms that differ in
    /// body, audit fields, legacy provenance or extensions. This test asserts the fixture
    /// distinguishes exactly those, so a projection-based comparison is provably insufficient.
    #[test]
    fn projection_equality_would_miss_real_record_differences() {
        let a = term_full();
        let mut b = term_full();

        // Same key and layer: a projection cannot tell them apart.
        assert_eq!(a.key, b.key);
        assert_eq!(a.layer, b.layer);

        // Differ in every field the projection omits.
        b.atom = "different".to_owned();
        b.slug = "different-slug".to_owned();
        b.body_ref[0] = 0x99;
        b.common.recorded_by = Some("someone-else".to_owned());
        b.legacy_id = None;
        b.extensions = Extensions::new();

        assert_ne!(a.atom, b.atom);
        assert_ne!(a.body_ref, b.body_ref);
        assert_ne!(a.common.recorded_by, b.common.recorded_by);
        assert_ne!(a.legacy_id, b.legacy_id);
        assert!(b.extensions.entries().is_empty() && !a.extensions.entries().is_empty());
    }

    /// The assertion fixture must differ from the term fixture in kind, so a test cannot compare
    /// across record types by accident.
    #[test]
    fn assertion_and_term_keys_have_distinct_kinds() {
        let t = term_full();
        let a = assertion_full();
        assert_ne!(t.key.kind(), a.key.kind());
    }

    /// Legacy provenance must survive as `Some`, not be silently normalised to absent.
    #[test]
    fn legacy_provenance_is_present_on_the_full_fixtures() {
        let t = term_full();
        assert!(t.legacy_id.is_some());
        assert!(t.legacy_source_path.is_some());
        assert!(t.legacy_content_digest.is_some());
        assert!(t.legacy_source_file_digest.is_some());
        assert!(t.legacy_source_revision.is_some());
    }

    /// `changed_fields` on a revision must name the tags that actually changed.
    #[test]
    fn revised_fixture_changed_fields_match_the_difference() {
        let first = term_full();
        let second = term_revised();
        assert_eq!(second.changed_fields, Some(vec![9, 17]));
        // 17 is `atom`, 9 is `updated_at`: both really differ.
        assert_ne!(first.atom, second.atom);
        assert_ne!(first.common.updated_at, second.common.updated_at);
    }
}
