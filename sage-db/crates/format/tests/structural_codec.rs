//! Independent public-boundary matrix for the two structural events.
//!
//! `IdAllocation` and `Genesis` are not records: one advances an allocation mark, the
//! other establishes two singletons at revision 1. The assertions here therefore check
//! different things from the record families - the numeric payload's per-space bound, the
//! absence of an actor where the format forbids one, and the single shared `extensions`
//! value that both genesis bodies reference.
//!
//! Expected images come from `tests/fixtures/natural-structural/`, whose `PROVENANCE.md`
//! records that they were produced by a separate Python encoder gated on the accepted
//! DELIVERY-10 bytes. `genesis-accepted.hex` **is** the accepted golden Genesis event.

use sage_db_format::{
    event::{self, EVENT_FLAG_REQUIRED},
    structural::*,
    wire::{DecodeError, ReadError, Reader, WriteError, Writer},
};
use sage_db_types::{
    identity::{KbUuid, RecordRevision},
    ledger::{BudgetError, BudgetUnit, SharedLedger},
    persist::*,
    query::ControlStop,
    relation::RuleProfile,
};

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/natural-structural"
);

// ---------------------------------------------------------------------------
// Ledgers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Open {
    bytes: Vec<u64>,
    entries: Vec<u64>,
}

impl SharedLedger for Open {
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

struct ByteBudget(u64);

impl SharedLedger for ByteBudget {
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

struct Cancelled;

impl SharedLedger for Cancelled {
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{FIXTURES}/{name}.hex");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("mandatory fixture {path} is unreadable: {error}"));
    let hex = text.trim().to_owned();
    assert!(!hex.is_empty(), "fixture {path} is empty");
    assert_eq!(hex.len() % 2, 0, "fixture {path} has an odd hex length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

fn encode(f: impl FnOnce(&mut Writer<'_>) -> Result<(), WriteError>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ledger = Open::default();
    f(&mut Writer::new(&mut out, &mut ledger)).expect("encode");
    out
}

fn edit(
    bytes: &[u8],
    tag: Option<u16>,
    drop: Option<u16>,
    replace: Option<(u16, Vec<u8>)>,
) -> Vec<u8> {
    let mut ledger = Open::default();
    let event = event::read_event(&mut Reader::new(bytes), &mut ledger).expect("framing");
    let mut fields: Vec<(u16, Vec<u8>)> = event
        .fields()
        .iter()
        .filter(|field| Some(field.tag()) != drop)
        .filter(|field| replace.as_ref().is_none_or(|(t, _)| *t != field.tag()))
        .map(|field| (field.tag(), field.value().to_vec()))
        .collect();
    if let Some(pair) = replace {
        fields.push(pair);
    }
    fields.sort_by_key(|(tag, _)| *tag);
    let mut out = Vec::new();
    out.extend_from_slice(&tag.unwrap_or(event.tag()).to_le_bytes());
    out.extend_from_slice(&EVENT_FLAG_REQUIRED.to_le_bytes());
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (tag, value) in fields {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&value);
    }
    out
}

fn text(value: &str) -> Vec<u8> {
    let mut out = (value.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(value.as_bytes());
    out
}

fn allocation(space: IdSpace, hwm: u128) -> IdAllocationEvent {
    IdAllocationEvent {
        allocation: Watermark { space, hwm },
        reason: None,
        extensions: Extensions::new(),
    }
}

fn genesis() -> GenesisRecord {
    GenesisRecord {
        created_at: 1_700_000_000_000_000,
        reason: None,
        metadata: GenesisMetadata {
            kb_uuid: KbUuid::from_bytes([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]),
            slug: "sample-kb".into(),
            name: "Sample KB".into(),
            description: "Genesis fixture.".into(),
            domains: vec!["demo".into()],
            extensions: Extensions::new(),
        },
        rules: GenesisRules {
            profile: RuleProfile::FormalV1,
            // The accepted DELIVERY-10 rules binding, so this helper is the same record
            // the `genesis-typical` fixture was generated from.
            rules_digest: hex32("19fe486994acf70283c1055ffa8a27057fe7b764146b1c9c7ac0b3a3f3176ded"),
            rules_ref: hex32("04342325f9c6733f4dc100ee8d310b1d862f82f894b23589c303d87f25654f88"),
            extensions: Extensions::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// 176 IdAllocation
// ---------------------------------------------------------------------------

#[test]
fn id_allocation_matches_the_accepted_genesis_watershed() {
    // The accepted genesis transaction carries nine of these, one per space, all at 0.
    // This asserts the exact bytes of the first one and the per-space round trip.
    let bytes = encode(|w| write_id_allocation(w, &allocation(IdSpace::TermId, 0)));
    assert_eq!(bytes, fixture("idalloc-zero"));
    assert_eq!(
        read_id_allocation(&mut Reader::new(&bytes), &mut Open::default()).unwrap(),
        allocation(IdSpace::TermId, 0)
    );
}

#[test]
fn the_mark_is_a_u128_not_a_u64() {
    let bytes =
        encode(|w| write_id_allocation(w, &allocation(IdSpace::SessionCheckpointId, u128::MAX)));
    assert_eq!(bytes, fixture("idalloc-u128-max"));
    // Exact width arithmetic, not a comment: 6 (event header) + 10 (`164` = 2+2+4+2)
    // + 24 (`165` = 2+2+4+16) = 40. A u64 watermark would be 32, so this separates the
    // two implementations instead of agreeing with whatever the encoder emitted.
    assert_eq!(bytes.len(), 40);
    assert_eq!(
        read_id_allocation(&mut Reader::new(&bytes), &mut Open::default()).unwrap(),
        allocation(IdSpace::SessionCheckpointId, u128::MAX)
    );
}

#[test]
fn every_space_round_trips_at_its_own_bound() {
    for space in IdSpace::ALL {
        let event = allocation(space, space.max_id());
        let bytes = encode(|w| write_id_allocation(w, &event));
        assert_eq!(
            read_id_allocation(&mut Reader::new(&bytes), &mut Open::default()).unwrap(),
            event,
            "{space:?}"
        );
    }
}

#[test]
fn a_term_mark_above_u32_max_is_refused_on_both_paths() {
    let over = allocation(IdSpace::TermId, u32::MAX as u128 + 1);
    assert!(!over.allocation.is_in_range());
    let mut out = Vec::new();
    let mut ledger = Open::default();
    assert!(
        write_id_allocation(&mut Writer::new(&mut out, &mut ledger), &over).is_err(),
        "a term watermark above u32::MAX claims an id no term key can carry back"
    );
    assert!(
        out.is_empty(),
        "nothing may be published after a data refusal"
    );

    let bytes = fixture("idalloc-term-max");
    let raw = edit(
        &bytes,
        None,
        None,
        Some((165, (u32::MAX as u128 + 1).to_le_bytes().to_vec())),
    );
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::WatermarkOutOfRange {
            space: 0,
            ..
        }))
    ));
}

#[test]
fn the_space_domain_is_closed() {
    let bytes = fixture("idalloc-zero");
    let raw = edit(&bytes, None, None, Some((164, 9u16.to_le_bytes().to_vec())));
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: 164,
            value: 9
        }))
    ));
}

#[test]
fn allocation_carries_its_conditional_reason_and_extensions() {
    // §7.2's non-record class grants 7 and 14. Refusing them would reject a legal file;
    // dropping them would violate §8.3, so the event carries both losslessly.
    let mut event = allocation(IdSpace::AssertionId, 7);
    event.reason = Some("bulk import advanced the assertion space".into());
    event
        .extensions
        .insert("import.batch", b"42".to_vec())
        .unwrap();
    let bytes = encode(|w| write_id_allocation(w, &event));
    assert_eq!(
        read_id_allocation(&mut Reader::new(&bytes), &mut Open::default()).unwrap(),
        event,
        "reason and extensions must survive the round trip"
    );
}

#[test]
fn allocation_refuses_a_created_at_it_cannot_store() {
    // Row 176 lists only 164/165; `8` has no slot on the carrier, so accepting it would
    // be accept-and-drop. Row 224's genesis is the event that does carry a timestamp.
    let bytes = fixture("idalloc-zero");
    let raw = edit(&bytes, None, None, Some((8, 5i64.to_le_bytes().to_vec())));
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::ForbiddenField {
            field: 8,
            ..
        }))
    ));
}

#[test]
fn allocation_requires_both_payload_fields() {
    let bytes = fixture("idalloc-zero");
    for tag in [164u16, 165] {
        let raw = edit(&bytes, None, Some(tag), None);
        assert!(matches!(
            read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
            Err(ReadError::Data(DecodeError::MissingField { field, .. })) if field == tag
        ));
    }
}

#[test]
fn allocation_refuses_a_record_key_it_is_not_identified_by() {
    let bytes = fixture("idalloc-zero");
    let raw = edit(
        &bytes,
        None,
        None,
        Some((
            1,
            vec![1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )),
    );
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::ForbiddenField {
            field: 1,
            ..
        }))
    ));
}

#[test]
fn allocation_is_refused_under_the_genesis_tag() {
    let bytes = fixture("idalloc-zero");
    let raw = edit(&bytes, Some(224), None, None);
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Uninterpretable(_))
    ));
}

#[test]
fn allocation_decode_observes_a_cancelled_attempt() {
    let bytes = fixture("idalloc-zero");
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&bytes), &mut Cancelled),
        Err(ReadError::Stopped(ControlStop::Cancelled))
    ));
}

#[test]
fn allocation_encode_refuses_before_publishing_under_a_zero_budget() {
    let mut out = Vec::new();
    let mut ledger = ByteBudget(0);
    let error = write_id_allocation(
        &mut Writer::new(&mut out, &mut ledger),
        &allocation(IdSpace::TermId, 0),
    )
    .expect_err("zero budget");
    assert!(matches!(error, WriteError::Budget(_)), "{error:?}");
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// 224 Genesis
// ---------------------------------------------------------------------------

#[test]
fn genesis_reproduces_the_accepted_golden_event() {
    // The fixture is the accepted DELIVERY-10 genesis image; the values below are that
    // event's own field values. Equality therefore tests the encoder against accepted
    // evidence rather than against itself.
    let accepted = fixture("genesis-accepted");
    let mut record = genesis();
    // The accepted event's own created_at µs (0x000643d3251bc000) and description.
    record.created_at = 1_763_424_000_000_000;
    record.metadata.description = "Genesis fixture for the draft format vectors.".into();
    assert_eq!(
        encode(|w| write_genesis(w, &record)),
        accepted,
        "genesis must reproduce the accepted golden bytes exactly"
    );
    assert_eq!(
        read_genesis(&mut Reader::new(&accepted), &mut Open::default()).unwrap(),
        record
    );
}

fn hex32(value: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("hex");
    }
    out
}

#[test]
fn genesis_typical_image_matches_its_independent_vector() {
    // `genesis()` is the fixture generator's own record, so the whole image is asserted,
    // not only the parts one test happens to look at.
    let record = genesis();
    assert_eq!(
        encode(|w| write_genesis(w, &record)),
        fixture("genesis-typical")
    );
    assert_eq!(
        read_genesis(
            &mut Reader::new(&fixture("genesis-typical")),
            &mut Open::default()
        )
        .unwrap(),
        record
    );
}

#[test]
fn genesis_is_revision_one_by_construction() {
    let bytes = fixture("genesis-accepted");
    let mut ledger = Open::default();
    let event = event::read_event(&mut Reader::new(&bytes), &mut ledger).unwrap();
    assert_eq!(event.field(2).unwrap(), &1u64.to_le_bytes());
}

#[test]
fn genesis_never_carries_an_actor() {
    // D10 row 224 forbids 5 and 6. The record type has no field for either, so this is a
    // byte-level confirmation rather than a type claim.
    let bytes = fixture("genesis-accepted");
    let mut ledger = Open::default();
    let event = event::read_event(&mut Reader::new(&bytes), &mut ledger).unwrap();
    assert!(event.field(5).is_none(), "genesis must not name an actor");
    assert!(
        event.field(6).is_none(),
        "genesis must not invent an origin"
    );
    assert!(event.field(1).is_none(), "genesis has no record key");

    let with_actor = edit(&bytes, None, None, Some((6, 2u16.to_le_bytes().to_vec())));
    assert!(matches!(
        read_genesis(&mut Reader::new(&with_actor), &mut Open::default()),
        Err(ReadError::Data(DecodeError::ForbiddenField {
            field: 6,
            ..
        }))
    ));
}

#[test]
fn genesis_requires_created_at_despite_the_row_omitting_it() {
    // §6.1 makes tag 8 `must` and the accepted golden event carries it, so the row's
    // 专属必现 column is incomplete. The stricter accepted source wins; reported, not
    // silently resolved.
    let bytes = fixture("genesis-accepted");
    let raw = edit(&bytes, None, Some(8), None);
    assert!(matches!(
        read_genesis(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::MissingField { field: 8, .. }))
    ));
}

#[test]
fn genesis_requires_every_declared_field() {
    let bytes = fixture("genesis-accepted");
    for tag in [2u16, 144, 145, 146, 147, 148, 163, 166, 196] {
        let raw = edit(&bytes, None, Some(tag), None);
        assert!(
            matches!(
                read_genesis(&mut Reader::new(&raw), &mut Open::default()),
                Err(ReadError::Data(DecodeError::MissingField { field, .. })) if field == tag
            ),
            "tag {tag} must be mandatory"
        );
    }
}

#[test]
fn genesis_revision_must_be_one() {
    let bytes = fixture("genesis-accepted");
    let raw = edit(&bytes, None, None, Some((2, 2u64.to_le_bytes().to_vec())));
    assert!(matches!(
        read_genesis(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::WrongRevision {
            event: 224,
            revision: 2
        }))
    ));
}

#[test]
fn genesis_profile_domain_is_closed() {
    let bytes = fixture("genesis-accepted");
    let raw = edit(&bytes, None, None, Some((196, 2u16.to_le_bytes().to_vec())));
    assert!(matches!(
        read_genesis(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: 196,
            value: 2
        }))
    ));
}

#[test]
fn genesis_rejects_a_uuid_that_is_not_sixteen_bytes() {
    let bytes = fixture("genesis-accepted");
    for bad in [vec![0u8; 15], vec![0u8; 17]] {
        let raw = edit(&bytes, None, None, Some((144, bad.clone())));
        let error =
            read_genesis(&mut Reader::new(&raw), &mut Open::default()).expect_err("uuid len");
        assert!(
            matches!(
                error,
                ReadError::Data(DecodeError::Truncated { wanted: 16, .. })
            ),
            "{} bytes: {error:?}",
            bad.len()
        );
    }
}

#[test]
fn genesis_extensions_are_one_value_shared_by_both_halves() {
    let mut record = genesis();
    let mut exts = Extensions::new();
    exts.insert("genesis.note", vec![0x02]).unwrap();
    record.reason = Some("first init".into());
    record.metadata.extensions = exts.clone();
    record.rules.extensions = exts;
    let bytes = encode(|w| write_genesis(w, &record));
    assert_eq!(bytes, fixture("genesis-extended"));
    let decoded = read_genesis(&mut Reader::new(&bytes), &mut Open::default()).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(
        decoded.metadata.extensions, decoded.rules.extensions,
        "one encoded value, two references"
    );
}

#[test]
fn genesis_refuses_two_different_extension_maps() {
    let mut record = genesis();
    record
        .metadata
        .extensions
        .insert("a.one", b"1".to_vec())
        .unwrap();
    record
        .rules
        .extensions
        .insert("b.two", b"2".to_vec())
        .unwrap();
    let mut out = Vec::new();
    let mut ledger = Open::default();
    assert!(
        write_genesis(&mut Writer::new(&mut out, &mut ledger), &record).is_err(),
        "two different maps have no single encoding"
    );
    assert!(out.is_empty());
}

#[test]
fn genesis_string_and_list_bounds_are_enforced() {
    let bytes = fixture("genesis-accepted");
    let cases = [
        (145u16, text(&"s".repeat(65)), 64u32),
        (146, text(&"n".repeat(601)), 600),
        (147, text(&"d".repeat(8001)), 8000),
    ];
    for (tag, value, max) in cases {
        let raw = edit(&bytes, None, None, Some((tag, value)));
        assert!(
            matches!(
                read_genesis(&mut Reader::new(&raw), &mut Open::default()),
                Err(ReadError::Data(DecodeError::StringTooLong { max: seen, .. })) if seen == max
            ),
            "tag {tag}"
        );
    }

    // A domain element beyond 128 bytes, and more than 64 domains, are both limits.
    let mut list = 1u32.to_le_bytes().to_vec();
    list.extend_from_slice(&text(&"d".repeat(129)));
    let raw = edit(&bytes, None, None, Some((148, list)));
    assert!(matches!(
        read_genesis(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::StringTooLong { max: 128, .. }))
    ));

    let mut many = 65u32.to_le_bytes().to_vec();
    for _ in 0..65 {
        many.extend_from_slice(&text("d"));
    }
    let raw = edit(&bytes, None, None, Some((148, many)));
    assert!(matches!(
        read_genesis(&mut Reader::new(&raw), &mut Open::default()),
        Err(ReadError::Data(DecodeError::CountTooLong { max: 64, .. }))
    ));
}

#[test]
fn genesis_refuses_record_fields_it_does_not_own() {
    let bytes = fixture("genesis-accepted");
    for tag in [4u16, 9, 13] {
        let value = match tag {
            4 => 0u16.to_le_bytes().to_vec(),
            9 => 1i64.to_le_bytes().to_vec(),
            _ => 1u32.to_le_bytes().to_vec(),
        };
        let raw = edit(&bytes, None, None, Some((tag, value)));
        assert!(
            matches!(
                read_genesis(&mut Reader::new(&raw), &mut Open::default()),
                Err(ReadError::Data(DecodeError::ForbiddenField { field, .. })) if field == tag
            ),
            "tag {tag}"
        );
    }
}

#[test]
fn genesis_decode_observes_a_cancelled_attempt() {
    let bytes = fixture("genesis-accepted");
    assert!(matches!(
        read_genesis(&mut Reader::new(&bytes), &mut Cancelled),
        Err(ReadError::Stopped(ControlStop::Cancelled))
    ));
}

#[test]
fn genesis_encode_refuses_before_publishing_under_a_zero_budget() {
    let mut out = Vec::new();
    let mut ledger = ByteBudget(0);
    assert!(write_genesis(&mut Writer::new(&mut out, &mut ledger), &genesis()).is_err());
    assert!(out.is_empty());
}

#[test]
fn structural_readers_refuse_each_other() {
    let allocation_bytes = fixture("idalloc-zero");
    assert!(matches!(
        read_genesis(&mut Reader::new(&allocation_bytes), &mut Open::default()),
        Err(ReadError::Uninterpretable(_))
    ));
    let genesis_bytes = fixture("genesis-accepted");
    assert!(matches!(
        read_id_allocation(&mut Reader::new(&genesis_bytes), &mut Open::default()),
        Err(ReadError::Uninterpretable(_))
    ));
}

#[test]
fn genesis_decodes_into_records_whose_actor_is_absent() {
    // The projection is where "no actor" becomes a fact rather than a default: the
    // records say actor is None, which is different from Unavailable.
    let record = read_genesis(
        &mut Reader::new(&fixture("genesis-accepted")),
        &mut Open::default(),
    )
    .expect("decode");
    let metadata = record.metadata_record();
    let rules = record.rules_record();
    assert_eq!(metadata.revision, RecordRevision::FIRST);
    assert_eq!(rules.revision, RecordRevision::FIRST);
    assert!(
        metadata.audit.actor.is_none(),
        "genesis states no actor at all"
    );
    assert!(rules.audit.actor.is_none());
    assert!(metadata.audit.recorded_by.is_none());
    assert!(rules.from_profile.is_none(), "a genesis migrates nothing");
}
