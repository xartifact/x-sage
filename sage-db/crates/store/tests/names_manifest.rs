//! Manifest file names: the store's renderer and classifier must agree with the format's grammar.
//!
//! The defect this file exists for: `EntryName::Manifest` rendered `v1-<id>.sgm` while
//! `validate_manifest_name` rejected leading zeros, so the store refused the canonical `v000001-...` that
//! D10 uses and accepted `v1-...` that format refuses. It would have blocked every real CURRENT commit.
//! There is now one grammar (format's), and these tests pin both directions.

use sage_db_store::names::{EntryName, Id128, is_valid_entry_name};
use sage_db_types::ledger::{BudgetError, ControlStop, SharedLedger};

/// A ledger that admits everything: the bounded renderer needs one, and these tests are about the bytes it emits.
struct Open;

impl SharedLedger for Open {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

fn id() -> Id128 {
    Id128::from_bytes([
        0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1,
        0xf0,
    ])
}

/// Every rendered manifest name must be accepted by the format's grammar.
#[test]
fn rendered_manifest_names_are_accepted_by_the_format_grammar() {
    for version in [1u64, 7, 99_999, 100_000, u64::MAX] {
        let name = EntryName::Manifest { version, id: id() }.to_file_name();
        assert!(
            sage_db_format::file::is_valid_manifest_name(name.as_bytes()),
            "format must accept our own rendering {name:?} (version {version})"
        );
    }
}

/// §2.4: at least six digits, zero-padded.
#[test]
fn small_versions_are_zero_padded_to_six_digits() {
    let name = EntryName::Manifest {
        version: 1,
        id: id(),
    }
    .to_file_name();
    assert!(
        name.starts_with("v000001-"),
        "version 1 must render as v000001, got {name:?}"
    );
    let name7 = EntryName::Manifest {
        version: 7,
        id: id(),
    }
    .to_file_name();
    assert!(name7.starts_with("v000007-"), "got {name7:?}");
}

/// The canonical D10 name must classify as valid — this is the direction the old code refused.
#[test]
fn the_canonical_d10_name_is_accepted() {
    let name = "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm";
    assert!(
        sage_db_format::file::is_valid_manifest_name(name.as_bytes()),
        "format accepts the D10 name"
    );
    // The store's classifier delegates to the same grammar, so it must agree.
    assert!(
        is_valid_entry_name(name),
        "the store's classifier must accept the canonical D10 name"
    );
}

/// Non-padded and otherwise malformed names must be refused in both directions.
#[test]
fn malformed_manifest_names_are_refused() {
    for bad in [
        "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",     // no padding
        "v00000-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm", // five digits
        "v000000-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm", // version 0
        "v000001-0F1E2D3C4B5A69788796A5B4C3D2E1F0.sgm", // uppercase
        "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f.sgm", // short hex
        "../v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm", // traversal
    ] {
        assert!(
            !sage_db_format::file::is_valid_manifest_name(bad.as_bytes()),
            "format must refuse {bad:?}"
        );
        assert!(
            !is_valid_entry_name(bad),
            "the store's classifier must refuse {bad:?}"
        );
    }
}

/// A version beyond the accepted width must not silently render something the grammar refuses.
#[test]
fn an_oversized_version_does_not_render_a_valid_looking_name() {
    let name = EntryName::Manifest {
        version: u64::MAX,
        id: id(),
    }
    .to_file_name();
    // u64::MAX is 20 digits, exactly the format's maximum, so it is legal.
    assert!(
        sage_db_format::file::is_valid_manifest_name(name.as_bytes()),
        "u64::MAX is 20 digits, within the grammar's 20: {name:?}"
    );
}

/// Round-trip: rendering then classifying must agree, for every version.
///
/// t67's property. A renderer that can emit a name its own parser rejects is the same class of defect as an
/// encoder emitting bytes its decoder refuses.
#[test]
fn render_then_classify_round_trips_for_every_version() {
    for version in [1u64, 7, 99_999, 100_000, u64::MAX] {
        let name = EntryName::Manifest { version, id: id() }.to_file_name();
        assert!(
            is_valid_entry_name(&name),
            "classify(render({version})) must be accepted, got {name:?}"
        );
        // And the version survives the round trip at its width.
        let digits = name
            .strip_prefix('v')
            .and_then(|r| r.split_once('-'))
            .map(|(v, _)| v)
            .expect("shape");
        assert_eq!(
            digits.parse::<u64>().expect("digits"),
            version,
            "the numeric segment must parse back to the same version"
        );
        assert!(digits.len() >= 6, "at least six digits: {digits:?}");
    }
}

/// The exact literal from the CURRENT vector must classify as a manifest name.
#[test]
fn the_current_vector_name_is_recognised_as_a_manifest() {
    let name = "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm";
    assert!(is_valid_entry_name(name));
    // And it is not mistaken for another family.
    assert!(!sage_db_format::manifest::is_tx_file_name(name));
}

/// The walk classifier must agree with the entry-name classifier — one grammar, two consumers.
///
/// The defect this pins: `classify_walked` had its own inline grammar with `!starts_with('0')`, so it rejected
/// every padded name the renderer produces (versions 1..99999, genesis included) and accepted the unpadded form
/// format refuses. The earlier tests passed because they exercised `is_valid_entry_name`, not this entry point.
#[test]
fn classify_walked_agrees_with_the_entry_name_classifier() {
    use sage_db_store::names::{WalkedName, classify_walked};

    for version in [1u64, 7, 99_999, 100_000, u64::MAX] {
        let name = EntryName::Manifest { version, id: id() }.to_file_name();
        assert!(
            is_valid_entry_name(&name),
            "render({version}) must be a valid name"
        );
        match classify_walked(&name) {
            WalkedName::Manifest { version: seen } => {
                assert_eq!(
                    seen.parse::<u64>().expect("digits"),
                    version,
                    "classify_walked must recover the same version for {name:?}"
                );
            }
            other => panic!("classify_walked must recognise {name:?} as Manifest, got {other:?}"),
        }
    }
}

#[test]
fn classify_walked_refuses_the_same_malformed_names_as_the_grammar() {
    use sage_db_store::names::{WalkedName, classify_walked};

    for bad in [
        "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",      // unpadded
        "v00001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",  // five digits
        "v000000-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm", // version zero
        "v000001-0F1E2D3C4B5A69788796A5B4C3D2E1F0.sgm", // uppercase hex
        "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f.sgm",  // short hex
    ] {
        assert!(
            matches!(classify_walked(bad), WalkedName::Unrecognised(_)),
            "classify_walked must refuse {bad:?}"
        );
        assert!(
            !is_valid_entry_name(bad),
            "and so must the entry classifier"
        );
    }
}

#[test]
fn classify_walked_recognises_the_canonical_d10_name() {
    use sage_db_store::names::{WalkedName, classify_walked};
    let name = "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm";
    match classify_walked(name) {
        WalkedName::Manifest { version } => assert_eq!(version, "000001"),
        other => panic!("the D10 name must classify as Manifest, got {other:?}"),
    }
}

/// The bounded renderer must produce byte-identical names to the canonical emitter.
///
/// Two renderers exist for one grammar: `to_file_name` (infallible, for callers off the commit path) and
/// `render_name_bounded` (admits capacity, so it can refuse). They must agree exactly, or the commit path would
/// publish a name the classifier rejects — which is precisely what happened when the bounded path hex-encoded an
/// already-hex string.
#[test]
fn the_bounded_renderer_matches_the_canonical_emitter() {
    use sage_db_store::names::render_name_bounded;

    let id = id();
    for name in [
        EntryName::Root(sage_db_store::names::RootName::Current),
        EntryName::Blob([0x5A; 32]),
        EntryName::Transaction(id),
        EntryName::Manifest { version: 1, id },
        EntryName::Manifest {
            version: u64::MAX,
            id,
        },
        EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Tx,
            id,
        },
    ] {
        let canonical = name.to_file_name();
        let bounded = render_name_bounded(name, &mut Open).expect("bounded render");
        assert_eq!(canonical, bounded, "renderers must agree for {name:?}");
    }
}
