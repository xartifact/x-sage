//! Encoder tests: independent D10 byte comparison, plus negative admission and control paths.
//!
//! The positive tests build a [`TxToEncode`] / [`DecodedManifest`] by **decoding a real golden file**
//! and re-encoding it, then comparing to the original bytes. That is an independent expectation: the
//! bytes were produced by a different implementation, so equality is not an encoder talking to itself.
//! A self-roundtrip (encode then decode and compare values) would pass even with a symmetric error in
//! both directions, which is why it is not the primary evidence here.

use sage_db_format::encode::{
    DecodedManifest, EventToEncode, RequestCanonVersion, TxHeaderToEncode, TxToEncode,
    encode_manifest, encode_tx,
};
use sage_db_format::event::{Field, read_event};
use sage_db_format::manifest::read_manifest;
use sage_db_format::transaction::{read_envelope, read_transaction_header};
use sage_db_format::wire::Reader;
use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, OperatorAction, Refusal,
    UnsupportedReason, classify,
};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::persist::Extensions;

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

/// The canonical sink, collecting into a caller buffer. Exactly what the ruling allows: one complete
/// image handed to `BytesSink`, no second sink trait.
struct VecSink<'a> {
    out: &'a mut Vec<u8>,
}

impl sage_db_types::framing::BytesSink for VecSink<'_> {
    fn write(
        &mut self,
        bytes: &[u8],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), sage_db_types::framing::Refusal> {
        ledger
            .try_charge(bytes.len() as u64)
            .map_err(sage_db_types::framing::Refusal::Budget)?;
        if self.out.try_reserve_exact(bytes.len()).is_err() {
            return Err(sage_db_types::framing::Refusal::Allocation {
                requested: bytes.len(),
            });
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }
}

struct NullSink;

impl sage_db_types::framing::BytesSink for NullSink {
    fn write(
        &mut self,
        _bytes: &[u8],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), sage_db_types::framing::Refusal> {
        ledger
            .checkpoint()
            .map_err(sage_db_types::framing::Refusal::Stopped)
    }
}

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

/// Admits nothing beyond `n` bytes, so the first real charge fails.
struct Allow(u64);

impl SharedLedger for Allow {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.0 {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.0));
        }
        self.0 -= bytes;
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// Cancels at the first control checkpoint.
struct Halt;

impl SharedLedger for Halt {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

/// A sink that refuses, to prove the encoder propagates a sink refusal instead of swallowing it.
struct RefusingSink;

impl sage_db_types::framing::BytesSink for RefusingSink {
    fn write(&mut self, _bytes: &[u8], _ledger: &mut dyn SharedLedger) -> Result<(), Refusal> {
        Err(Refusal::Allocation { requested: 1 })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/file-codec")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Rebuild the writer inputs from a real transaction file, using only its bytes.
///
/// The event fields are taken from the file's own field stream, so nothing here invents a value.
struct RebuiltTx {
    header: TxHeaderToEncode,
    /// Owned per-event field values; index-aligned with `tags`.
    owners: Vec<Vec<(u16, Vec<u8>)>>,
    /// Event tags in order.
    tags: Vec<u16>,
}

fn rebuild_tx(bytes: &[u8]) -> RebuiltTx {
    let header = read_transaction_header(bytes, &Limits::V1, None, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("header: {e:?}"));

    let envelope = match read_envelope(bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(e) => e,
        _ => panic!("envelope"),
    };

    // Skip the header region: its field count, then each `tag|flags|len|value`.
    let mut reader = Reader::new(&envelope.body);
    let count = reader.u16().expect("header count");
    for _ in 0..count {
        let _tag = reader.u16().expect("tag");
        let _flags = reader.u16().expect("flags");
        let len = reader.u32().expect("len") as usize;
        reader.take_exact(len).expect("value");
    }

    let mut owners: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
    let mut tags: Vec<u16> = Vec::new();
    while !reader.is_empty() {
        let event = read_event(&mut reader, &mut Open).expect("event");
        tags.push(event.tag());
        owners.push(
            event
                .fields()
                .iter()
                .map(|f| (f.tag(), f.value().to_vec()))
                .collect(),
        );
    }

    RebuiltTx {
        header: TxHeaderToEncode {
            kb_uuid: header.kb_uuid,
            version: header.version,
            tx_id: header.tx_id,
            // `prev_version` is absent exactly for genesis, which the header states as version 1.
            prev_version: if header.version == 1 {
                None
            } else {
                Some(header.prev_version)
            },
            created_at: header.created_at as i64,
            principal_id: header.principal_id.clone(),
            request_id: header.request_id.clone(),
            payload_digest: header.payload_digest,
            rules_profile: header.rules_profile,
            rules_digest: header.rules_digest,
            request_canon_version: if header.request_canon_version == 0 {
                RequestCanonVersion::NonRequest
            } else {
                RequestCanonVersion::V1
            },
            expect_version: header.expect_version,
            extensions: Extensions::new(),
        },
        owners,
        tags,
    }
}

fn tx_to_encode<'a>(rebuilt: &'a RebuiltTx) -> TxToEncode<'a> {
    let events: Vec<EventToEncode<'a>> = rebuilt
        .owners
        .iter()
        .zip(&rebuilt.tags)
        .map(|(pairs, tag)| EventToEncode {
            tag: *tag,
            fields: pairs
                .iter()
                .map(|(t, v)| Field::new(*t, v.as_slice()))
                .collect(),
        })
        .collect();
    TxToEncode {
        header: rebuilt.header.clone(),
        events,
    }
}

fn manifest_to_encode(bytes: &[u8]) -> DecodedManifest {
    let summary = read_manifest(bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("manifest: {e:?}"));
    let h = summary.header;
    DecodedManifest {
        kb_uuid: h.kb_uuid,
        version: h.version,
        created_at: h.created_at,
        rules_profile: h.rules_profile,
        rules_digest: h.rules_digest,
        state_digest: h.state_digest,
        graph_digest: h.graph_digest,
        chain_digest: h.chain_digest,
        bookkeeping_digest: h.bookkeeping_digest,
        prev_manifest_name: h.prev_manifest_name.map(str::to_string),
        prev_manifest_sha256: h.prev_manifest_sha256,
        tx_file_name: h.tx_file_name.to_string(),
        tx_file_sha256: h.tx_file_sha256,
    }
}

// ---------------------------------------------------------------------------
// Primary evidence: byte equality with the immutable D10 files
// ---------------------------------------------------------------------------

#[test]
fn encode_manifest_reproduces_the_golden_genesis_manifest_byte_for_byte() {
    let real = fixture("golden.genesis.manifest.sgm");
    let to_encode = manifest_to_encode(&real);

    let mut out = Vec::new();
    match encode_manifest(
        &to_encode,
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode refused: {other:?}"),
    }

    assert_eq!(
        out.len(),
        real.len(),
        "encoded length must match the golden file"
    );
    assert_eq!(
        out, real,
        "encoded bytes must match the golden file exactly"
    );
}

#[test]
fn encode_manifest_reproduces_the_v2_golden_manifest_byte_for_byte() {
    // v2 is the non-genesis case: it must carry the predecessor pair, and the encoder has to place
    // those two fields at the right tags for the bytes to match.
    let real = fixture("golden.v2.manifest.sgm");
    let to_encode = manifest_to_encode(&real);
    assert!(
        to_encode.prev_manifest_name.is_some(),
        "v2 must carry a predecessor name"
    );

    let mut out = Vec::new();
    match encode_manifest(
        &to_encode,
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode refused: {other:?}"),
    }
    assert_eq!(out, real);
}

#[test]
fn encoding_from_a_prepared_view_reproduces_the_golden_genesis_transaction() {
    // `prepared::encode_prepared` must be the SAME serialization as `encode_tx`, and the
    // strongest available statement of that is: both reproduce the immutable D10 file.
    // A weaker local equality test would still pass if both paths drifted together.
    use sage_db_format::prepared::{OwnedEvent, encode_prepared};

    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    assert_eq!(rebuilt.tags.len(), 10, "Genesis + 9 IdAllocation");

    // Convert the rebuilt events into the owned form a Store would carry.
    let owned: Vec<OwnedEvent> = rebuilt
        .owners
        .iter()
        .zip(&rebuilt.tags)
        .map(|(pairs, tag)| OwnedEvent {
            tag: *tag,
            fields: pairs.clone(),
        })
        .collect();
    let header = rebuilt.header.clone();

    let mut out = Vec::new();
    match encode_prepared(
        &header,
        &owned,
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode_prepared refused the golden transaction: {other:?}"),
    }

    assert_eq!(
        out.len(),
        real.len(),
        "encoded length must match the golden file"
    );
    assert_eq!(
        out, real,
        "the prepared path must reproduce the golden bytes"
    );
}

#[test]
fn encoding_from_an_encodable_view_reproduces_the_golden_genesis_transaction() {
    // The `&EncodableView` entry point must be the same serialization too. This is the path
    // a Store actually takes when it admits once and encodes once.
    use sage_db_format::prepared::{OwnedEvent, encode_view, view_for_encode};

    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    let owned: Vec<OwnedEvent> = rebuilt
        .owners
        .iter()
        .zip(&rebuilt.tags)
        .map(|(pairs, tag)| OwnedEvent {
            tag: *tag,
            fields: pairs.clone(),
        })
        .collect();
    let header = rebuilt.header.clone();

    let view = match view_for_encode(&header, &owned, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(view) => view,
        other => panic!("admission refused: {other:?}"),
    };
    let mut out = Vec::new();
    match encode_view(
        &view,
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode_view refused the golden transaction: {other:?}"),
    }
    assert_eq!(out, real, "the view path must reproduce the golden bytes");
}

#[test]
fn encode_tx_reproduces_the_golden_genesis_transaction_byte_for_byte() {
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    // Sanity: the rebuild saw the whole transaction, not a truncated prefix.
    assert_eq!(rebuilt.tags.len(), 10, "Genesis + 9 IdAllocation");

    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode refused: {other:?}"),
    }

    assert_eq!(
        out.len(),
        real.len(),
        "encoded length must match the golden file"
    );
    assert_eq!(
        out, real,
        "encoded bytes must match the golden file exactly"
    );
}

#[test]
fn encode_tx_reproduces_the_v2_golden_transaction_byte_for_byte() {
    // v2 exercises the non-genesis path: `prev_version` is present on the wire, so a writer that
    // always emitted 0 (the genesis value) would produce different bytes here.
    let real = fixture("golden.v2.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    assert!(
        rebuilt.header.prev_version.is_some(),
        "v2 must carry a previous version"
    );

    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Ok(()) => {}
        other => panic!("encode refused: {other:?}"),
    }
    assert_eq!(out, real);
}

// ---------------------------------------------------------------------------
// Negative: header consistency the encoder must refuse to produce
// ---------------------------------------------------------------------------

fn genesis_inputs() -> (Vec<u8>, RebuiltTx) {
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    (real, rebuilt)
}

#[test]
fn a_genesis_header_claiming_a_predecessor_is_refused() {
    let (_real, mut rebuilt) = genesis_inputs();
    rebuilt.header.prev_version = Some(9); // version is 1, so this cannot be right
    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RevisionNotMonotonic {
            expected,
            found,
        })) => {
            assert_eq!(expected, 10);
            assert_eq!(found, 1);
        }
        other => panic!("expected RevisionNotMonotonic, got {}", describe(&other)),
    }
    assert!(out.is_empty(), "a refused encode must emit no bytes");
}

#[test]
fn a_non_genesis_header_without_a_predecessor_is_refused() {
    let (_real, mut rebuilt) = genesis_inputs();
    rebuilt.header.version = 7;
    rebuilt.header.prev_version = None; // absent predecessor is only legal at version 1
    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    ) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RevisionNotMonotonic {
            expected,
            found,
        })) => {
            assert_eq!(expected, 1);
            assert_eq!(found, 7);
        }
        other => panic!("expected RevisionNotMonotonic, got {}", describe(&other)),
    }
}

#[test]
fn canon_version_and_the_zero_digest_sentinel_must_agree() {
    // A real request claiming `NonRequest` would let it masquerade as genesis, and request-id
    // de-duplication keys off that distinction.
    let (_real, mut rebuilt) = genesis_inputs();
    rebuilt.header.request_canon_version = RequestCanonVersion::V1; // still carries the zero digest
    let mut out = Vec::new();
    assert!(matches!(
        encode_tx(
            &tx_to_encode(&rebuilt),
            &mut VecSink { out: &mut out },
            &Limits::V1,
            &mut Open
        ),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RequestCanonMismatch))
    ));

    // And the mirror case: a real digest labelled NonRequest.
    let (_real, mut rebuilt) = genesis_inputs();
    rebuilt.header.request_canon_version = RequestCanonVersion::NonRequest;
    rebuilt.header.payload_digest = [0x5A; 32];
    let mut out = Vec::new();
    assert!(matches!(
        encode_tx(
            &tx_to_encode(&rebuilt),
            &mut VecSink { out: &mut out },
            &Limits::V1,
            &mut Open
        ),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RequestCanonMismatch))
    ));
}

#[test]
fn expect_version_must_equal_prev_version() {
    let (_real, mut rebuilt) = genesis_inputs();
    rebuilt.header.expect_version = Some(3); // prev_version is 0
    let mut out = Vec::new();
    assert!(matches!(
        encode_tx(
            &tx_to_encode(&rebuilt),
            &mut VecSink { out: &mut out },
            &Limits::V1,
            &mut Open
        ),
        FormatOutcome::Format(FormatError::Corrupt(
            CorruptReason::OutcomeHeaderMismatch { .. }
        ))
    ));
}

#[test]
fn an_event_with_no_fields_or_an_empty_value_is_refused() {
    // A zero-length field value is rejected by the format, so the encoder must not produce one.
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);

    // Empty field list.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].fields.clear();
    let mut out = Vec::new();
    match encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Corrupt(_)) => {}
        other => panic!(
            "empty event fields must be Corrupt, got {}",
            describe(&other)
        ),
    }

    // Zero-length value.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].fields = vec![Field::new(192, &[])];
    let mut out = Vec::new();
    match encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ZeroLengthField { tag })) => {
            assert_eq!(tag, 192);
        }
        other => panic!(
            "zero-length field must be Corrupt, got {}",
            describe(&other)
        ),
    }
}

#[test]
fn out_of_order_and_duplicate_tags_are_refused() {
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);

    // Duplicate: strict ascent forbids equality.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].fields = vec![Field::new(5, &[1u8]), Field::new(5, &[2u8])];
    let mut out = Vec::new();
    assert!(matches!(
        encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldOrder))
    ));

    // Descending.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].fields = vec![Field::new(9, &[1u8]), Field::new(5, &[2u8])];
    let mut out = Vec::new();
    assert!(matches!(
        encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldOrder))
    ));
}

#[test]
fn an_unregistered_field_or_event_tag_is_unsupported_not_corrupt() {
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);

    // Tag 24 is a documented hole in the major-0 field table.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].fields = vec![Field::new(24, &[1u8])];
    let mut out = Vec::new();
    match encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FieldTag(t))) => {
            assert_eq!(t, 24);
        }
        other => panic!(
            "expected UnsupportedFormat(FieldTag(24)), got {}",
            describe(&other)
        ),
    }

    // An unassigned event tag.
    let mut tx = tx_to_encode(&rebuilt);
    tx.events[1].tag = 999;
    let mut out = Vec::new();
    match encode_tx(&tx, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::EventTag(t))) => {
            assert_eq!(t, 999);
        }
        other => panic!(
            "expected UnsupportedFormat(EventTag(999)), got {}",
            describe(&other)
        ),
    }
}

#[test]
fn a_manifest_genesis_with_a_predecessor_or_a_later_version_without_one_is_refused() {
    let real = fixture("golden.genesis.manifest.sgm");
    let mut m = manifest_to_encode(&real);
    m.prev_manifest_name = Some("v000042-00112233445566778899aabbccddeeff.sgm".into());
    m.prev_manifest_sha256 = Some([1u8; 32]);
    let mut out = Vec::new();
    assert!(matches!(
        encode_manifest(&m, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch))
    ));

    // Half a link: a name with no digest looks verifiable and is not.
    let mut m = manifest_to_encode(&real);
    m.version = 2;
    m.prev_manifest_name = Some("v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm".into());
    m.prev_manifest_sha256 = None;
    let mut out = Vec::new();
    assert!(matches!(
        encode_manifest(&m, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(_)))
    ));
}

#[test]
fn an_illegal_generated_name_is_refused_before_any_bytes_exist() {
    let real = fixture("golden.genesis.manifest.sgm");

    let mut m = manifest_to_encode(&real);
    m.tx_file_name = "tx-NOTHEX.sgl".into();
    let mut out = Vec::new();
    match encode_manifest(&m, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(t))) => {
            assert_eq!(t, sage_db_format::manifest::TAG_TX_FILE_NAME);
        }
        other => panic!("expected Corrupt(PathName), got {}", describe(&other)),
    }
    assert!(out.is_empty());

    // A predecessor name that violates §2.4.
    let mut m = manifest_to_encode(&fixture("golden.v2.manifest.sgm"));
    m.prev_manifest_name = Some("v1-short.sgm".into());
    let mut out = Vec::new();
    assert!(matches!(
        encode_manifest(&m, &mut VecSink { out: &mut out }, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(_)))
    ));
}

// ---------------------------------------------------------------------------
// Negative: ceilings
// ---------------------------------------------------------------------------

#[test]
fn whole_file_ceilings_are_limits_not_corruption() {
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);
    let tiny = Limits {
        max_tx_file_bytes: 64,
        ..Limits::V1
    };
    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &tiny,
        &mut Open,
    ) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::TxFile)) => {}
        other => panic!("expected Limit(TxFile), got {}", describe(&other)),
    }
    assert!(out.is_empty());

    let m = manifest_to_encode(&fixture("golden.genesis.manifest.sgm"));
    let tiny = Limits {
        max_manifest_file_bytes: 64,
        ..Limits::V1
    };
    let mut out = Vec::new();
    match encode_manifest(&m, &mut VecSink { out: &mut out }, &tiny, &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::ManifestFile)) => {}
        other => panic!("expected Limit(ManifestFile), got {}", describe(&other)),
    }
}

#[test]
fn string_and_event_count_ceilings_are_limits() {
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);

    // A field value over the string ceiling.
    let tiny = Limits {
        max_string_bytes: 2,
        ..Limits::V1
    };
    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &tiny,
        &mut Open,
    ) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::String { .. })) => {}
        other => panic!("expected Limit(String), got {}", describe(&other)),
    }

    // More events than the ceiling allows.
    let tiny = Limits {
        max_events_per_tx: 1,
        ..Limits::V1
    };
    let mut out = Vec::new();
    match encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &tiny,
        &mut Open,
    ) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::EventsPerTx)) => {}
        other => panic!("expected Limit(EventsPerTx), got {}", describe(&other)),
    }
}

// ---------------------------------------------------------------------------
// Negative: admission and control paths (refusals are never verdicts)
// ---------------------------------------------------------------------------

#[test]
fn a_budget_refusal_never_becomes_a_bytes_verdict() {
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);
    let mut out = Vec::new();

    let outcome = encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Allow(0),
    );
    assert!(
        matches!(outcome, FormatOutcome::Refused(Refusal::Budget(_))),
        "expected Refused(Budget), got {}",
        describe(&outcome)
    );
    assert_ne!(classify(&outcome), OperatorAction::RestoreBackup);
    assert_eq!(classify(&outcome), OperatorAction::RetryWithBudget);
    assert!(out.is_empty(), "a refused encode must not emit bytes");
}

#[test]
fn cancellation_is_reported_as_a_stop_and_writes_nothing() {
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);
    let mut out = Vec::new();

    let outcome = encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Halt,
    );
    assert!(
        outcome.as_refusal().is_some(),
        "cancellation must stay a refusal, got {}",
        describe(&outcome)
    );
    assert_eq!(classify(&outcome), OperatorAction::AbandonOrRetry);
    assert_ne!(classify(&outcome), OperatorAction::RestoreBackup);
    assert!(out.is_empty());
}

#[test]
fn a_sink_refusal_is_propagated_not_swallowed() {
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);
    let outcome = encode_tx(
        &tx_to_encode(&rebuilt),
        &mut RefusingSink,
        &Limits::V1,
        &mut Open,
    );
    assert!(
        matches!(outcome, FormatOutcome::Refused(Refusal::Allocation { .. })),
        "a sink refusal must surface as a refusal, got {}",
        describe(&outcome)
    );
}

#[test]
fn the_discarding_sink_still_runs_every_check() {
    // A caller that only wants validation must not get a weaker check than one that keeps the bytes.
    let tx_real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&tx_real);
    assert!(
        encode_tx(
            &tx_to_encode(&rebuilt),
            &mut NullSink,
            &Limits::V1,
            &mut Open
        )
        .is_ok()
    );

    let mut bad = rebuild_tx(&tx_real);
    bad.header.version = 99;
    assert!(!encode_tx(&tx_to_encode(&bad), &mut NullSink, &Limits::V1, &mut Open).is_ok());
}

// ---------------------------------------------------------------------------
// Producer/consumer agreement
// ---------------------------------------------------------------------------

#[test]
fn encoded_bytes_are_accepted_by_this_builds_own_decoders() {
    // A weaker property than byte equality, but it catches the specific failure the writers must avoid:
    // emitting something this build's reader would call Corrupt.
    let real = fixture("golden.genesis.tx.sgl");
    let rebuilt = rebuild_tx(&real);
    let mut out = Vec::new();
    encode_tx(
        &tx_to_encode(&rebuilt),
        &mut VecSink { out: &mut out },
        &Limits::V1,
        &mut Open,
    )
    .into_result()
    .expect("encode");

    assert!(
        read_transaction_header(&out, &Limits::V1, None, &mut Open).is_ok(),
        "the encoder's own output must be readable"
    );

    let m = manifest_to_encode(&fixture("golden.genesis.manifest.sgm"));
    let mut mout = Vec::new();
    encode_manifest(&m, &mut VecSink { out: &mut mout }, &Limits::V1, &mut Open)
        .into_result()
        .expect("encode");
    assert!(read_manifest(&mout, &Limits::V1, &mut Open).is_ok());
}

/// Render an outcome for a panic message.
fn describe<T>(outcome: &FormatOutcome<T>) -> String {
    match outcome {
        FormatOutcome::Ok(_) => "Ok(_)".to_string(),
        FormatOutcome::Format(e) => format!("Format({e:?})"),
        FormatOutcome::Refused(r) => format!("Refused({r:?})"),
    }
}
