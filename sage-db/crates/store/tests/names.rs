//! Implementation tests for typed names and path containment (P1-3, store owner).
//!
//! These are the store's own unit tests, not the independent harness. They assert the properties
//! the accepted S2 contract puts on names: a caller cannot build a traversal, an absolute path, a
//! separator-bearing name, or an out-of-grammar file name; and the renderers are total and
//! canonical.

use sage_db_store::names::{
    EntryName, FAMILIES, Family, Id128, MAX_NAME_BYTES, RootName, StagingKind, encode_id128,
    is_valid_entry_name,
};

#[test]
fn families_are_a_closed_stable_set() {
    // The family list is used by capability checks; it must be complete and stable.
    // `Family` is the seam's single declaration; `FAMILIES` is the canonical order.
    let names: Vec<&str> = FAMILIES.iter().map(|f| f.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "manifests",
            "transactions",
            "blobs",
            "checkpoints",
            "staging",
            "cache"
        ]
    );
    // No duplicates: two families sharing a directory would make publication ambiguous.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len());
}

#[test]
fn id128_renders_as_32_lowercase_hex() {
    let id = Id128::from_bytes([
        0x00, 0x01, 0x0a, 0x0f, 0x10, 0x7f, 0x80, 0xff, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22,
        0x33,
    ]);
    let rendered = encode_id128(&id);
    assert_eq!(rendered.len(), 32);
    let s = std::str::from_utf8(&rendered).expect("ASCII hex");
    assert_eq!(s, "00010a0f107f80ffdeadbeef00112233");
    assert!(
        s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

#[test]
fn root_names_are_exactly_the_three_expected() {
    assert_eq!(RootName::Current.as_str(), "CURRENT");
    assert_eq!(RootName::CurrentTmp.as_str(), "CURRENT.tmp");
    assert_eq!(RootName::Lock.as_str(), "LOCK");
    // `CURRENT.tmp` is a FIXED name, not per-attempt: the format forbids multiple `.tmp` names.
    assert!(!RootName::CurrentTmp.as_str().contains("tmp-"));
}

#[test]
fn entry_names_render_canonically_and_round_trip_through_the_validator() {
    let id = Id128::from_bytes([0xab; 16]);
    let cases = [
        EntryName::Blob([0x11; 32]),
        EntryName::Transaction(id),
        EntryName::Manifest { version: 7, id },
        EntryName::Checkpoint(id),
        EntryName::Staging {
            kind: StagingKind::Blob,
            id,
        },
        EntryName::Staging {
            kind: StagingKind::Tx,
            id,
        },
        EntryName::Staging {
            kind: StagingKind::Manifest,
            id,
        },
        EntryName::Staging {
            kind: StagingKind::Current,
            id,
        },
        EntryName::Root(RootName::Current),
        EntryName::Root(RootName::CurrentTmp),
        EntryName::Root(RootName::Lock),
    ];
    for entry in cases {
        let name = entry.to_file_name();
        assert!(
            is_valid_entry_name(&name),
            "renderer produced a name its own validator rejects: {name}"
        );
        assert!(name.len() <= MAX_NAME_BYTES, "over the 64-byte cap: {name}");
        // The relative path is family-qualified exactly when the entry has a family.
        let rel = entry.to_relative_path();
        match entry.family() {
            None => assert_eq!(rel, name),
            Some(family) => assert_eq!(rel, format!("{}/{}", family.as_str(), name)),
        }
        // A rendered name is a SINGLE segment: no separators, no traversal.
        assert!(!name.contains('/') && !name.contains('\\') && !name.contains('\0'));
        assert_ne!(name, ".");
        assert_ne!(name, "..");
    }
}

#[test]
fn existing_family_assignment_is_total() {
    let id = Id128::from_bytes([0; 16]);
    assert_eq!(EntryName::Blob([0; 32]).family(), Some(Family::Blobs));
    assert_eq!(
        EntryName::Transaction(id).family(),
        Some(Family::Transactions)
    );
    assert_eq!(
        EntryName::Manifest { version: 1, id }.family(),
        Some(Family::Manifests)
    );
    assert_eq!(
        EntryName::Checkpoint(id).family(),
        Some(Family::Checkpoints)
    );
    assert_eq!(
        EntryName::Staging {
            kind: StagingKind::Blob,
            id
        }
        .family(),
        Some(Family::Staging)
    );
    assert_eq!(EntryName::Root(RootName::Current).family(), None);
}

#[test]
fn traversal_and_separator_names_are_refused() {
    // Every one of these would escape the KB directory or address another family.
    let hostile = [
        "..",
        ".",
        "../blobs/abc",
        "blobs/../CURRENT",
        "/etc/passwd",
        "a/b",
        "a\\b",
        "CURRENT\0",
        "tx-../../../etc/passwd",
        "v1-../x.sgm",
        "",
        "   ",
        "CURRENT.",
        "-CURRENT",
    ];
    for name in hostile {
        assert!(
            !is_valid_entry_name(name),
            "must refuse hostile name {name:?}"
        );
    }
}

#[test]
fn out_of_grammar_names_are_refused() {
    let hex32 = "0".repeat(32);
    let hex64 = "0".repeat(64);
    let good = [
        format!("tx-{hex32}.sgl"),
        format!("cp-{hex32}.sgb"),
        format!("v000001-{hex32}.sgm"),
        format!("blob-{hex32}.part"),
        // A blob name is 64 hex characters: the hex form of a 32-BYTE digest.
        hex64.clone(),
        "CURRENT".to_owned(),
        "CURRENT.tmp".to_owned(),
        "LOCK".to_owned(),
    ];
    for name in &good {
        assert!(is_valid_entry_name(name), "should accept {name:?}");
    }

    let bad = [
        // uppercase hex is not canonical
        format!("tx-{}.sgl", "A".repeat(32)),
        // wrong suffix
        format!("tx-{hex32}.sgx"),
        // §2.4: fewer than six digits is malformed
        format!("v1-{hex32}.sgm"),
        format!("v00001-{hex32}.sgm"),
        // §2.4: an all-zero version segment does not exist
        format!("v000000-{hex32}.sgm"),
        // §2.4: at most twenty digits
        format!("v{}-{hex32}.sgm", "1".repeat(21)),
        // manifest without a version
        format!("v-{hex32}.sgm"),
        // staging kind not in the closed set
        format!("blobx-{hex32}.part"),
        // 31 hex is not 32
        format!("tx-{}.sgl", "0".repeat(31)),
        // over the 64-byte cap
        "x".repeat(65),
        // a family directory name is not an entry
        "manifests".to_owned(),
    ];
    for name in &bad {
        assert!(!is_valid_entry_name(name), "should refuse {name:?}");
    }
}

#[test]
fn names_at_the_cap_boundary_are_handled() {
    // The longest canonical name is `manifest-<32 hex>.part` (40) and `v20digits-<32>.sgm`;
    // both are under the cap, and a 64-byte name is accepted only if it is a real grammar.
    assert_eq!(MAX_NAME_BYTES, 64);
    // Longest canonical manifest name: "v" + 20-digit version + "-" + 32 hex + ".sgm" = 58 bytes.
    let long_version = "12345678901234567890";
    let max_manifest = format!("v{long_version}-{}.sgm", "0".repeat(32));
    assert_eq!(max_manifest.len(), 58);
    assert!(max_manifest.len() <= MAX_NAME_BYTES);
    assert!(
        is_valid_entry_name(&max_manifest),
        "maximal manifest name must be valid"
    );
    // One byte over the cap is refused even if the grammar would otherwise match.
    let too_long = format!("v{long_version}-{}.sgm", "0".repeat(40));
    assert!(too_long.len() > MAX_NAME_BYTES);
    assert!(!is_valid_entry_name(&too_long));
}

#[test]
fn blob_names_are_exactly_64_hex_after_render() {
    let entry = EntryName::Blob([0x0f; 32]);
    let name = entry.to_file_name();
    assert_eq!(name.len(), 64);
    assert_eq!(name, "0f".repeat(32));
    assert!(is_valid_entry_name(&name));
}

#[test]
fn walked_name_classifier_is_total_and_never_fails() {
    use sage_db_store::names::{WalkedName, classify_walked};

    // Engine-generated names classify by kind, so an orphan report can group them.
    assert!(matches!(
        classify_walked("CURRENT"),
        WalkedName::Root(RootName::Current)
    ));
    assert!(matches!(
        classify_walked("CURRENT.tmp"),
        WalkedName::Root(RootName::CurrentTmp)
    ));
    assert!(matches!(
        classify_walked("LOCK"),
        WalkedName::Root(RootName::Lock)
    ));
    // §2.4: at least six digits, zero-padded. `v42-...` is malformed; `v000042-...` is canonical.
    match classify_walked(&format!("v000042-{}.sgm", "a".repeat(32))) {
        WalkedName::Manifest { version } => assert_eq!(version, "000042"),
        other => panic!("expected Manifest, got {other:?}"),
    }
    assert!(
        matches!(
            classify_walked(&format!("v42-{}.sgm", "a".repeat(32))),
            WalkedName::Unrecognised(_)
        ),
        "an unpadded version must not classify as a manifest"
    );
    assert_eq!(
        classify_walked(&format!("tx-{}.sgl", "b".repeat(32))),
        WalkedName::Transaction
    );
    assert_eq!(
        classify_walked(&format!("cp-{}.sgb", "c".repeat(32))),
        WalkedName::Checkpoint
    );
    match classify_walked(&format!("blob-{}.part", "d".repeat(32))) {
        WalkedName::Staging { kind } => assert_eq!(kind, "blob"),
        other => panic!("expected Staging, got {other:?}"),
    }
    assert_eq!(
        classify_walked(&"e".repeat(64)),
        WalkedName::Blob,
        "64 hex is a content name"
    );

    // Anything else is Unrecognised — reported verbatim, never an error.
    for stray in ["NOTES.txt", ".DS_Store", "manifests", "", "v01-abc.sgm"] {
        match classify_walked(stray) {
            WalkedName::Unrecognised(name) => assert_eq!(name, stray),
            other => panic!("expected Unrecognised for {stray:?}, got {other:?}"),
        }
    }

    // Total: no input can panic.
    for name in ["", ".", "..", "\0", "/", &"x".repeat(200)] {
        let _ = classify_walked(name);
    }
}
