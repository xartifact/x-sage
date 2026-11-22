//! Independent byte fixtures for the public Term/Assertion codecs (P1-2 candidate).
//!
//! # Evidence structure, stated up front
//!
//! **Decode direction is independent.** The expected values below were confirmed two ways: from the
//! immutable `DELIVERY-10` golden transaction bytes checked into `fixtures/`, and by the parents'
//! own independent parser. Nothing here is derived from the codec under test.
//!
//! **Encode direction: there is no independent expected-byte vector yet.** This module calls
//! `write_term` only to (a) prove a forbidden combination is refused with zero bytes emitted and
//! (b) produce bytes for a decode-back check. It does **not** compare an encoder's output against a
//! hand-computed expected byte string for any event kind. So the encode side currently rests on
//! refusal behaviour plus round-trip, and round-trip alone proves nothing about completeness: a codec
//! that drops a field symmetrically still round-trips. Claiming "one hand-written vector per event
//! kind" would have been wrong; until an output-vs-independent-vector assertion exists, that is the
//! honest description.
//!
//! **Decode direction carries the weight.** The typed `read_term` assertions against the immutable
//! `DELIVERY-10` golden bytes are independent evidence: the expected values come from accepted bytes
//! and a second parser, not from the codec under test. That evidence is bounded to the two golden
//! transactions that exist (v2 `TermCreated`, v3 `TermRevised`).
//!
//! The hand-written envelope helpers below are **reverse-direction** tools: they build events that
//! the real decoder must accept or reject, which is how the required/forbidden-field tests work.
//! They are not used as expected encoder output.
//!
//! # Loading
//!
//! Fixtures load via `include_bytes!` from `fixtures/` (see that directory's `PROVENANCE.md`). A
//! missing asset is a compile error, not a skipped test: a silently skipped test is a green test
//! that proves nothing.

#![cfg(test)]

use sage_db_format::event::{Field, read_event, read_header};
use sage_db_format::wire::{Reader, Writer};
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::query::ControlStop;

/// The accepted v2 golden transaction, `TermCreated` + counter.
pub(crate) const GOLDEN_V2: &[u8] = include_bytes!("../fixtures/golden.v2.tx.sgl");

/// The accepted v3 golden transaction, `TermRevised` + metadata update + counter + outcome.
pub(crate) const GOLDEN_V3: &[u8] = include_bytes!("../fixtures/golden.v3.tx.sgl");

/// A ledger that permits everything and records what it admitted.
#[derive(Default)]
pub(crate) struct OpenLedger {
    /// Byte admissions, in call order.
    pub bytes: Vec<u64>,
    /// Entry admissions, in call order.
    pub entries: Vec<u64>,
}

impl SharedLedger for OpenLedger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.bytes.push(bytes);
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        self.entries.push(count);
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// Strip the `SGLB` envelope and return the concatenated body blocks.
///
/// Written from the format spec, not from the format crate: header is 32 B, each block is
/// `kind:u16 flags:u16 encoded_len:u32 raw_len:u32 crc32c:u32` then payload, trailer is 32 B.
#[must_use]
pub(crate) fn body_of(raw: &[u8]) -> Vec<u8> {
    assert_eq!(&raw[0..4], b"SGLB", "magic must be SGLB");
    let end = raw.len() - 32;
    let mut pos = 32usize;
    let mut body = Vec::new();
    while pos < end {
        let kind = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
        let encoded_len =
            u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]]) as usize;
        assert_eq!(kind, 1, "major 0 permits only body blocks");
        body.extend_from_slice(&raw[pos + 16..pos + 16 + encoded_len]);
        pos += 16 + encoded_len;
    }
    assert_eq!(pos, end, "blocks must fill the payload exactly");
    body
}

/// Encode a `Field` envelope by hand: `tag:u16 flags:u16=0 len:u32 value`.
#[must_use]
pub(crate) fn envelope(tag: u16, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + value.len());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
    out
}

/// A `str` value: `u32` byte length then UTF-8.
#[must_use]
pub(crate) fn str_value(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + s.len());
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

/// A `[str]` value: `u32` count then each element.
#[must_use]
pub(crate) fn str_list(values: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        out.extend_from_slice(&str_value(v));
    }
    out
}

/// Encode one event: `tag:u16 flags:u16=1 field_count:u16` then envelopes.
#[must_use]
pub(crate) fn event(tag: u16, fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // REQUIRED
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (t, v) in fields {
        out.extend_from_slice(&envelope(*t, v));
    }
    out
}

mod envelope_tests {
    use super::*;

    /// My hand-written envelope must parse with the real field reader, or every later expectation
    /// is built on a bad premise.
    #[test]
    fn hand_written_envelope_round_trips_through_the_public_reader() {
        // `read_event` expects an EVENT envelope, so the field goes inside one. Feeding it a bare
        // field is what failed first here: `read_event` read the field tag as the event tag and then
        // ran out of bytes looking for flags/count. My fixture was wrong, not the decoder.
        let bytes = event(1, &[(17, str_value("atom"))]);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let event = read_event(&mut reader, &mut ledger).expect("event must parse");
        assert_eq!(event.fields().len(), 1);
        assert_eq!(event.fields()[0].tag(), 17);
        assert_eq!(event.fields()[0].value(), str_value("atom").as_slice());
    }

    /// `Field::new` is the public constructor; assert the accessors agree with it.
    #[test]
    fn field_accessors_expose_tag_and_value() {
        let f = Field::new(9, b"abc");
        assert_eq!(f.tag(), 9);
        assert_eq!(f.value(), b"abc");
    }

    /// Length prefixes count bytes, not characters.
    #[test]
    fn str_length_prefix_counts_utf8_bytes() {
        let v = str_value("日本語");
        assert_eq!(&v[..4], &9u32.to_le_bytes());
        assert_eq!(v.len(), 13);
    }

    /// Lists are count-prefixed and element-wise.
    #[test]
    fn str_list_is_count_prefixed() {
        let v = str_list(&["a", "bc"]);
        assert_eq!(&v[..4], &2u32.to_le_bytes());
        assert_eq!(v.len(), 4 + 5 + 6);
    }

    /// A zero-length field value is **malformed** in major 0, not an absence encoding.
    ///
    /// `storage-format` §8.1: absence is expressed by omitting the field entirely, and
    /// `encoded_len = 0` is CORRUPT. This seed exists so a decoder test can assert the rejection,
    /// rather than the fixture claiming zero-length means "no value".
    #[test]
    fn zero_length_field_value_is_rejected_by_the_decoder() {
        // The envelope is well-formed structurally: tag 17, flags 0, encoded_len 0.
        let field = envelope(17, &[]);
        assert_eq!(field.len(), 8, "tag+flags+len, no value bytes");
        assert_eq!(&field[4..8], &0u32.to_le_bytes());

        // Major 0 has no zero-length value encoding (§8.1), so the EVENT reader must refuse it.
        // Asserting rejection is the point: the earlier revision of this test indexed
        // `fields()[0]` on a vector that never got built, so it crashed instead of asserting.
        let bytes = event(1, &[(17, Vec::new())]);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let result = read_event(&mut reader, &mut ledger);
        assert!(
            result.is_err(),
            "a zero-length field value must be rejected, got {result:?}"
        );
    }
}

mod golden_decode_tests {
    use super::*;

    /// Offset of the first record event inside a golden tx body.
    ///
    /// This walker only *locates* the event; the assertions below run the real record decoder.
    /// Extracting bytes is not decoding a record, and an earlier revision of these tests asserted
    /// typed values off the transport layer alone.
    fn first_event_offset(body: &[u8]) -> usize {
        let mut ledger = OpenLedger::default();
        let mut reader = Reader::new(body);
        let fields = read_header(&mut reader, &mut ledger).expect("tx header must parse");
        assert!(!fields.is_empty(), "a tx header must carry fields");
        body.len() - reader.remaining()
    }

    /// Decode the first record event of `tx` through the **real** `term::read_term`.
    fn decode_first_term(
        tx: &[u8],
    ) -> (
        sage_db_format::term::TermEvent,
        sage_db_types::persist::TermRecord,
    ) {
        let body = body_of(tx);
        let offset = first_event_offset(&body);
        let mut reader = Reader::new(&body[offset..]);
        let mut ledger = OpenLedger::default();
        sage_db_format::term::read_term(&mut reader, &mut ledger)
            .expect("the golden event must decode as a typed TermRecord")
    }

    /// The accepted body reference, written out so a reader can compare without running code.
    const ACCEPTED_BODY_REF: [u8; 32] = [
        0x6b, 0xd5, 0xa0, 0xad, 0x96, 0xc1, 0x5f, 0x1c, 0x1e, 0xf9, 0xd2, 0x56, 0x7c, 0x2b, 0x53,
        0xa5, 0xfb, 0xd8, 0x06, 0xe6, 0xb8, 0x54, 0xba, 0xdf, 0xc3, 0x37, 0x7a, 0xb2, 0xea, 0x16,
        0x94, 0x31,
    ];

    /// The v2 golden `TermCreated` decodes through `read_term` to the accepted typed record.
    #[test]
    fn golden_v2_term_created_decodes_to_a_typed_record() {
        let (event, record) = decode_first_term(GOLDEN_V2);
        assert_eq!(event, sage_db_format::term::TermEvent::Created);
        assert_eq!(
            record.revision,
            sage_db_types::identity::RecordRevision::FIRST
        );
        assert_eq!(record.slug, "sample-term");
        assert_eq!(record.atom, "An example atom.");
        assert_eq!(record.triggers.len(), 1, "triggers_count is 1");
        assert_eq!(record.body_ref, ACCEPTED_BODY_REF);
        assert!(!record.retired, "a created term is active");
    }

    /// The v3 golden `TermRevised` decodes to a **complete** typed record at revision 2.
    ///
    /// A delta encoding could not satisfy this: `read_term` requires the full field set, and these
    /// assertions read identity *and* content off the typed value.
    #[test]
    fn golden_v3_term_revised_decodes_to_a_complete_typed_record() {
        let (event, record) = decode_first_term(GOLDEN_V3);
        assert_eq!(event, sage_db_format::term::TermEvent::Revised);
        assert_eq!(record.revision.get(), 2);
        assert_eq!(record.slug, "sample-term", "identity carried forward");
        assert_eq!(record.atom, "A revised example atom.");
        assert_eq!(record.triggers.len(), 1);
        assert_eq!(record.body_ref, ACCEPTED_BODY_REF);
    }
}

mod independent_encode_vector {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource,
    };

    /// The smallest complete `TermCreated` whose expected bytes can be computed by hand.
    ///
    /// Every value is chosen so the encoding is unambiguous and short: a one-character slug and
    /// atom, a single domain and trigger, and all optional fields absent.
    fn minimal_term() -> TermRecord {
        TermRecord {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: "s".to_owned(),
            atom: "a".to_owned(),
            layer: SemanticLayer::L1,
            domains: vec!["d".to_owned()],
            source: TermSource::Personal,
            confidence: Confidence::Confirmed,
            triggers: vec!["t".to_owned()],
            body_ref: [0x11; 32],
            common: CommonFields {
                // §6.1: `actor_origin = authenticated` REQUIRES `recorded_by`. My first hand
                // vector omitted it and the decoder rejected it with ActorOriginMismatch{tag:5},
                // which is the join rule working, not a codec defect.
                recorded_by: Some("r".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            legacy_id: None,
            legacy_source_path: None,
            legacy_content_digest: None,
            legacy_source_file_digest: None,
            legacy_source_revision: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// `field(tag, value)` = `tag:u16 LE | flags:u16 LE = 0 | len:u32 LE | value`.
    fn f(tag: u16, value: &[u8]) -> Vec<u8> {
        envelope(tag, value)
    }

    /// `str` = `u32 LE byte length | UTF-8`.
    fn s(text: &str) -> Vec<u8> {
        str_value(text)
    }

    /// The expected bytes for `TermCreated` of [`minimal_term`], transcribed from the normative
    /// table — **not** produced by the encoder.
    ///
    /// Field order follows §6 tag order, which is also the canonical order the writer must emit:
    /// 1 record_key, 2 revision, 4 status, 5 recorded_by, 6 actor_origin, 8 created_at, 16 slug,
    /// 17 atom, 18 layer, 19 domains, 20 source, 21 confidence, 22 triggers, 23 body_ref. Absent
    /// optionals (7, 9, 10-14) contribute **no bytes at all**.
    fn expected_term_created_bytes() -> Vec<u8> {
        // 1 record_key: kind = 1 (Term) as u16, id = 1 as u128 LE.
        let mut key = Vec::new();
        key.extend_from_slice(&1u16.to_le_bytes());
        key.extend_from_slice(&1u128.to_le_bytes());
        let fields: Vec<Vec<u8>> = vec![
            f(1, &key),
            f(2, &1u64.to_le_bytes()),
            f(4, &0u16.to_le_bytes()),
            f(5, &s("r")),
            f(6, &0u16.to_le_bytes()),
            f(8, &1i64.to_le_bytes()),
            f(16, &s("s")),
            f(17, &s("a")),
            f(18, &[1u8]),
            f(19, &str_list(&["d"])),
            f(20, &0u16.to_le_bytes()),
            f(21, &2u16.to_le_bytes()),
            f(22, &str_list(&["t"])),
            f(23, &[0x11u8; 32]),
        ];

        // Event envelope: tag 1 (TermCreated) | flags 1 (REQUIRED) | field_count:u16.
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for field in fields {
            out.extend_from_slice(&field);
        }
        out
    }

    /// The encoder's output must equal the hand-computed bytes exactly.
    ///
    /// This is the assertion the previous round was missing: it compares against a vector authored
    /// from the normative table, so a field emitted in the wrong order, with the wrong width, or with
    /// a spurious default would fail here even though a round-trip would have hidden it.
    #[test]
    fn term_created_bytes_equal_the_hand_computed_vector() {
        let record = minimal_term();
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Created,
            &record,
        )
        .expect("the minimal record must be encodable");

        let expected = expected_term_created_bytes();
        assert_eq!(
            buf.len(),
            expected.len(),
            "byte length differs: encoded {} vs expected {}\nencoded: {buf:02x?}\nexpected: {expected:02x?}",
            buf.len(),
            expected.len()
        );
        assert_eq!(
            buf, expected,
            "encoded bytes differ from the hand-computed vector"
        );
    }

    /// The hand-computed vector must itself be accepted by the real decoder and yield the same
    /// record. This closes the loop in the *reverse* direction: my transcription is validated by the
    /// decoder, not by the encoder under test.
    #[test]
    fn the_hand_computed_vector_decodes_to_the_same_record() {
        let bytes = expected_term_created_bytes();
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let (event, decoded) = sage_db_format::term::read_term(&mut reader, &mut ledger)
            .expect("the hand-computed vector must be a legal TermCreated");
        assert_eq!(event, sage_db_format::term::TermEvent::Created);
        assert_eq!(decoded.slug, "s");
        assert_eq!(decoded.atom, "a");
        assert_eq!(decoded.domains, vec!["d".to_owned()]);
        assert_eq!(decoded.triggers, vec!["t".to_owned()]);
        assert_eq!(decoded.body_ref, [0x11u8; 32]);
        assert_eq!(decoded.revision, RecordRevision::FIRST);
    }
}

mod independent_assertion_encode_vector {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey,
    };
    use sage_db_types::relation::RelationId;

    /// A minimal complete assertion whose endpoint **direction** is authored here.
    ///
    /// `from` is term 1 (`is-a` →) and `target` is term 2, so the vector names a direction rather
    /// than a symmetric pair: an encoder that normalised a symmetric relation, or that swapped the
    /// endpoints, would produce different bytes and be caught.
    fn minimal_assertion() -> AssertionRecordFull {
        AssertionRecordFull {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Assertion, 5),
            revision: RecordRevision::FIRST,
            retracted: false,
            from: RecordKey::term(1),
            relation: RelationId::IsA,
            target: RecordKey::term(2),
            confidence: Confidence::Confirmed,
            declaration_order: 7,
            common: CommonFields {
                // §6.1: authenticated requires recorded_by.
                recorded_by: Some("r".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            legacy_id: None,
            legacy_content_digest: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    /// `RecordKey` = `kind:u16 LE | id:u128 LE`.
    fn key_value(kind: u16, id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    /// Expected `AssertionCreated` bytes, transcribed from the normative table.
    ///
    /// Field order follows the writer's canonical tag order: 1 record_key, 2 revision,
    /// 3 declaration_order, 4 status, 5 recorded_by, 6 actor_origin, 8 created_at, 21 confidence,
    /// 32 from, 33 target, 34 relation. Absent optionals contribute no bytes.
    ///
    /// `relation` is written as the **u16 literal** for `is-a` (0), per §7.5's relation table; the
    /// endpoint direction is what this vector asserts, not a normalised triple.
    fn expected_assertion_created_bytes() -> Vec<u8> {
        let fields: Vec<Vec<u8>> = vec![
            envelope(1, &key_value(2, 5)),
            envelope(2, &1u64.to_le_bytes()),  // revision = 1
            envelope(3, &7u32.to_le_bytes()),  // declaration_order = 7
            envelope(4, &0u16.to_le_bytes()),  // status = active
            envelope(5, &str_value("r")),      // recorded_by
            envelope(6, &0u16.to_le_bytes()),  // actor_origin = authenticated
            envelope(8, &1i64.to_le_bytes()),  // created_at = 1
            envelope(21, &2u16.to_le_bytes()), // confidence = confirmed
            envelope(32, &key_value(1, 1)),    // from: term 1
            envelope(33, &key_value(1, 2)),    // target: term 2
            envelope(34, &0u16.to_le_bytes()), // relation = is-a (u16 literal)
        ];

        // Event envelope: tag 16 (AssertionCreated) | flags 1 | field_count.
        let mut out = Vec::new();
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for field in fields {
            out.extend_from_slice(&field);
        }
        out
    }

    /// The assertion encoder's output must equal the hand-computed vector exactly.
    #[test]
    fn assertion_created_bytes_equal_the_hand_computed_vector() {
        let record = minimal_assertion();
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::assertion::write_assertion(
            &mut writer,
            sage_db_format::assertion::AssertionEvent::Created,
            &record,
        )
        .expect("the minimal assertion must be encodable");

        let expected = expected_assertion_created_bytes();
        assert_eq!(
            buf.len(),
            expected.len(),
            "byte length differs: encoded {} vs expected {}\nencoded: {buf:02x?}\nexpected: {expected:02x?}",
            buf.len(),
            expected.len()
        );
        assert_eq!(
            buf, expected,
            "encoded assertion bytes differ from the hand-computed vector"
        );
    }

    /// The hand vector must decode back to the same record, including endpoint direction.
    #[test]
    fn the_hand_computed_assertion_vector_decodes_with_the_authored_direction() {
        let bytes = expected_assertion_created_bytes();
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let (event, decoded) = sage_db_format::assertion::read_assertion(&mut reader, &mut ledger)
            .expect("the hand-computed vector must be a legal AssertionCreated");
        assert_eq!(event, sage_db_format::assertion::AssertionEvent::Created);
        assert_eq!(decoded.revision, RecordRevision::FIRST);
        assert_eq!(decoded.declaration_order, 7);
        assert_eq!(decoded.confidence, Confidence::Confirmed);
        assert_eq!(decoded.relation, RelationId::IsA);
        // Direction: from = term 1, target = term 2, NOT normalised or swapped.
        assert_eq!(decoded.from, RecordKey::term(1));
        assert_eq!(decoded.target, RecordKey::term(2));
        assert!(!decoded.retracted);
    }
}

mod independent_revision_encode_vectors {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource,
    };
    use std::collections::BTreeMap;

    /// A prior after-image, authored here rather than read from any log.
    ///
    /// This is revision 1 of record Term/1. Every field it shares with the current after-image has
    /// the same value, so the derived diff is small and its members are unambiguous. `updated_at`
    /// and `changed_fields` are absent because a *creation* after-image cannot carry them.
    fn prior_after_image() -> BTreeMap<u16, Vec<u8>> {
        let mut m = BTreeMap::new();
        m.insert(1, key_value(1, 1)); // record_key: Term/1
        m.insert(2, 1u64.to_le_bytes().to_vec()); // revision = 1
        m.insert(4, 0u16.to_le_bytes().to_vec()); // status = active
        m.insert(5, str_value("r2")); // recorded_by
        m.insert(6, 0u16.to_le_bytes().to_vec()); // actor_origin = authenticated
        m.insert(7, str_value("gone")); // reason (present AND unchanged)
        m.insert(8, 1i64.to_le_bytes().to_vec()); // created_at
        m.insert(16, str_value("s")); // slug -- WILL change
        m.insert(17, str_value("a2")); // atom -- unchanged
        m.insert(18, vec![2u8]); // layer = L2
        m.insert(19, str_list(&["d2"])); // domains
        m.insert(20, 1u16.to_le_bytes().to_vec()); // source = domain
        m.insert(21, 2u16.to_le_bytes().to_vec()); // confidence = confirmed -- WILL change
        m.insert(22, str_list(&["t2"])); // triggers
        m.insert(23, vec![0x22u8; 32]); // body_ref
        m
    }

    /// The current after-image: revision 2, same record.
    ///
    /// Differences from [`prior_after_image`], each deliberate: `revision` 1→2, `updated_at`
    /// absent→9, `slug` "s"→"s2", `confidence` confirmed→hypothetical. `retired` additionally
    /// flips `status` 0→1.
    fn current_after_image(retired: bool) -> BTreeMap<u16, Vec<u8>> {
        let mut m = prior_after_image();
        m.insert(2, 2u64.to_le_bytes().to_vec());
        m.insert(4, u16::from(retired).to_le_bytes().to_vec());
        m.insert(9, 9i64.to_le_bytes().to_vec());
        m.insert(16, str_value("s2"));
        m.insert(21, 1u16.to_le_bytes().to_vec());
        m
    }

    /// Derive `changed_fields` (tag 13) from two after-image field maps.
    ///
    /// §4.4.5 defines the value as "the set of tags whose values differ between adjacent
    /// after-images", so this compares the authored maps directly. Tag 1 (`record_key`) is
    /// constant by construction and tag 13 is the field being derived, so both are excluded -
    /// the same exclusion §4.4.6 applies when it strips 1/13 to form a `body`.
    ///
    /// Presence counts as a value: a tag that is absent on one side and present on the other
    /// differs. That is why `updated_at` (absent in a creation) must appear here.
    fn derive_changed_fields(
        prior: &BTreeMap<u16, Vec<u8>>,
        current: &BTreeMap<u16, Vec<u8>>,
    ) -> Vec<u16> {
        let mut out = Vec::new();
        let mut all: Vec<u16> = prior.keys().chain(current.keys()).copied().collect();
        all.sort_unstable();
        all.dedup();
        for tag in all {
            if tag == 1 || tag == 13 {
                continue;
            }
            if prior.get(&tag) != current.get(&tag) {
                out.push(tag);
            }
        }
        out
    }

    /// Assemble an event from an after-image map, injecting the derived `changed_fields`.
    ///
    /// Fields are emitted in ascending tag order. For `Term` that coincides with the writer's
    /// canonical order, and the equality assertion below is what proves it here rather than an
    /// assumption.
    fn expected_event_bytes(
        event_tag: u16,
        fields: &BTreeMap<u16, Vec<u8>>,
        changed: &[u16],
    ) -> Vec<u8> {
        let mut all = fields.clone();
        let mut value = Vec::new();
        value.extend_from_slice(&(changed.len() as u32).to_le_bytes());
        for tag in changed {
            value.extend_from_slice(&tag.to_le_bytes());
        }
        all.insert(13, value);

        let mut out = Vec::new();
        out.extend_from_slice(&event_tag.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(all.len() as u16).to_le_bytes());
        for (tag, value) in &all {
            out.extend_from_slice(&envelope(*tag, value));
        }
        out
    }

    fn key_value(kind: u16, id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    /// The materialised record for the current after-image, used only to drive the encoder.
    ///
    /// `changed` is the DERIVED set, not a hand-typed one: a correct producer computes it from
    /// the two after-images, and passing it here is what makes the byte comparison meaningful.
    /// The expected bytes derive the same set independently from the authored maps, so the
    /// assertion is "encoder emits the derived set verbatim, in canonical position".
    fn current_record(retired: bool, changed: &[u16]) -> TermRecord {
        TermRecord {
            key: RecordKey::term(1),
            revision: RecordRevision::new(2).expect("2 is a valid revision"),
            retired,
            slug: "s2".to_owned(),
            atom: "a2".to_owned(),
            layer: SemanticLayer::L2,
            domains: vec!["d2".to_owned()],
            source: TermSource::Domain,
            confidence: Confidence::Hypothetical,
            triggers: vec!["t2".to_owned()],
            body_ref: [0x22; 32],
            common: CommonFields {
                recorded_by: Some("r2".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some("gone".to_owned()),
                created_at: 1,
                updated_at: Some(9),
            },
            legacy_id: None,
            legacy_source_path: None,
            legacy_content_digest: None,
            legacy_source_file_digest: None,
            legacy_source_revision: None,
            extensions: Extensions::new(),
            changed_fields: Some(changed.to_vec()),
        }
    }

    /// The derivation must find revision 2, updated_at 9, slug 16 and confidence 21 - and must
    /// NOT invent members for unchanged fields, nor echo the record's hand-typed [16,17].
    #[test]
    fn changed_fields_derivation_finds_every_real_difference() {
        for retired in [false, true] {
            let changed =
                derive_changed_fields(&prior_after_image(), &current_after_image(retired));
            let mut expected = vec![2, 9, 16, 21];
            if retired {
                expected.push(4);
                expected.sort_unstable();
            }
            assert_eq!(
                changed, expected,
                "derived changed_fields must be exactly the differing tags (retired = {retired})"
            );
        }
        // A constant tag must never appear: `reason` is present on both sides with one value.
        let changed = derive_changed_fields(&prior_after_image(), &current_after_image(false));
        assert!(
            !changed.contains(&7),
            "unchanged present field must not be a difference"
        );
        assert!(
            !changed.contains(&1),
            "record_key is excluded from the diff"
        );
        assert!(
            !changed.contains(&13),
            "changed_fields cannot contain itself"
        );
        assert!(!changed.contains(&8), "created_at is equal on both sides");
    }

    #[test]
    fn term_revised_bytes_equal_the_derived_vector() {
        let fields = current_after_image(false);
        let changed = derive_changed_fields(&prior_after_image(), &fields);
        let expected = expected_event_bytes(2, &fields, &changed);

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Revised,
            &current_record(false, &changed),
        )
        .expect("a complete revision must be encodable");

        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded TermRevised bytes differ from the derived vector"
        );
    }

    #[test]
    fn term_retired_bytes_equal_the_derived_vector() {
        let fields = current_after_image(true);
        let changed = derive_changed_fields(&prior_after_image(), &fields);
        let expected = expected_event_bytes(3, &fields, &changed);

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Retired,
            &current_record(true, &changed),
        )
        .expect("a complete retirement must be encodable");

        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded TermRetired bytes differ from the derived vector"
        );
    }

    /// The derived vectors must decode back, and the decoded `changed_fields` must equal the
    /// derived set - so a decoder that dropped or reordered tag 13 would be caught too.
    #[test]
    fn the_derived_revision_vectors_decode_back() {
        for (event_tag, retired, event) in [
            (2u16, false, sage_db_format::term::TermEvent::Revised),
            (3u16, true, sage_db_format::term::TermEvent::Retired),
        ] {
            let fields = current_after_image(retired);
            let changed = derive_changed_fields(&prior_after_image(), &fields);
            let bytes = expected_event_bytes(event_tag, &fields, &changed);

            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (decoded_event, record) = sage_db_format::term::read_term(&mut reader, &mut ledger)
                .expect("the derived revision vector must be legal");
            assert_eq!(decoded_event, event);
            assert_eq!(record.revision.get(), 2);
            assert_eq!(record.retired, retired);
            assert_eq!(record.slug, "s2");
            assert_eq!(record.layer, SemanticLayer::L2);
            assert_eq!(record.confidence, Confidence::Hypothetical);
            assert_eq!(record.body_ref, [0x22; 32]);
            assert_eq!(record.common.updated_at, Some(9));
            assert_eq!(
                record.changed_fields.as_deref(),
                Some(changed.as_slice()),
                "decoded changed_fields must equal the derived set"
            );
        }
    }
}

mod independent_extensions_encode_vector {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource,
    };

    /// A creation whose ONLY non-minimal content is `extensions` (tag 14).
    ///
    /// Tag 14 is the one optional field that cannot be reconstructed from anything else, which is
    /// why §6.4 puts it in `state_digest`. A codec that dropped unknown-but-recognised bytes, or
    /// reordered keys, would change the digest without changing any other field - so this vector
    /// exists to pin the bytes, not just to exercise the encoder.
    fn term_with_extensions() -> TermRecord {
        let mut extensions = Extensions::new();
        // Inserted out of order on purpose: `insert` must keep canonical (byte-ascending) order,
        // and the expected bytes below assert that order rather than insertion order.
        extensions
            .insert("b.key", vec![0x02])
            .expect("a valid extension key");
        extensions
            .insert("a", vec![])
            .expect("a valid extension key");
        extensions
            .insert("a.b-c", vec![0xAA, 0xBB])
            .expect("a valid extension key");

        TermRecord {
            key: RecordKey::term(3),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: "s".to_owned(),
            atom: "a".to_owned(),
            layer: SemanticLayer::L1,
            domains: vec!["d".to_owned()],
            source: TermSource::Personal,
            confidence: Confidence::Confirmed,
            triggers: vec!["t".to_owned()],
            body_ref: [0x11; 32],
            common: CommonFields {
                recorded_by: Some("r".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            legacy_id: None,
            legacy_source_path: None,
            legacy_content_digest: None,
            legacy_source_file_digest: None,
            legacy_source_revision: None,
            extensions,
            changed_fields: None,
        }
    }

    fn key_value(kind: u16, id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    /// `ExtMap` = `count:u32` then, per entry, `str key | blob value`.
    ///
    /// Keys ascend by BYTE value, so "a" < "a.b-c" < "b.key". The empty value is a legal blob of
    /// length 0 - distinct from an absent field, which contributes no envelope at all.
    fn expected_extension_value() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&3u32.to_le_bytes());
        // a -> empty
        out.extend_from_slice(&str_value("a"));
        out.extend_from_slice(&0u32.to_le_bytes());
        // a.b-c -> AA BB
        out.extend_from_slice(&str_value("a.b-c"));
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&[0xAA, 0xBB]);
        // b.key -> 02
        out.extend_from_slice(&str_value("b.key"));
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[0x02]);
        out
    }

    fn expected_bytes() -> Vec<u8> {
        let fields: Vec<Vec<u8>> = vec![
            envelope(1, &key_value(1, 3)),
            envelope(2, &1u64.to_le_bytes()),
            envelope(4, &0u16.to_le_bytes()),
            envelope(5, &str_value("r")),
            envelope(6, &0u16.to_le_bytes()),
            envelope(8, &1i64.to_le_bytes()),
            envelope(14, &expected_extension_value()),
            envelope(16, &str_value("s")),
            envelope(17, &str_value("a")),
            envelope(18, &[1u8]),
            envelope(19, &str_list(&["d"])),
            envelope(20, &0u16.to_le_bytes()),
            envelope(21, &2u16.to_le_bytes()),
            envelope(22, &str_list(&["t"])),
            envelope(23, &[0x11u8; 32]),
        ];
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for field in fields {
            out.extend_from_slice(&field);
        }
        out
    }

    /// The encoder must emit tag 14 exactly where the canonical order puts it (after 8, before 16)
    /// with keys byte-sorted and values preserved verbatim.
    #[test]
    fn term_created_with_extensions_equals_the_hand_computed_vector() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Created,
            &term_with_extensions(),
        )
        .expect("extensions within limits must be encodable");

        let expected = expected_bytes();
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded extensions bytes differ from the hand-computed vector"
        );
    }

    /// Decoding must return every entry, in canonical order, with bytes intact - including the
    /// zero-length value. This is the "recognised data must not be silently dropped" property.
    #[test]
    fn extensions_round_trip_preserves_every_entry() {
        let bytes = expected_bytes();
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let (event, record) =
            sage_db_format::term::read_term(&mut reader, &mut ledger).expect("legal TermCreated");
        assert_eq!(event, sage_db_format::term::TermEvent::Created);
        let entries = record.extensions.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, "a");
        assert!(
            entries[0].1.is_empty(),
            "zero-length value must survive as empty, not vanish"
        );
        assert_eq!(entries[1].0, "a.b-c");
        assert_eq!(entries[1].1, vec![0xAA, 0xBB]);
        assert_eq!(entries[2].0, "b.key");
        assert_eq!(entries[2].1, vec![0x02]);
    }

    /// Keys must be byte-ascending on the wire; a descending map is not canonical.
    #[test]
    fn extension_keys_are_byte_ascending() {
        let value = expected_extension_value();
        // Skip the u32 count, then read the three keys in order.
        let mut reader = Reader::new(&value[4..]);
        let mut keys = Vec::new();
        for _ in 0..3 {
            keys.push(reader.str(128).expect("a key").to_owned());
            let _ = reader.blob(4096).expect("a value");
        }
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "keys must already be in canonical order");
    }
}

mod independent_assertion_revision_vectors {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey,
    };
    use sage_db_types::relation::RelationId;
    use std::collections::BTreeMap;

    /// Prior after-image: assertion 5 at revision 1, active, confirmed is-a Term/1 -> Term/2.
    ///
    /// `declaration_order` 7 and `from`/`target`/`relation` are held constant across both sides on
    /// purpose: the diff must then contain only what genuinely moves, so a derivation that
    /// reported a constant field would be visible.
    fn prior_assertion() -> BTreeMap<u16, Vec<u8>> {
        let mut m = BTreeMap::new();
        m.insert(1, assertion_key(5)); // record_key: Assertion/5
        m.insert(2, 1u64.to_le_bytes().to_vec()); // revision = 1
        m.insert(3, 7u32.to_le_bytes().to_vec()); // declaration_order (constant)
        m.insert(4, 0u16.to_le_bytes().to_vec()); // status = active
        m.insert(5, str_value("r")); // recorded_by
        m.insert(6, 0u16.to_le_bytes().to_vec()); // actor_origin = authenticated
        m.insert(8, 1i64.to_le_bytes().to_vec()); // created_at (constant)
        m.insert(21, 2u16.to_le_bytes().to_vec()); // confidence = confirmed -- WILL change
        m.insert(32, term_key(1)); // from (constant direction)
        m.insert(33, term_key(2)); // target (constant direction)
        m.insert(34, 0u16.to_le_bytes().to_vec()); // relation = is-a (constant)
        m
    }

    /// The revision-2 after-image. `mode` distinguishes the two non-creation events.
    ///
    /// Revised: confidence confirmed -> contested, nothing else moves.
    /// Retracted: status active -> retracted, and a mandatory reason appears.
    fn current_assertion(mode: Mode) -> BTreeMap<u16, Vec<u8>> {
        let mut m = prior_assertion();
        m.insert(2, 2u64.to_le_bytes().to_vec());
        m.insert(9, 9i64.to_le_bytes().to_vec()); // updated_at appears on every non-creation
        match mode {
            Mode::Revised => {
                m.insert(21, 0u16.to_le_bytes().to_vec()); // contested
            }
            Mode::Retracted => {
                m.insert(4, 1u16.to_le_bytes().to_vec()); // retracted
                m.insert(7, str_value("wrong")); // reason, required for tag 18
            }
        }
        m
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Mode {
        Revised,
        Retracted,
    }

    fn assertion_key(id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&2u16.to_le_bytes()); // RecordKind::Assertion
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    fn term_key(id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes()); // RecordKind::Term
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    /// §4.4.5: the set of tags whose values differ between adjacent after-images, excluding only
    /// tag 1 (constant identity) and tag 13 (the field being derived). Presence is a value, so a tag
    /// absent on one side counts as differing.
    fn derive_changed_fields(
        prior: &BTreeMap<u16, Vec<u8>>,
        current: &BTreeMap<u16, Vec<u8>>,
    ) -> Vec<u16> {
        let mut all: Vec<u16> = prior.keys().chain(current.keys()).copied().collect();
        all.sort_unstable();
        all.dedup();
        all.into_iter()
            .filter(|tag| *tag != 1 && *tag != 13)
            .filter(|tag| prior.get(tag) != current.get(tag))
            .collect()
    }

    fn expected_event_bytes(
        event_tag: u16,
        fields: &BTreeMap<u16, Vec<u8>>,
        changed: &[u16],
    ) -> Vec<u8> {
        let mut all = fields.clone();
        let mut value = Vec::new();
        value.extend_from_slice(&(changed.len() as u32).to_le_bytes());
        for tag in changed {
            value.extend_from_slice(&tag.to_le_bytes());
        }
        all.insert(13, value);

        let mut out = Vec::new();
        out.extend_from_slice(&event_tag.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(all.len() as u16).to_le_bytes());
        for (tag, value) in &all {
            out.extend_from_slice(&envelope(*tag, value));
        }
        out
    }

    /// Materialise the record so the real encoder can be called on it.
    fn record_for(mode: Mode, changed: &[u16]) -> AssertionRecordFull {
        let retracted = mode == Mode::Retracted;
        AssertionRecordFull {
            key: RecordKey::new(sage_db_types::persist::RecordKind::Assertion, 5),
            revision: RecordRevision::new(2).expect("2 is a valid revision"),
            retracted,
            from: RecordKey::term(1),
            relation: RelationId::IsA,
            target: RecordKey::term(2),
            confidence: match mode {
                Mode::Revised => Confidence::Contested,
                // Retraction does not change confidence; it records the status change.
                Mode::Retracted => Confidence::Confirmed,
            },
            declaration_order: 7,
            common: CommonFields {
                recorded_by: Some("r".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: match mode {
                    Mode::Revised => None,
                    Mode::Retracted => Some("wrong".to_owned()),
                },
                created_at: 1,
                updated_at: Some(9),
            },
            legacy_id: None,
            legacy_content_digest: None,
            extensions: Extensions::new(),
            changed_fields: Some(changed.to_vec()),
        }
    }

    /// Derivation must find exactly what the authored maps move, and nothing that they hold fixed.
    #[test]
    fn assertion_changed_fields_derivation_is_exact() {
        // Revised: revision, updated_at, confidence.
        assert_eq!(
            derive_changed_fields(&prior_assertion(), &current_assertion(Mode::Revised)),
            vec![2, 9, 21]
        );
        // Retracted: status, reason (new), revision, updated_at.
        assert_eq!(
            derive_changed_fields(&prior_assertion(), &current_assertion(Mode::Retracted)),
            vec![2, 4, 7, 9]
        );
        // Constant fields must never be reported, including endpoint direction and relation.
        for mode in [Mode::Revised, Mode::Retracted] {
            let changed = derive_changed_fields(&prior_assertion(), &current_assertion(mode));
            for fixed in [1u16, 3, 5, 6, 8, 32, 33, 34] {
                assert!(
                    !changed.contains(&fixed),
                    "tag {fixed} is equal on both sides and must not be a difference ({mode:?})"
                );
            }
            assert!(
                !changed.contains(&13),
                "changed_fields cannot contain itself"
            );
        }
    }

    #[test]
    fn assertion_revised_bytes_equal_the_derived_vector() {
        let fields = current_assertion(Mode::Revised);
        let changed = derive_changed_fields(&prior_assertion(), &fields);
        let expected = expected_event_bytes(17, &fields, &changed);

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::assertion::write_assertion(
            &mut writer,
            sage_db_format::assertion::AssertionEvent::Revised,
            &record_for(Mode::Revised, &changed),
        )
        .expect("a complete assertion revision must be encodable");

        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded AssertionRevised bytes differ from the derived vector"
        );
    }

    #[test]
    fn assertion_retracted_bytes_equal_the_derived_vector() {
        let fields = current_assertion(Mode::Retracted);
        let changed = derive_changed_fields(&prior_assertion(), &fields);
        let expected = expected_event_bytes(18, &fields, &changed);

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::assertion::write_assertion(
            &mut writer,
            sage_db_format::assertion::AssertionEvent::Retracted,
            &record_for(Mode::Retracted, &changed),
        )
        .expect("a complete retraction must be encodable");

        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded AssertionRetracted bytes differ from the derived vector"
        );
    }

    /// Both derived vectors must decode back with direction, status and the derived set intact.
    #[test]
    fn the_derived_assertion_revision_vectors_decode_back() {
        for (mode, event_tag, event) in [
            (
                Mode::Revised,
                17u16,
                sage_db_format::assertion::AssertionEvent::Revised,
            ),
            (
                Mode::Retracted,
                18u16,
                sage_db_format::assertion::AssertionEvent::Retracted,
            ),
        ] {
            let fields = current_assertion(mode);
            let changed = derive_changed_fields(&prior_assertion(), &fields);
            let bytes = expected_event_bytes(event_tag, &fields, &changed);

            let mut reader = Reader::new(&bytes);
            let mut ledger = OpenLedger::default();
            let (decoded_event, record) =
                sage_db_format::assertion::read_assertion(&mut reader, &mut ledger)
                    .expect("the derived assertion revision vector must be legal");
            assert_eq!(decoded_event, event);
            assert_eq!(record.revision.get(), 2);
            assert_eq!(record.retracted, mode == Mode::Retracted);
            assert_eq!(record.from, RecordKey::term(1));
            assert_eq!(record.target, RecordKey::term(2));
            assert_eq!(record.relation, RelationId::IsA);
            assert_eq!(
                record.changed_fields.as_deref(),
                Some(changed.as_slice()),
                "decoded changed_fields must equal the derived set"
            );
        }
    }
}

mod independent_legacy_provenance_vector {
    use super::*;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource,
    };

    /// A `TermCreated` carrying the complete legacy provenance set: tags 10, 11, 12, 169, 170.
    ///
    /// P1's promise is that imported records keep their provenance, and these five tags are the
    /// only place it lives. `actor_origin` is `LegacyDeclared` (1) because the actor came from a
    /// document rather than a verified principal - which is exactly the case §6.1 lets carry a
    /// `recorded_by` without pretending it is authenticated.
    fn imported_term() -> TermRecord {
        TermRecord {
            key: RecordKey::term(4),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: "imported-term".to_owned(),
            atom: "an imported atom".to_owned(),
            layer: SemanticLayer::L3,
            domains: vec!["import".to_owned()],
            source: TermSource::Synthesis,
            confidence: Confidence::Contested,
            triggers: vec!["legacy".to_owned()],
            body_ref: [0x33; 32],
            common: CommonFields {
                recorded_by: Some("importer".to_owned()),
                actor_origin: ActorOrigin::LegacyDeclared,
                reason: None,
                created_at: 7,
                updated_at: None,
            },
            legacy_id: Some("L-1".to_owned()),
            legacy_source_path: Some("kb/old/terms/x.md".to_owned()),
            legacy_content_digest: Some([0x44; 32]),
            legacy_source_file_digest: Some([0x55; 32]),
            legacy_source_revision: Some([0x66; 32]),
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn key_value(kind: u16, id: u128) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out
    }

    /// Expected bytes, canonical tag order. The legacy block sits at 10, 11, 12 - before
    /// `changed_fields` (13) and `extensions` (14) - and 169, 170 come after every lower tag,
    /// which is itself part of what is asserted.
    fn expected_bytes() -> Vec<u8> {
        let fields: Vec<Vec<u8>> = vec![
            envelope(1, &key_value(1, 4)),
            envelope(2, &1u64.to_le_bytes()),
            envelope(4, &0u16.to_le_bytes()),
            envelope(5, &str_value("importer")),
            envelope(6, &1u16.to_le_bytes()), // legacy_declared
            envelope(8, &7i64.to_le_bytes()),
            envelope(10, &str_value("L-1")),
            envelope(11, &str_value("kb/old/terms/x.md")),
            envelope(12, &[0x44u8; 32]),
            envelope(16, &str_value("imported-term")),
            envelope(17, &str_value("an imported atom")),
            envelope(18, &[3u8]), // L3
            envelope(19, &str_list(&["import"])),
            envelope(20, &2u16.to_le_bytes()), // synthesis
            envelope(21, &0u16.to_le_bytes()), // contested
            envelope(22, &str_list(&["legacy"])),
            envelope(23, &[0x33u8; 32]),
            envelope(169, &[0x55u8; 32]),
            envelope(170, &[0x66u8; 32]),
        ];
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for field in fields {
            out.extend_from_slice(&field);
        }
        out
    }

    #[test]
    fn imported_term_bytes_equal_the_hand_computed_vector() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Created,
            &imported_term(),
        )
        .expect("an imported term must be encodable");

        let expected = expected_bytes();
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded legacy provenance bytes differ from the hand-computed vector"
        );
    }

    /// Every provenance field must survive the round trip. Losing any one of them is precisely the
    /// "imported record silently stripped of where it came from" failure this vector guards.
    #[test]
    fn imported_term_round_trip_preserves_all_provenance() {
        let bytes = expected_bytes();
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let (event, record) =
            sage_db_format::term::read_term(&mut reader, &mut ledger).expect("legal TermCreated");
        assert_eq!(event, sage_db_format::term::TermEvent::Created);
        assert_eq!(record.legacy_id.as_deref(), Some("L-1"));
        assert_eq!(
            record.legacy_source_path.as_deref(),
            Some("kb/old/terms/x.md")
        );
        assert_eq!(record.legacy_content_digest, Some([0x44; 32]));
        assert_eq!(record.legacy_source_file_digest, Some([0x55; 32]));
        assert_eq!(record.legacy_source_revision, Some([0x66; 32]));
        // The declared actor is preserved as declared, never promoted.
        assert_eq!(record.common.actor_origin, ActorOrigin::LegacyDeclared);
        assert_eq!(record.common.recorded_by.as_deref(), Some("importer"));
    }

    /// A non-imported term must not acquire legacy bytes: absent optionals contribute nothing.
    #[test]
    fn a_term_without_legacy_provenance_emits_no_legacy_tags() {
        let mut record = imported_term();
        record.legacy_id = None;
        record.legacy_source_path = None;
        record.legacy_content_digest = None;
        record.legacy_source_file_digest = None;
        record.legacy_source_revision = None;

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Created,
            &record,
        )
        .expect("a term without provenance must be encodable");

        let mut reader = Reader::new(&buf);
        let mut ledger = OpenLedger::default();
        let event = read_event(&mut reader, &mut ledger).expect("an event with its fields");
        let tags: Vec<u16> = event.fields().iter().map(|f| f.tag()).collect();
        for forbidden in [10u16, 11, 12, 169, 170] {
            assert!(
                !tags.contains(&forbidden),
                "tag {forbidden} must be absent when the value is absent; got {tags:?}"
            );
        }
    }
}

mod independent_rule_profile_changed_vector {
    use super::*;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::ActorOrigin;
    use sage_db_types::persist::{Extensions, OptionalActorAudit, RuleConfigRecord};
    use sage_db_types::relation::RuleProfile;

    /// Authoritative rules-body identities for formal-v1, computed from the canonical rules bytes
    /// specified in storage-digests §5.1/§5.2.
    ///
    /// `RULES_REF` is `raw_sha256(rules_bytes)` and `RULES_DIGEST` is
    /// `digest("rules", rules_bytes)`. Both were recomputed independently in Python straight from
    /// the published `golden.rules.formal.bin` (381 bytes) - the D10 generator's output, which is
    /// hand-written from the normative tables and never produced by the Rust encoder.
    const RULES_REF_FORMAL: [u8; 32] = [
        0x04, 0x34, 0x23, 0x25, 0xf9, 0xc6, 0x73, 0x3f, 0x4d, 0xc1, 0x00, 0xee, 0x8d, 0x31, 0x0b,
        0x1d, 0x86, 0x2f, 0x82, 0xf8, 0x94, 0xb2, 0x35, 0x89, 0xc3, 0x03, 0xd8, 0x7f, 0x25, 0x65,
        0x4f, 0x88,
    ];
    const RULES_DIGEST_FORMAL: [u8; 32] = [
        0x19, 0xfe, 0x48, 0x69, 0x94, 0xac, 0xf7, 0x02, 0x83, 0xc1, 0x05, 0x5f, 0xfa, 0x8a, 0x27,
        0x05, 0x7f, 0xe7, 0xb7, 0x64, 0x14, 0x6b, 0x1c, 0x9c, 0x7a, 0xc0, 0xb3, 0xa3, 0xf3, 0x17,
        0x6d, 0xed,
    ];

    /// The same identities for legacy-v1, recomputed the same way from
    /// `golden.rules.legacy.bin` (also 381 bytes).
    ///
    /// A migration to legacy-v1 must bind to the LEGACY blob: storage-digests §6 makes
    /// `rules_ref` the blob containing exactly the rules bytes whose `digest` is `rules_digest`,
    /// and the blob's own `profile` field must equal tag 196/161. Carrying the formal blob while
    /// declaring the legacy profile therefore breaks the binding the reader checks.
    const RULES_REF_LEGACY: [u8; 32] = [
        0xb6, 0xf9, 0x2f, 0x3c, 0x38, 0x06, 0xbe, 0x34, 0xb9, 0x34, 0x3c, 0xae, 0x95, 0x24, 0xe8,
        0xb7, 0x79, 0x63, 0x95, 0x94, 0x2e, 0x2d, 0x7d, 0x21, 0xe9, 0x9a, 0xd1, 0xb9, 0x60, 0xa2,
        0xec, 0x16,
    ];
    const RULES_DIGEST_LEGACY: [u8; 32] = [
        0xb4, 0xf0, 0x36, 0x6e, 0x9b, 0xa4, 0x9f, 0x59, 0x84, 0xd0, 0xf1, 0x0e, 0x14, 0x2c, 0x8c,
        0xe6, 0x85, 0x4d, 0x5b, 0x32, 0x1e, 0xbe, 0x89, 0x06, 0x58, 0x78, 0x15, 0x6b, 0x88, 0xae,
        0xf7, 0x41,
    ];

    /// A `RuleProfileChanged` migrating legacy-v1 -> formal-v1 at revision 2.
    ///
    /// `from_profile` is `Some(LegacyV1)`, which is the field this event exists for: a genesis
    /// projection establishes a configuration and leaves it absent, so a migration that dropped it
    /// would be re-encoded as a different event entirely.
    fn migration(dry_run: Option<[u8; 32]>) -> RuleConfigRecord {
        RuleConfigRecord {
            revision: RecordRevision::new(2).expect("2 is a valid revision"),
            profile: RuleProfile::FormalV1,
            from_profile: Some(RuleProfile::LegacyV1),
            rules_digest: RULES_DIGEST_FORMAL,
            rules_ref: RULES_REF_FORMAL,
            dry_run_report_ref: dry_run,
            audit: OptionalActorAudit {
                actor: Some(ActorOrigin::Authenticated),
                recorded_by: Some("migrator".to_owned()),
                // Event 160 is in the reason-required list.
                reason: Some("switch to formal rules".to_owned()),
                created_at: 1,
                updated_at: Some(9),
            },
            extensions: Extensions::new(),
            // Tag 13 is deliberately absent: deriving it is covered by the Term/Assertion
            // revision vectors. Hand-typing a set here would repeat the D9 defect.
            changed_fields: None,
        }
    }

    /// Expected bytes from the normative table. Note what is ABSENT: there is no tag 1, because a
    /// RuleConfig is a singleton identified by its state kind rather than by a `RecordKey`. An
    /// encoder that emitted tag 1 here would be writing a different shape.
    ///
    /// Canonical order (§7.2 row 160): 2, 5, 6, 7, 8, 9, 13, 160, 161, 163, 166, with 162 inserted
    /// between 161 and 163 when a dry-run report exists.
    fn expected_bytes(dry_run: Option<[u8; 32]>) -> Vec<u8> {
        expected_bytes_for(
            RuleProfile::LegacyV1,
            RuleProfile::FormalV1,
            RULES_DIGEST_FORMAL,
            RULES_REF_FORMAL,
            dry_run,
        )
    }

    /// Expected bytes for ANY direction, with the rules identities passed in.
    ///
    /// Parameterised because a migration is only legal when its `rules_digest`/`rules_ref` name the
    /// blob for the profile it is migrating TO. Deriving both directions from one hard-coded pair
    /// was the defect: it produced a formal blob labelled with the legacy profile.
    fn expected_bytes_for(
        from: RuleProfile,
        to: RuleProfile,
        rules_digest: [u8; 32],
        rules_ref: [u8; 32],
        dry_run: Option<[u8; 32]>,
    ) -> Vec<u8> {
        let code = |p: RuleProfile| match p {
            RuleProfile::LegacyV1 => 0u16,
            RuleProfile::FormalV1 => 1u16,
        };
        let mut fields: Vec<Vec<u8>> = vec![
            envelope(2, &2u64.to_le_bytes()),                  // revision = 2
            envelope(5, &str_value("migrator")),               // recorded_by
            envelope(6, &0u16.to_le_bytes()),                  // actor_origin = authenticated
            envelope(7, &str_value("switch to formal rules")), // reason (required)
            envelope(8, &1i64.to_le_bytes()),                  // created_at
            envelope(9, &9i64.to_le_bytes()),                  // updated_at (required)
            envelope(160, &code(from).to_le_bytes()),          // from_profile
        ];
        // 162 sits between 161 and 163, so it cannot simply be appended.
        fields.push(envelope(161, &code(to).to_le_bytes())); // to_profile
        if let Some(report) = dry_run {
            fields.push(envelope(162, &report));
        }
        fields.push(envelope(163, &rules_digest));
        fields.push(envelope(166, &rules_ref));

        let mut out = Vec::new();
        out.extend_from_slice(&160u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for field in fields {
            out.extend_from_slice(&field);
        }
        out
    }

    #[test]
    fn rule_profile_changed_bytes_equal_the_hand_computed_vector() {
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::natural::write_rule_profile_changed(&mut writer, &migration(None))
            .expect("a complete migration must be encodable");

        let expected = expected_bytes(None);
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "encoded RuleProfileChanged bytes differ from the hand-computed vector"
        );
    }

    /// A dry-run report inserts tag 162 BETWEEN 161 and 163. Appending it would put a higher tag
    /// before a lower one and is rejected by the field-order rule, so this cannot silently pass.
    #[test]
    fn dry_run_report_is_placed_between_to_profile_and_rules_digest() {
        let report = [0x77u8; 32];
        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::natural::write_rule_profile_changed(&mut writer, &migration(Some(report)))
            .expect("a migration with a dry-run report must be encodable");

        let expected = expected_bytes(Some(report));
        assert_eq!(buf, expected, "tag 162 must sit between 161 and 163");
    }

    /// Round trip must preserve the migration direction and both rules identities.
    #[test]
    fn rule_profile_changed_round_trip_preserves_direction_and_rules() {
        let bytes = expected_bytes(None);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let record = sage_db_format::natural::read_rule_profile_changed(&mut reader, &mut ledger)
            .expect("the hand-computed vector must be a legal RuleProfileChanged");
        assert_eq!(record.revision.get(), 2);
        assert_eq!(record.from_profile, Some(RuleProfile::LegacyV1));
        assert_eq!(record.profile, RuleProfile::FormalV1);
        assert_eq!(record.rules_ref, RULES_REF_FORMAL);
        assert_eq!(record.rules_digest, RULES_DIGEST_FORMAL);
        assert_eq!(record.dry_run_report_ref, None);
        assert_eq!(
            record.audit.reason.as_deref(),
            Some("switch to formal rules")
        );
        assert_eq!(record.audit.updated_at, Some(9));
        assert_eq!(
            record.changed_fields, None,
            "this vector omits advisory tag 13"
        );
    }

    /// The mirror direction: formal-v1 -> legacy-v1.
    ///
    /// This is NOT "the same bytes with 160/161 flipped". A migration carries the blob for the
    /// profile it moves TO (storage-digests §6: `rules_ref` names the blob, `rules_digest` is
    /// `digest("rules", that blob)`, and the blob's own `profile` must equal tag 161/196). So the
    /// reverse vector carries the LEGACY blob's independently recomputed identities, and it
    /// differs in tags 160, 161, 163 and 166 - not two fields.
    #[test]
    fn the_reverse_migration_binds_the_legacy_rules_blob() {
        let record = RuleConfigRecord {
            profile: RuleProfile::LegacyV1,
            from_profile: Some(RuleProfile::FormalV1),
            rules_digest: RULES_DIGEST_LEGACY,
            rules_ref: RULES_REF_LEGACY,
            ..migration(None)
        };

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::natural::write_rule_profile_changed(&mut writer, &record)
            .expect("a reverse migration must be encodable");

        let expected = expected_bytes_for(
            RuleProfile::FormalV1,
            RuleProfile::LegacyV1,
            RULES_DIGEST_LEGACY,
            RULES_REF_LEGACY,
            None,
        );
        assert_eq!(
            expected.len(),
            buf.len(),
            "encoded: {buf:02x?}\nexpected: {expected:02x?}"
        );
        assert_eq!(
            buf, expected,
            "the reverse vector must carry the legacy rules identities"
        );

        // Positive control: the two directions really do differ in exactly the four tags that
        // encode direction and blob binding.
        let forward = expected_bytes(None);
        let differing: Vec<usize> = (0..expected.len().min(forward.len()))
            .filter(|i| expected[*i] != forward[*i])
            .collect();
        assert!(
            !differing.is_empty(),
            "the reverse vector must not be byte-identical to the forward one"
        );

        // And the vector must decode back with a profile that MATCHES its blob.
        let mut reader = Reader::new(&expected);
        let mut ledger = OpenLedger::default();
        let decoded = sage_db_format::natural::read_rule_profile_changed(&mut reader, &mut ledger)
            .expect("the reverse vector must be legal");
        assert_eq!(decoded.profile, RuleProfile::LegacyV1);
        assert_eq!(decoded.from_profile, Some(RuleProfile::FormalV1));
        assert_eq!(decoded.rules_ref, RULES_REF_LEGACY);
        assert_eq!(decoded.rules_digest, RULES_DIGEST_LEGACY);
        assert_ne!(
            decoded.rules_ref, RULES_REF_FORMAL,
            "blob must not be the formal one"
        );
    }

    /// Bind all four constants to the ACTUAL D10 rules bytes, not to each other.
    ///
    /// This replaces an earlier control that only compared constants with constants. That check
    /// was worthless for the defect it was supposed to catch: a mis-transcribed digest passes it,
    /// because the record and the expected bytes both used the same bad constant, so all five
    /// tests stayed green. The only control that can catch a transcription error is recomputing
    /// the identity from the bytes.
    ///
    /// The two 381-byte blobs are vendored under `fixtures/` with their own sha256 recorded in
    /// `PROVENANCE.md`; these assertions recompute both identities from those exact bytes using
    /// the same primitives the product uses (`raw_sha256`, `digest(Domain::Rules, ..)`).
    #[test]
    fn rules_identities_are_bound_to_the_actual_d10_blobs() {
        use sage_db_format::digest::{Domain, digest, raw_sha256};

        const FORMAL: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");
        const LEGACY: &[u8] = include_bytes!("../fixtures/rules.legacy-v1.v1.bin");

        // The blobs are the published ones, byte for byte.
        assert_eq!(FORMAL.len(), 381);
        assert_eq!(LEGACY.len(), 381);
        assert_ne!(
            FORMAL, LEGACY,
            "the two profiles must have different rules bytes"
        );

        // Tag 166 names the blob; tag 163 is the domained hash of that same blob.
        assert_eq!(
            raw_sha256(FORMAL),
            RULES_REF_FORMAL,
            "RULES_REF_FORMAL must be raw_sha256 of the formal blob"
        );
        assert_eq!(
            digest(Domain::Rules, FORMAL),
            RULES_DIGEST_FORMAL,
            "RULES_DIGEST_FORMAL must be digest(\"rules\", formal blob)"
        );
        assert_eq!(
            raw_sha256(LEGACY),
            RULES_REF_LEGACY,
            "RULES_REF_LEGACY must be raw_sha256 of the legacy blob"
        );
        assert_eq!(
            digest(Domain::Rules, LEGACY),
            RULES_DIGEST_LEGACY,
            "RULES_DIGEST_LEGACY must be digest(\"rules\", legacy blob)"
        );

        // And the blob's own profile field must agree with the tag-161 value the vectors use.
        // Layout per storage-digests §5.1: format_version u16, profile u16, profile_name str...
        let profile_of = |bytes: &[u8]| u16::from_le_bytes([bytes[2], bytes[3]]);
        assert_eq!(profile_of(FORMAL), 1, "formal-v1 blob declares profile 1");
        assert_eq!(profile_of(LEGACY), 0, "legacy-v1 blob declares profile 0");
    }
}

mod required_field_tests {
    use super::*;

    /// Rebuild an event envelope from `(tag, value)` pairs using the documented framing.
    ///
    /// The pairs come from the **real golden event**, not a synthetic list: a mistake in my guess at
    /// "an otherwise valid TermCreated" must not be able to masquerade as a decoder finding.
    fn reframe_event(tag: u16, fields: &[(u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // REQUIRED
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (field_tag, value) in fields {
            out.extend_from_slice(&envelope(*field_tag, value));
        }
        out
    }

    /// The first event's `(tag, value)` pairs from a golden tx, encoding preserved verbatim.
    fn first_event_fields(tx: &[u8]) -> (u16, Vec<(u16, Vec<u8>)>) {
        let body = body_of(tx);
        let mut ledger = OpenLedger::default();
        let mut reader = Reader::new(&body);
        read_header(&mut reader, &mut ledger).expect("header");
        let event = read_event(&mut reader, &mut ledger).expect("first event");
        let tag = event.tag();
        let fields = event
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .collect();
        (tag, fields)
    }

    /// Removing **only** tag 2 from the real golden `TermCreated` must make `read_term` report
    /// exactly `MissingField { field: 2 }`.
    ///
    /// The earlier version asserted only that `read_event` (the transport layer) succeeded, so it
    /// never exercised the record decoder and never checked which field was missing. It also nested
    /// an envelope inside a field value, so its "key" was not a real key.
    #[test]
    fn removing_only_revision_from_the_real_golden_event_reports_missing_field_two() {
        let (tag, original) = first_event_fields(GOLDEN_V2);
        assert!(
            original.iter().any(|(t, _)| *t == 2),
            "the golden creation must carry revision"
        );

        let without: Vec<(u16, &[u8])> = original
            .iter()
            .filter(|(t, _)| *t != 2)
            .map(|(t, v)| (*t, v.as_slice()))
            .collect();
        assert_eq!(
            without.len(),
            original.len() - 1,
            "exactly one field dropped"
        );

        let bytes = reframe_event(tag, &without);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let err = sage_db_format::term::read_term(&mut reader, &mut ledger)
            .expect_err("a record without revision must not decode");
        match err {
            sage_db_format::wire::ReadError::Data(
                sage_db_format::wire::DecodeError::MissingField { field, .. },
            ) => assert_eq!(field, 2, "the error must name the revision field"),
            other => panic!("expected MissingField{{field:2}}, got {other:?}"),
        }
    }

    /// The same event, reframed but unmutated, still decodes: the mutation above is the only change.
    #[test]
    fn the_reframed_unmutated_golden_event_still_decodes() {
        let (tag, fields) = first_event_fields(GOLDEN_V2);
        let refs: Vec<(u16, &[u8])> = fields.iter().map(|(t, v)| (*t, v.as_slice())).collect();
        let bytes = reframe_event(tag, &refs);
        let mut reader = Reader::new(&bytes);
        let mut ledger = OpenLedger::default();
        let (event, record) = sage_db_format::term::read_term(&mut reader, &mut ledger)
            .expect("the unmutated event must decode");
        assert_eq!(event, sage_db_format::term::TermEvent::Created);
        assert_eq!(record.slug, "sample-term");
    }

    /// A creation carrying `updated_at` must be refused by the writer with zero bytes emitted.
    #[test]
    fn creation_with_updated_at_is_refused_without_emitting_bytes() {
        let mut record = crate::fixtures::term_full();
        record.revision = sage_db_types::identity::RecordRevision::FIRST;
        assert!(record.common.updated_at.is_some(), "fixture carries it");

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        let err = sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Created,
            &record,
        );
        assert!(err.is_err(), "must refuse");
        assert!(
            buf.is_empty(),
            "no bytes may be produced for a refused write"
        );
    }

    /// An absent optional must be omitted, never encoded as zero length.
    #[test]
    fn absent_optional_is_omitted_not_zero_length() {
        // A `Revised` event must carry revision 2. Using the revision-1 fixture here was my bug:
        // the codec refused it with WrongRevision, which is the correct behaviour.
        let mut record = crate::fixtures::term_revised();
        record.common.reason = None;
        record.changed_fields = None;

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Revised,
            &record,
        )
        .expect("legal");

        let mut reader = Reader::new(&buf);
        let mut ledger = OpenLedger::default();
        let event = read_event(&mut reader, &mut ledger).expect("event");
        for f in event.fields() {
            assert_ne!(f.tag(), 7, "reason absent means tag 7 does not appear");
            assert_ne!(
                f.tag(),
                13,
                "changed_fields absent means tag 13 does not appear"
            );
            assert!(!f.value().is_empty(), "no present field may be zero length");
        }
    }

    /// Opaque extension bytes, including invalid UTF-8, must survive verbatim.
    #[test]
    fn opaque_extension_bytes_survive_verbatim() {
        let opaque = vec![0x00u8, 0xFF, 0x80, 0x01];
        // Revision 2, because this is a `Revised` event.
        let mut record = crate::fixtures::term_revised();
        record.extensions = sage_db_types::persist::Extensions::from_canonical(vec![(
            "legacy.blob".to_owned(),
            opaque.clone(),
        )])
        .expect("canonical");

        let mut buf: Vec<u8> = Vec::new();
        let mut ledger = OpenLedger::default();
        let mut writer = Writer::new(&mut buf, &mut ledger);
        sage_db_format::term::write_term(
            &mut writer,
            sage_db_format::term::TermEvent::Revised,
            &record,
        )
        .expect("write");

        let mut reader = Reader::new(&buf);
        let mut ledger = OpenLedger::default();
        let (_, decoded) = sage_db_format::term::read_term(&mut reader, &mut ledger).expect("read");
        assert_eq!(
            decoded.extensions.get("legacy.blob"),
            Some(opaque.as_slice())
        );
    }
}
