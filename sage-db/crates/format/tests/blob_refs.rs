//! Blob-reference enumeration against accepted bytes, not against the helper's own idea.
//!
//! Source of the counterexample: coordinator probe
//! `parallel-01/rechecks/boundary-v3/probe/tests/rules_blob.rs`, which found the rules
//! blob (tag 166) silently omitted from an accepted Genesis event while the helper's own
//! documentation claimed rules were included.
use sage_db_format::{
    event::{read_event, read_header},
    for_each_blob_ref, is_blob_ref_tag,
};
use sage_db_types::{
    framing::{BlobRefSink, Refusal},
    ledger::{BudgetError, SharedLedger},
    query::ControlStop,
};

struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

#[derive(Default)]
struct Refs(Vec<(u16, [u8; 32])>);

impl BlobRefSink for Refs {
    fn push(&mut self, tag: u16, sha: [u8; 32], _: &mut dyn SharedLedger) -> Result<(), Refusal> {
        self.0.push((tag, sha));
        Ok(())
    }
}

/// The immutable D10 genesis transaction, bundled by the file-codec fixtures.
fn genesis_body() -> Vec<u8> {
    let tx = std::fs::read("tests/fixtures/file-codec/golden.genesis.tx.sgl")
        .expect("mandatory fixture is bundled, not optional");
    assert_eq!(&tx[..4], b"SGLB", "fixture is a real SGLB file");
    let len = u32::from_le_bytes(tx[36..40].try_into().unwrap()) as usize;
    tx[48..48 + len].to_vec()
}

#[test]
fn the_accepted_genesis_rules_blob_is_enumerated() {
    let body = genesis_body();
    let mut reader = sage_db_format::wire::Reader::new(&body);
    read_header(&mut reader, &mut Open).expect("header");
    let start = reader.offset();
    let event = read_event(&mut reader, &mut Open).expect("event");
    assert_eq!(event.tag(), 224, "Genesis");

    // Positive control: the real family decoder accepts these bytes, so the fixture is
    // a legal event and the assertion below is about enumeration, not about validity.
    sage_db_format::structural::read_genesis(
        &mut sage_db_format::wire::Reader::new(&body[start..]),
        &mut Open,
    )
    .expect("real family decoder accepts the accepted golden event");

    // The event really carries tag 166; take the expected value from the bytes.
    let expected: [u8; 32] = event
        .fields()
        .iter()
        .find(|f| f.tag() == 166)
        .expect("accepted Genesis carries rules_ref")
        .value()
        .try_into()
        .expect("32-byte digest");

    let mut refs = Refs::default();
    for_each_blob_ref(&event, &mut refs, &mut Open).expect("enumeration");

    // Compare the WHOLE set, so a future removal of another intentional exclusion is
    // also visible instead of hiding behind an "is 166 present" check.
    let mut got: Vec<u16> = refs.0.iter().map(|(tag, _)| *tag).collect();
    got.sort_unstable();
    let mut want: Vec<u16> = event
        .fields()
        .iter()
        .map(|f| f.tag())
        .filter(|tag| is_blob_ref_tag(*tag))
        .collect();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "the enumerator and the tag predicate must agree on the same event"
    );
    assert!(
        refs.0.contains(&(166, expected)),
        "rules_ref 166 was omitted: {:?}",
        refs.0
    );
}

#[test]
fn digest_only_and_key_tags_are_not_fetched_as_blobs() {
    // Each of these is a digest OF a value or a key, not an address to open.
    for tag in [84u16, 100, 178, 12, 169, 184] {
        assert!(!is_blob_ref_tag(tag), "tag {tag} must not be enumerated");
    }
    for tag in [23u16, 50, 67, 68, 101, 115, 129, 133, 162, 166, 180, 185] {
        assert!(is_blob_ref_tag(tag), "tag {tag} is a blob reference");
    }
}

/// A sink that records nothing but counts pushes.
#[derive(Default)]
struct Quiet(usize);

impl BlobRefSink for Quiet {
    fn push(&mut self, _: u16, _: [u8; 32], _: &mut dyn SharedLedger) -> Result<(), Refusal> {
        self.0 += 1;
        Ok(())
    }
}

/// Cancels at the first checkpoint, admitting everything else.
struct Cancels;

impl SharedLedger for Cancels {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

#[test]
fn cancellation_is_observed_even_when_no_reference_exists() {
    // The defect this asserts against: an enumerator with no entry checkpoint returns Ok for
    // an event carrying no refs, so "no refs" and "the caller cancelled" are indistinguishable.
    // A field list with no ref tags pushes nothing, which is exactly the case that hid it.
    let no_refs = [sage_db_format::event::Field::new(17, b"atom")];
    let mut quiet = Quiet::default();
    match sage_db_format::dispatch::for_each_blob_ref_fields(&no_refs, &mut quiet, &mut Cancels) {
        Err(Refusal::Stopped(_)) => {}
        other => panic!("a cancelled caller must be observed on an empty set: {other:?}"),
    }
    assert_eq!(quiet.0, 0, "no references should have been pushed");

    // And an empty field list likewise.
    let empty: [sage_db_format::event::Field<'_>; 0] = [];
    let mut quiet = Quiet::default();
    match sage_db_format::dispatch::for_each_blob_ref_fields(&empty, &mut quiet, &mut Cancels) {
        Err(Refusal::Stopped(_)) => {}
        other => panic!("a cancelled caller must be observed on an empty list: {other:?}"),
    }
}

#[test]
fn a_deadline_is_reported_as_a_stop_not_a_classification() {
    struct Deadline;
    impl SharedLedger for Deadline {
        fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Err(ControlStop::DeadlineExceeded)
        }
    }
    let no_refs = [sage_db_format::event::Field::new(17, b"atom")];
    let mut quiet = Quiet::default();
    match sage_db_format::dispatch::for_each_blob_ref_fields(&no_refs, &mut quiet, &mut Deadline) {
        Err(Refusal::Stopped(ControlStop::DeadlineExceeded)) => {}
        other => panic!("deadline must stay a stop: {other:?}"),
    }
}

#[test]
fn references_are_still_enumerated_when_the_caller_continues() {
    // Positive control: the entry checkpoint must not swallow the work.
    let refs = [sage_db_format::event::Field::new(23, &[7u8; 32])];
    let mut quiet = Quiet::default();
    sage_db_format::dispatch::for_each_blob_ref_fields(&refs, &mut quiet, &mut Open)
        .expect("continuing caller");
    assert_eq!(quiet.0, 1, "the real reference must still be pushed");
}
