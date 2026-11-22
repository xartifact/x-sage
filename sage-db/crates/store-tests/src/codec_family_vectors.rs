//! Independent expected-byte vectors for record families that had no them.
//!
//! # What these are, and are not
//!
//! Each vector here is **assembled from the normative tables** (storage-format §6 field table
//! and §7.2 per-event rows) and then compared, byte for byte, against what the real encoder
//! produces. No expected byte comes from calling the production encoder: a vector produced that
//! way asserts only that the encoder equals itself.
//!
//! The companion assertion is a real decode: the same bytes are fed to the family's `read_*`
//! function, so a vector that is not a legal event is caught rather than silently "passing".
//!
//! # Scope
//!
//! These are wire-format vectors. They say nothing about Store, replay, folding, recovery,
//! commit ordering or durability, and they make no platform claim. Business rules that belong to
//! a later phase (for example whether a proposal *should* have been approved) are out of scope by
//! construction: the codec does not decide them, so neither does a codec vector.
#![cfg(test)]

use crate::codec_fixtures::{OpenLedger, envelope, str_value};
use sage_db_format::wire::{Reader, Writer};

/// A `RecordKey` value: `kind:u16 LE | id:u128 LE`.
#[must_use]
pub(crate) fn record_key(kind: u16, id: u128) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out
}

/// An event envelope: `tag:u16 | flags:u16 = 1 | field_count:u16` then each field envelope.
///
/// Fields must already be in ascending tag order; the encoder rejects anything else, so a vector
/// built out of order fails loudly instead of passing.
#[must_use]
pub(crate) fn event_bytes(tag: u16, fields: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for field in fields {
        out.extend_from_slice(&field);
    }
    out
}

mod evidence_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, EvidenceKind, EvidenceRecord, Extensions, RecordKey,
    };

    /// Evidence 9 about nothing in particular: kind image, so tag 68 is MANDATORY (§6 says
    /// `asset_sha256` is required when `evidence_kind = 1`).
    fn evidence(revision: u64, revised: bool) -> EvidenceRecord {
        EvidenceRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Evidence, 9),
            revision: RecordRevision::new(revision).expect("a valid revision"),
            kind: EvidenceKind::Image,
            title: "t".to_owned(),
            locator: Some("l".to_owned()),
            excerpt_ref: Some([0x61; 32]),
            asset_sha256: Some([0x62; 32]),
            captured_at: 3,
            declared_captured_by: Some("d".to_owned()),
            common: CommonFields {
                recorded_by: Some("e".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                // 48 permits a reason and 49 permits one; neither requires it. Supply it on the
                // revision so the optional path is covered there and absent on creation.
                reason: if revised { Some("r".to_owned()) } else { None },
                created_at: 1,
                updated_at: if revised { Some(9) } else { None },
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 rows 48/49. Note tag 4: evidence has no retired state in this
    /// major, so `status` is always 0 - and it is still written, because the field table makes it
    /// mandatory for state-bearing events.
    fn expected(revised: bool) -> Vec<u8> {
        let mut fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(4, 9)), // Evidence/9
            envelope(2, &if revised { 2u64 } else { 1 }.to_le_bytes()),
            envelope(4, &0u16.to_le_bytes()), // active, always
            envelope(5, &str_value("e")),     // recorded_by
            envelope(6, &0u16.to_le_bytes()), // authenticated
        ];
        if revised {
            fields.push(envelope(7, &str_value("r")));
        }
        fields.push(envelope(8, &1i64.to_le_bytes())); // created_at
        if revised {
            fields.push(envelope(9, &9i64.to_le_bytes()));
        }
        fields.push(envelope(64, &1u16.to_le_bytes())); // image
        fields.push(envelope(65, &str_value("t"))); // title
        fields.push(envelope(66, &str_value("l"))); // locator
        fields.push(envelope(67, &[0x61; 32])); // excerpt_ref
        fields.push(envelope(68, &[0x62; 32])); // asset_sha256 (required for image)
        fields.push(envelope(69, &3i64.to_le_bytes())); // captured_at
        fields.push(envelope(70, &str_value("d"))); // declared_captured_by
        event_bytes(if revised { 49 } else { 48 }, fields)
    }

    #[test]
    fn evidence_events_match_their_hand_computed_vectors() {
        for revised in [false, true] {
            let record = evidence(if revised { 2 } else { 1 }, revised);
            let mut buf: Vec<u8> = Vec::new();
            let mut ledger = OpenLedger::default();
            let mut writer = Writer::new(&mut buf, &mut ledger);
            let api = if revised {
                sage_db_format::evidence::EvidenceEvent::Revised
            } else {
                sage_db_format::evidence::EvidenceEvent::Created
            };
            sage_db_format::evidence::write_evidence(&mut writer, api, &record)
                .unwrap_or_else(|e| panic!("evidence revised={revised} must encode: {e:?}"));

            let expected = expected(revised);
            assert_eq!(
                expected.len(),
                buf.len(),
                "evidence revised={revised}: encoded {buf:02x?} vs expected {expected:02x?}"
            );
            assert_eq!(
                buf, expected,
                "evidence revised={revised} differs from the vector"
            );
        }
    }

    #[test]
    fn evidence_vectors_decode_back() {
        for revised in [false, true] {
            let bytes = expected(revised);
            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (event, record) = sage_db_format::evidence::read_evidence(&mut reader, &mut ledger)
                .unwrap_or_else(|e| panic!("evidence revised={revised} must decode: {e:?}"));
            let expected_event = if revised {
                sage_db_format::evidence::EvidenceEvent::Revised
            } else {
                sage_db_format::evidence::EvidenceEvent::Created
            };
            assert_eq!(event, expected_event);
            assert_eq!(record.kind, EvidenceKind::Image);
            assert_eq!(record.title, "t");
            assert_eq!(record.locator.as_deref(), Some("l"));
            assert_eq!(record.excerpt_ref, Some([0x61; 32]));
            assert_eq!(record.asset_sha256, Some([0x62; 32]));
            assert_eq!(record.captured_at, 3);
            assert_eq!(record.declared_captured_by.as_deref(), Some("d"));
        }
    }

    /// An image evidence record without `asset_sha256` is not representable in this vector on
    /// purpose: §6 makes tag 68 mandatory when `kind = 1`. Asserting the refusal proves the vector
    /// is not omitting a field the format requires.
    #[test]
    fn image_evidence_without_asset_is_refused() {
        let mut record = evidence(1, false);
        record.asset_sha256 = None;
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        let result = sage_db_format::evidence::write_evidence(
            &mut writer,
            sage_db_format::evidence::EvidenceEvent::Created,
            &record,
        );
        assert!(
            result.is_err(),
            "an image evidence without asset_sha256 must be refused"
        );
        assert!(buf.is_empty(), "a refused record must produce no bytes");
    }

    /// Text evidence MAY omit the asset, and then tag 68 must be absent rather than zero-length.
    #[test]
    fn text_evidence_omits_the_asset_tag_entirely() {
        let mut record = evidence(1, false);
        record.kind = EvidenceKind::Text;
        record.asset_sha256 = None;
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::evidence::write_evidence(
            &mut writer,
            sage_db_format::evidence::EvidenceEvent::Created,
            &record,
        )
        .expect("text evidence without an asset is legal");

        let mut reader = Reader::new(&buf);
        let mut ledger = OpenLedger::default();
        let event = sage_db_format::event::read_event(&mut reader, &mut ledger).expect("event");
        let tags: Vec<u16> = event.fields().iter().map(|f| f.tag()).collect();
        assert!(
            !tags.contains(&68),
            "absent asset must not emit tag 68; got {tags:?}"
        );
        assert!(tags.contains(&64), "evidence_kind is mandatory");
    }
}

mod evidence_link_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, EvidenceLinkRecord, Extensions, RecordKey,
    };

    /// Link 11 citing Evidence/9 rev 1 and supporting Term/3 rev 2.
    fn link(retracted: bool) -> EvidenceLinkRecord {
        EvidenceLinkRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::EvidenceLink, 11),
            revision: RecordRevision::new(if retracted { 2 } else { 1 }).expect("valid"),
            retracted,
            evidence: RecordKey::new(sage_db_types::persist::RecordKind::Evidence, 9),
            evidence_revision: RecordRevision::FIRST,
            target: RecordKey::term(3),
            target_revision: RecordRevision::new(2).expect("valid"),
            common: CommonFields {
                recorded_by: Some("k".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                // 51 is reason-required; 50 is not.
                reason: if retracted {
                    Some("r".to_owned())
                } else {
                    None
                },
                created_at: 1,
                updated_at: if retracted { Some(9) } else { None },
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 rows 50/51. Tags 80/81 are the evidence side and 82/83 the target
    /// side; each is a `RecordKey` plus a `u64` revision, which is why the revisions cannot be
    /// confused with the record's own tag 2.
    fn expected(retracted: bool) -> Vec<u8> {
        let mut fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(5, 11)), // EvidenceLink/11
            envelope(2, &if retracted { 2u64 } else { 1 }.to_le_bytes()),
            envelope(4, &u16::from(retracted).to_le_bytes()), // status
            envelope(5, &str_value("k")),
            envelope(6, &0u16.to_le_bytes()),
        ];
        if retracted {
            fields.push(envelope(7, &str_value("r")));
        }
        fields.push(envelope(8, &1i64.to_le_bytes()));
        if retracted {
            fields.push(envelope(9, &9i64.to_le_bytes()));
        }
        fields.push(envelope(80, &record_key(4, 9))); // evidence: Evidence/9
        fields.push(envelope(81, &1u64.to_le_bytes())); // evidence_revision
        fields.push(envelope(82, &record_key(1, 3))); // target: Term/3
        fields.push(envelope(83, &2u64.to_le_bytes())); // target_revision
        event_bytes(if retracted { 51 } else { 50 }, fields)
    }

    #[test]
    fn link_events_match_their_hand_computed_vectors() {
        for retracted in [false, true] {
            let mut buf: Vec<u8> = Vec::new();
            let mut ledger = OpenLedger::default();
            let mut writer = Writer::new(&mut buf, &mut ledger);
            let api = if retracted {
                sage_db_format::evidence::LinkEvent::Retracted
            } else {
                sage_db_format::evidence::LinkEvent::Created
            };
            sage_db_format::evidence::write_link(&mut writer, api, &link(retracted))
                .unwrap_or_else(|e| panic!("link retracted={retracted} must encode: {e:?}"));

            let expected = expected(retracted);
            assert_eq!(
                expected.len(),
                buf.len(),
                "link retracted={retracted}: encoded {buf:02x?} vs expected {expected:02x?}"
            );
            assert_eq!(
                buf, expected,
                "link retracted={retracted} differs from the vector"
            );
        }
    }

    #[test]
    fn link_vectors_decode_back() {
        for retracted in [false, true] {
            let bytes = expected(retracted);
            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (event, record) = sage_db_format::evidence::read_link(&mut reader, &mut ledger)
                .unwrap_or_else(|e| panic!("link retracted={retracted} must decode: {e:?}"));
            let expected_event = if retracted {
                sage_db_format::evidence::LinkEvent::Retracted
            } else {
                sage_db_format::evidence::LinkEvent::Created
            };
            assert_eq!(event, expected_event);
            assert_eq!(record.retracted, retracted);
            assert_eq!(
                record.evidence,
                RecordKey::new(sage_db_types::persist::RecordKind::Evidence, 9)
            );
            assert_eq!(record.evidence_revision.get(), 1);
            assert_eq!(record.target, RecordKey::term(3));
            assert_eq!(record.target_revision.get(), 2);
        }
    }

    /// The status byte must track the event, and the writer refuses the mismatch rather than
    /// silently writing an inconsistent image.
    #[test]
    fn link_status_must_match_the_event() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        let result = sage_db_format::evidence::write_link(
            &mut writer,
            sage_db_format::evidence::LinkEvent::Retracted,
            &link(false),
        );
        assert!(result.is_err(), "status/event mismatch must be refused");
        assert!(buf.is_empty());
    }
}

mod vector_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, VectorRecord, VectorStatus,
    };

    /// Vector 4 embedding Term/3 revision 2 with model "m".
    fn vector(retired: bool) -> VectorRecord {
        VectorRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Vector, 4),
            revision: RecordRevision::new(if retired { 2 } else { 1 }).expect("valid"),
            status: if retired {
                VectorStatus::Retired
            } else {
                VectorStatus::Active
            },
            subject: RecordKey::term(3),
            subject_revision: RecordRevision::new(2).expect("valid"),
            model_id: "m".to_owned(),
            dimensions: 8,
            input_digest: [0x71; 32],
            values_ref: [0x72; 32],
            embedding_revision: 5,
            common: CommonFields {
                recorded_by: Some("v".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: if retired { Some("r".to_owned()) } else { None },
                created_at: 1,
                updated_at: if retired { Some(9) } else { None },
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 rows 80/81. `embedding_revision`(171) is a `u64`, distinct from
    /// the record's own `revision`(2), and `subject_revision`(97) is a third independent `u64`.
    fn expected(retired: bool) -> Vec<u8> {
        let mut fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(6, 4)), // Vector/4
            envelope(2, &if retired { 2u64 } else { 1 }.to_le_bytes()),
            envelope(4, &u16::from(retired).to_le_bytes()), // 0 active / 1 retired
            envelope(5, &str_value("v")),
            envelope(6, &0u16.to_le_bytes()),
        ];
        if retired {
            fields.push(envelope(7, &str_value("r")));
        }
        fields.push(envelope(8, &1i64.to_le_bytes()));
        if retired {
            fields.push(envelope(9, &9i64.to_le_bytes()));
        }
        fields.push(envelope(96, &record_key(1, 3))); // subject: Term/3
        fields.push(envelope(97, &2u64.to_le_bytes())); // subject_revision
        fields.push(envelope(98, &str_value("m"))); // model_id
        fields.push(envelope(99, &8u32.to_le_bytes())); // dimensions
        fields.push(envelope(100, &[0x71; 32])); // input_digest
        fields.push(envelope(101, &[0x72; 32])); // values_ref
        fields.push(envelope(171, &5u64.to_le_bytes())); // embedding_revision
        event_bytes(if retired { 81 } else { 80 }, fields)
    }

    #[test]
    fn vector_events_match_their_hand_computed_vectors() {
        for retired in [false, true] {
            let mut buf: Vec<u8> = Vec::new();
            let mut ledger = OpenLedger::default();
            let mut writer = Writer::new(&mut buf, &mut ledger);
            sage_db_format::collection::write_vector(&mut writer, retired, &vector(retired))
                .unwrap_or_else(|e| panic!("vector retired={retired} must encode: {e:?}"));
            let expected = expected(retired);
            assert_eq!(
                expected.len(),
                buf.len(),
                "vector retired={retired}: encoded {buf:02x?} vs expected {expected:02x?}"
            );
            assert_eq!(
                buf, expected,
                "vector retired={retired} differs from the vector"
            );
        }
    }

    #[test]
    fn vector_vectors_decode_back() {
        for retired in [false, true] {
            let bytes = expected(retired);
            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (got_retired, record) =
                sage_db_format::collection::read_vector(&mut reader, &mut ledger)
                    .unwrap_or_else(|e| panic!("vector retired={retired} must decode: {e:?}"));
            assert_eq!(got_retired, retired, "retired flag must round-trip");
            assert_eq!(
                record.status,
                if retired {
                    VectorStatus::Retired
                } else {
                    VectorStatus::Active
                }
            );
            assert_eq!(record.subject, RecordKey::term(3));
            assert_eq!(record.subject_revision.get(), 2);
            assert_eq!(record.model_id, "m");
            assert_eq!(record.dimensions, 8);
            assert_eq!(record.input_digest, [0x71; 32]);
            assert_eq!(record.values_ref, [0x72; 32]);
            assert_eq!(record.embedding_revision, 5);
        }
    }

    /// `dimensions = 0` is outside §6's `1..=4096` and must be refused before any byte.
    #[test]
    fn zero_dimensions_is_refused() {
        let mut record = vector(false);
        record.dimensions = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        assert!(sage_db_format::collection::write_vector(&mut writer, false, &record).is_err());
        assert!(buf.is_empty());
    }
}

mod frontier_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, FrontierEntryRecord, FrontierStatus, RecordKey,
    };

    /// Frontier entry 6 about Term/3 and Term/5.
    fn entry(revision: u64) -> FrontierEntryRecord {
        FrontierEntryRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::FrontierEntry, 6),
            revision: RecordRevision::new(revision).expect("valid"),
            status: FrontierStatus::Open,
            concept_refs: vec![RecordKey::term(3), RecordKey::term(5)],
            text_ref: [0x81; 32],
            priority: 1,
            common: CommonFields {
                recorded_by: Some("f".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: Some(9),
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 row 112. This family uses ONE tag for first and later revisions,
    /// so a revision-1 vector is legal here where it would not be for a two-tag family.
    ///
    /// `concept_refs`(128) is `count:u32` then each `RecordKey`, ascending by key as §1 requires
    /// of sorted sets.
    fn expected(revision: u64) -> Vec<u8> {
        let mut refs = Vec::new();
        refs.extend_from_slice(&2u32.to_le_bytes());
        refs.extend_from_slice(&record_key(1, 3));
        refs.extend_from_slice(&record_key(1, 5));

        let fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(8, 6)), // FrontierEntry/6
            envelope(2, &revision.to_le_bytes()),
            envelope(4, &0u16.to_le_bytes()), // open
            envelope(5, &str_value("f")),
            envelope(6, &0u16.to_le_bytes()),
            envelope(8, &1i64.to_le_bytes()),
            envelope(9, &9i64.to_le_bytes()),
            envelope(128, &refs),
            envelope(129, &[0x81; 32]),         // text_ref
            envelope(130, &1u16.to_le_bytes()), // priority = medium
        ];
        event_bytes(112, fields)
    }

    #[test]
    fn frontier_entry_matches_the_hand_computed_vector() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::collection::write_frontier(&mut writer, &entry(1))
            .expect("a frontier entry must encode");
        let expected = expected(1);
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded {buf:02x?} vs expected {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "frontier bytes differ from the hand-computed vector"
        );
    }

    #[test]
    fn frontier_vector_decodes_back() {
        let bytes = expected(1);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let record = sage_db_format::collection::read_frontier(&mut reader, &mut ledger)
            .expect("the vector must be legal");
        assert_eq!(record.status, FrontierStatus::Open);
        assert_eq!(
            record.concept_refs,
            vec![RecordKey::term(3), RecordKey::term(5)]
        );
        assert_eq!(record.text_ref, [0x81; 32]);
        assert_eq!(record.priority, 1);
    }

    /// `priority` has a closed domain (0..=2); 3 must be refused.
    #[test]
    fn priority_above_two_is_refused() {
        let mut record = entry(1);
        record.priority = 3;
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        assert!(sage_db_format::collection::write_frontier(&mut writer, &record).is_err());
        assert!(buf.is_empty());
    }
}

mod checkpoint_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, SessionCheckpointRecord,
    };

    /// Checkpoint 2 for Term/3, session 4.
    fn checkpoint() -> SessionCheckpointRecord {
        SessionCheckpointRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::SessionCheckpoint, 2),
            revision: RecordRevision::FIRST,
            location: sage_db_types::persist::CheckpointLocation::Term {
                concept: RecordKey::term(3),
                session_no: 4,
            },
            content_ref: [0x91; 32],
            common: CommonFields {
                recorded_by: Some("s".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 row 128: `2,48,132,133`. Tag 48 (`concept`) is shared with the
    /// claim family, which is exactly why it is worth pinning here too: a codec that treated tag
    /// 48 as claim-only would fail.
    fn expected() -> Vec<u8> {
        let fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(9, 2)), // SessionCheckpoint/2
            envelope(2, &1u64.to_le_bytes()),
            envelope(5, &str_value("s")),
            envelope(6, &0u16.to_le_bytes()),
            envelope(8, &1i64.to_le_bytes()),
            envelope(48, &record_key(1, 3)),    // concept: Term/3
            envelope(132, &4u32.to_le_bytes()), // session_no
            envelope(133, &[0x91; 32]),         // content_ref
        ];
        event_bytes(128, fields)
    }

    #[test]
    fn checkpoint_matches_the_hand_computed_vector() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::collection::write_checkpoint(&mut writer, &checkpoint())
            .expect("a checkpoint must encode");
        let expected = expected();
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded {buf:02x?} vs expected {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "checkpoint bytes differ from the hand-computed vector"
        );
    }

    #[test]
    fn checkpoint_vector_decodes_back() {
        let bytes = expected();
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let record = sage_db_format::collection::read_checkpoint(&mut reader, &mut ledger)
            .expect("the vector must be legal");
        assert_eq!(
            record.location,
            sage_db_types::persist::CheckpointLocation::Term {
                concept: RecordKey::term(3),
                session_no: 4,
            }
        );
        assert_eq!(record.content_ref, [0x91; 32]);
    }

    /// `session_no` is `≥ 1`; zero must be refused.
    #[test]
    fn zero_session_no_is_refused() {
        let mut record = checkpoint();
        record.location = sage_db_types::persist::CheckpointLocation::Term {
            concept: RecordKey::term(3),
            session_no: 0,
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        assert!(sage_db_format::collection::write_checkpoint(&mut writer, &record).is_err());
        assert!(buf.is_empty());
    }
}

mod proposal_family {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, ProposalKind, ProposalRecord, ProposalStatus,
        RecordKey, RevPair,
    };

    /// Proposal 12 against Term/3, at revision 1 (pending) or 2 (approved).
    fn proposal(decided: bool) -> ProposalRecord {
        ProposalRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Proposal, 12),
            revision: RecordRevision::new(if decided { 2 } else { 1 }).expect("valid"),
            status: if decided {
                ProposalStatus::Approved
            } else {
                ProposalStatus::Pending
            },
            kind: ProposalKind::ClaimCreate,
            subject: RecordKey::term(3),
            // Two baselines, ascending by (key, revision) as tag 114 requires.
            base_revisions: vec![
                RevPair {
                    key: RecordKey::term(3),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: RecordKey::term(5),
                    revision: RecordRevision::new(2).expect("valid"),
                },
            ],
            payload_ref: [0xA1; 32],
            review_note: if decided { Some("n".to_owned()) } else { None },
            declared_author: "a".to_owned(),
            common: CommonFields {
                recorded_by: Some("p".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: if decided { Some("r".to_owned()) } else { None },
                created_at: 1,
                updated_at: if decided { Some(9) } else { None },
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// Expected bytes per §7.2 rows 96/97.
    ///
    /// `base_revisions`(114) is `count:u32` then, per pair, `RecordKey` + `u64 revision`. Two
    /// independent `u64` revisions appear inside it, which is why the vector pins a pair with
    /// revision 1 and a pair with revision 2: a codec that reused one value would be visible.
    fn expected(decided: bool) -> Vec<u8> {
        let mut baselines = Vec::new();
        baselines.extend_from_slice(&2u32.to_le_bytes());
        baselines.extend_from_slice(&record_key(1, 3));
        baselines.extend_from_slice(&1u64.to_le_bytes());
        baselines.extend_from_slice(&record_key(1, 5));
        baselines.extend_from_slice(&2u64.to_le_bytes());

        let mut fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(7, 12)), // Proposal/12
            envelope(2, &if decided { 2u64 } else { 1 }.to_le_bytes()),
            envelope(4, &if decided { 1u16 } else { 0 }.to_le_bytes()), // approved / pending
            envelope(5, &str_value("p")),
            envelope(6, &0u16.to_le_bytes()),
        ];
        if decided {
            fields.push(envelope(7, &str_value("r")));
        }
        fields.push(envelope(8, &1i64.to_le_bytes()));
        if decided {
            fields.push(envelope(9, &9i64.to_le_bytes()));
        }
        fields.push(envelope(112, &1u16.to_le_bytes())); // claim-create
        fields.push(envelope(113, &record_key(1, 3))); // subject: Term/3
        fields.push(envelope(114, &baselines));
        fields.push(envelope(115, &[0xA1; 32])); // payload_ref
        if decided {
            fields.push(envelope(116, &str_value("n"))); // review_note, required
        }
        fields.push(envelope(168, &str_value("a"))); // declared_author
        event_bytes(if decided { 97 } else { 96 }, fields)
    }

    #[test]
    fn proposal_events_match_their_hand_computed_vectors() {
        for decided in [false, true] {
            let mut buf: Vec<u8> = Vec::new();
            let mut ledger = OpenLedger::default();
            let mut writer = Writer::new(&mut buf, &mut ledger);
            sage_db_format::collection::write_proposal(&mut writer, decided, &proposal(decided))
                .unwrap_or_else(|e| panic!("proposal decided={decided} must encode: {e:?}"));
            let expected = expected(decided);
            assert_eq!(
                expected.len(),
                buf.len(),
                "proposal decided={decided}: encoded {buf:02x?} vs expected {expected:02x?}"
            );
            assert_eq!(
                buf, expected,
                "proposal decided={decided} differs from the vector"
            );
        }
    }

    #[test]
    fn proposal_vectors_decode_back() {
        for decided in [false, true] {
            let bytes = expected(decided);
            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (got_decided, record) =
                sage_db_format::collection::read_proposal(&mut reader, &mut ledger)
                    .unwrap_or_else(|e| panic!("proposal decided={decided} must decode: {e:?}"));
            assert_eq!(got_decided, decided);
            assert_eq!(record.kind, ProposalKind::ClaimCreate);
            assert_eq!(record.subject, RecordKey::term(3));
            assert_eq!(record.payload_ref, [0xA1; 32]);
            assert_eq!(record.declared_author, "a");
            // Both baseline revisions survive, including the non-1 one.
            assert_eq!(record.base_revisions.len(), 2);
            assert_eq!(record.base_revisions[0].revision.get(), 1);
            assert_eq!(record.base_revisions[1].revision.get(), 2);
            assert_eq!(record.review_note.is_some(), decided);
        }
    }

    /// An unsorted baseline is not canonical and must be refused, so the vector's ascending
    /// order is a rule rather than a coincidence.
    #[test]
    fn unsorted_baseline_is_refused() {
        let mut record = proposal(false);
        record.base_revisions = vec![
            RevPair {
                key: RecordKey::term(5),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key: RecordKey::term(3),
                revision: RecordRevision::FIRST,
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        assert!(sage_db_format::collection::write_proposal(&mut writer, false, &record).is_err());
        assert!(buf.is_empty());
    }

    /// Decision events require `review_note`; omitting it must fail rather than produce a
    /// decided image without its justification.
    #[test]
    fn decided_proposal_without_review_note_is_refused() {
        let mut record = proposal(true);
        record.review_note = None;
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        assert!(sage_db_format::collection::write_proposal(&mut writer, true, &record).is_err());
        assert!(buf.is_empty());
    }
}

mod claim_family {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, Extensions, RecordKey,
    };

    /// Claim 7 whose concept is Term/3, at the revision and status the event requires.
    ///
    /// Values are chosen to be unambiguous on the wire: one-character identity strings, small
    /// integers, and a body of repeated bytes so a shifted offset is visible in a dump.
    fn claim(event: ClaimEvent, revision: u64, status: ClaimStatus) -> ClaimRecord {
        ClaimRecord {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Claim, 7),
            revision: RecordRevision::new(revision).expect("a valid revision"),
            status,
            concept: RecordKey::term(3),
            kind: ClaimKind::Risk,
            text_ref: [0x5A; 32],
            confidence: Confidence::Contested,
            common: CommonFields {
                recorded_by: Some("c".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: match event {
                    // 34 and 35 REQUIRE a reason. 33 permits one, and the vector supplies it so
                    // the optional string path is exercised; 32 forbids it in this vector because
                    // a creation has nothing to explain yet.
                    ClaimEvent::Created => None,
                    ClaimEvent::Revised | ClaimEvent::Superseded | ClaimEvent::Rejected => {
                        Some("r".to_owned())
                    }
                },
                created_at: 1,
                updated_at: match event {
                    ClaimEvent::Created => None,
                    _ => Some(9),
                },
            },
            legacy_id: None,
            legacy_content_digest: None,
            extensions: Extensions::new(),
            // Tag 13 is deliberately ABSENT from every vector in this file. Deriving it
            // correctly is covered by the Term/Assertion revision vectors; repeating a
            // hand-typed set here would be the D9 defect again.
            changed_fields: None,
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ClaimEvent {
        Created,
        Revised,
        Superseded,
        Rejected,
    }

    impl ClaimEvent {
        const fn tag(self) -> u16 {
            match self {
                Self::Created => 32,
                Self::Revised => 33,
                Self::Superseded => 34,
                Self::Rejected => 35,
            }
        }

        const fn api(self) -> sage_db_format::claim::ClaimEvent {
            match self {
                Self::Created => sage_db_format::claim::ClaimEvent::Created,
                Self::Revised => sage_db_format::claim::ClaimEvent::Revised,
                Self::Superseded => sage_db_format::claim::ClaimEvent::Superseded,
                Self::Rejected => sage_db_format::claim::ClaimEvent::Rejected,
            }
        }
    }

    /// Expected bytes for one claim event, per storage-format §7.2 rows 32/33/34/35.
    ///
    /// Required for all four: `1,2,4,6,8,21,48,49,50`. Optional: `5,7,9,10,12,13,14`.
    fn expected(event: ClaimEvent) -> Vec<u8> {
        let creation = event == ClaimEvent::Created;
        let mut fields: Vec<Vec<u8>> = vec![
            envelope(1, &record_key(3, 7)),                 // record_key: Claim/7
            envelope(2, &revision_of(event).to_le_bytes()), // revision: 1 at creation, else 2
            // §7.3 claim status: 0 accepted / 1 superseded / 2 rejected. Three distinct codes,
            // so this cannot be a boolean.
            envelope(4, &status_code(event).to_le_bytes()), // status
            envelope(5, &str_value("c")),                   // recorded_by
            envelope(6, &0u16.to_le_bytes()),               // actor_origin
        ];
        if !creation {
            // 34/35 REQUIRE a reason; 33 permits one. The vector exercises the permitted case
            // too, so the optional-string path is covered rather than assumed.
            fields.push(envelope(7, &str_value("r")));
        }
        fields.push(envelope(8, &1i64.to_le_bytes())); // created_at
        if !creation {
            fields.push(envelope(9, &9i64.to_le_bytes())); // updated_at
            // no tag 13: see the note on `changed_fields` above
        }
        fields.push(envelope(21, &0u16.to_le_bytes())); // contested
        fields.push(envelope(48, &record_key(1, 3))); // concept: Term/3
        fields.push(envelope(49, &3u16.to_le_bytes())); // risk
        fields.push(envelope(50, &[0x5A; 32])); // text_ref
        event_bytes(event.tag(), fields)
    }

    /// The persisted status code, per §7.3.
    const fn status_code(event: ClaimEvent) -> u16 {
        match event {
            ClaimEvent::Created | ClaimEvent::Revised => 0,
            ClaimEvent::Superseded => 1,
            ClaimEvent::Rejected => 2,
        }
    }

    fn status_of(event: ClaimEvent) -> ClaimStatus {
        match event {
            ClaimEvent::Created | ClaimEvent::Revised => ClaimStatus::Accepted,
            ClaimEvent::Superseded => ClaimStatus::Superseded,
            ClaimEvent::Rejected => ClaimStatus::Rejected,
        }
    }

    fn revision_of(event: ClaimEvent) -> u64 {
        if event == ClaimEvent::Created { 1 } else { 2 }
    }

    /// All four claim events: encoder output equals the hand-computed vector.
    #[test]
    fn claim_events_match_their_hand_computed_vectors() {
        for event in [
            ClaimEvent::Created,
            ClaimEvent::Revised,
            ClaimEvent::Superseded,
            ClaimEvent::Rejected,
        ] {
            let record = claim(event, revision_of(event), status_of(event));
            let mut buf: Vec<u8> = Vec::new();
            let mut ledger = OpenLedger::default();
            let mut writer = Writer::new(&mut buf, &mut ledger);
            sage_db_format::claim::write_claim(&mut writer, event.api(), &record)
                .unwrap_or_else(|e| panic!("{event:?} must be encodable: {e:?}"));

            let expected = expected(event);
            assert_eq!(
                expected.len(),
                buf.len(),
                "{event:?} length: encoded {buf:02x?} vs expected {expected:02x?}"
            );
            assert_eq!(
                buf, expected,
                "{event:?} bytes differ from the hand-computed vector"
            );
        }
    }

    /// All four claim events: the hand-computed vector decodes back to the same record.
    ///
    /// This is what stops a "vector" that is not a legal event from looking like evidence.
    #[test]
    fn claim_vectors_decode_back() {
        for event in [
            ClaimEvent::Created,
            ClaimEvent::Revised,
            ClaimEvent::Superseded,
            ClaimEvent::Rejected,
        ] {
            let bytes = expected(event);
            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (decoded_event, record) =
                sage_db_format::claim::read_claim(&mut reader, &mut ledger)
                    .unwrap_or_else(|e| panic!("{event:?} vector must be legal: {e:?}"));
            assert_eq!(decoded_event, event.api());
            assert_eq!(record.status, status_of(event));
            assert_eq!(record.revision.get(), revision_of(event));
            assert_eq!(record.concept, RecordKey::term(3));
            assert_eq!(record.kind, ClaimKind::Risk);
            assert_eq!(record.text_ref, [0x5A; 32]);
            assert_eq!(record.confidence, Confidence::Contested);
        }
    }

    /// The four status codes must actually differ on the wire; a codec that wrote `0` for every
    /// event would still round-trip its own output but would collapse the lifecycle.
    #[test]
    fn claim_status_codes_are_distinct_per_event() {
        let created = expected(ClaimEvent::Created);
        let superseded = expected(ClaimEvent::Superseded);
        let rejected = expected(ClaimEvent::Rejected);
        assert_ne!(created, superseded);
        assert_ne!(superseded, rejected);
        // Independently of layout: the status payload for superseded is 1 and for rejected is 2.
        assert!(
            superseded
                .windows(10)
                .any(|w| w == [4, 0, 0, 0, 2, 0, 0, 0, 1, 0])
        );
        assert!(
            rejected
                .windows(10)
                .any(|w| w == [4, 0, 0, 0, 2, 0, 0, 0, 2, 0])
        );
    }
}
