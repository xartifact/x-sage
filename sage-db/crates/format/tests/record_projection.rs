//! Canonical state-body projection: `storage-digests` §4.4.3 / §4.4.6.
//!
//! Fixtures are the **real D10 events** extracted from the accepted golden transactions, one
//! event envelope per file, so a projection expectation is never produced by the projection
//! being tested. The digests the projected entries feed are the accepted values from §7.4-7.5b.
//!
//! The positive chain is: real event -> `project_event` -> entries -> `encode_state_payload`
//! -> the accepted `state_digest`. That end-to-end equality is what proves the body bytes, the
//! keys and the revisions together; a unit assertion on one of them could not.

#![allow(clippy::indexing_slicing)]

use sage_db_format::event::read_event;
use sage_db_format::projection::{ProjectionError, project_event};
use sage_db_format::state::state_digest;
use sage_db_format::wire::Reader;
use sage_db_types::framing::{FormatOutcome, Refusal};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::persist::{RecordSet, StateEntry};

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

/// Refuses the first byte charge.
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

/// Counts what was admitted, so the resource claims are measured rather than asserted.
#[derive(Default)]
struct Counted {
    bytes: u64,
    entries: u64,
}

impl SharedLedger for Counted {
    fn try_charge(&mut self, b: u64) -> Result<(), BudgetError> {
        self.bytes += b;
        Ok(())
    }
    fn try_charge_entries(&mut self, c: u64) -> Result<(), BudgetError> {
        self.entries += c;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/record-projection")
}

fn event_bytes(name: &str) -> Vec<u8> {
    let path = dir().join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("missing mandatory fixture {}: {e}", path.display()))
}

/// Decode one fixture event and project it.
fn project_fixture(name: &str, out: &mut Vec<StateEntry>) -> FormatOutcome<usize> {
    let bytes = event_bytes(name);
    let mut ledger = Open;
    let event = match read_event(&mut Reader::new(&bytes), &mut ledger) {
        Ok(event) => event,
        Err(error) => panic!("{name}: fixture must decode: {error:?}"),
    };
    project_event(event.tag(), event.fields(), out, &mut Open)
}

fn describe<T>(outcome: &FormatOutcome<T>) -> String {
    match outcome {
        FormatOutcome::Ok(_) => "Ok".to_string(),
        FormatOutcome::Format(e) => format!("Format({e:?})"),
        FormatOutcome::Refused(r) => format!("Refused({r:?})"),
    }
}

/// Build a `RecordSet` from entries in canonical `(kind, key)` order.
fn record_set(entries: Vec<StateEntry>, ledger: &mut dyn SharedLedger) -> RecordSet {
    let mut sorted = entries;
    // The STORE owns ordering; for a test the expected order is the canonical one.
    sorted.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    RecordSet::from_sorted(sorted, ledger).expect("canonical order")
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// The positive chain: real events -> projection -> accepted digests
// ---------------------------------------------------------------------------

/// §7.4: `Genesis` alone yields two entries — KbMetadata(12) with a 175-byte body and
/// RuleConfig(13) with a 122-byte body — and together they hash to the accepted value.
#[test]
fn genesis_projects_to_two_entries_matching_the_accepted_digest() {
    let mut entries = Vec::new();
    let count = project_fixture("genesis.e0.tag224.bin", &mut entries);
    assert_eq!(
        count,
        FormatOutcome::Ok(2),
        "Genesis emits exactly two entries"
    );
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].kind, 12, "KbMetadata first");
    assert_eq!(entries[0].key, Vec::<u8>::new(), "singleton key is empty");
    assert_eq!(entries[0].revision, 1);
    assert_eq!(entries[0].body.len(), 175, "quoted KbMetadata body length");

    assert_eq!(entries[1].kind, 13, "RuleConfig second");
    assert_eq!(entries[1].key, Vec::<u8>::new());
    assert_eq!(entries[1].revision, 1);
    assert_eq!(entries[1].body.len(), 122, "quoted RuleConfig body length");

    let set = record_set(entries, &mut Open);
    let digest = state_digest(&set, &mut Open).expect("digest");
    assert_eq!(
        hex_of(&digest),
        "b6422a4ac1ef53598c2259991facb9bf16079b479aee20c97ced428859aacc88",
        "accepted genesis state_digest from the projected entries"
    );
}

/// §4.4.6 is a normative table, so it must be visible in the bodies themselves: tag 163 belongs
/// to RuleConfig only, and the shared tags appear in both.
#[test]
fn genesis_split_puts_rule_digest_only_in_rule_config() {
    let mut entries = Vec::new();
    assert_eq!(
        project_fixture("genesis.e0.tag224.bin", &mut entries),
        FormatOutcome::Ok(2)
    );

    fn tags(body: &[u8]) -> Vec<u16> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < body.len() {
            let tag = u16::from_le_bytes([body[at], body[at + 1]]);
            let len = u32::from_le_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]);
            out.push(tag);
            at += 8 + len as usize;
        }
        out
    }
    let kb = tags(&entries[0].body);
    let rules = tags(&entries[1].body);
    assert!(!kb.contains(&163), "163 must not enter KbMetadata: {kb:?}");
    assert!(rules.contains(&163), "163 belongs to RuleConfig: {rules:?}");
    for shared in [2u16, 8] {
        assert!(
            kb.contains(&shared) && rules.contains(&shared),
            "tag {shared} is shared"
        );
    }
    // Tags 1 and 13 never appear in any body.
    for body_tags in [&kb, &rules] {
        assert!(!body_tags.contains(&1), "tag 1 is Entry.kind+key");
        assert!(!body_tags.contains(&13), "tag 13 is reconstructible");
    }
}

/// The shared Genesis tags must be **byte-identical** in both bodies (§4.4.6). This is the
/// invariant most easily broken by a future edit that copies a value instead of referencing one.
#[test]
fn genesis_shared_tags_are_byte_identical_in_both_bodies() {
    let mut entries = Vec::new();
    assert_eq!(
        project_fixture("genesis.e0.tag224.bin", &mut entries),
        FormatOutcome::Ok(2)
    );

    fn value_of(body: &[u8], want: u16) -> Option<Vec<u8>> {
        let mut at = 0;
        while at < body.len() {
            let tag = u16::from_le_bytes([body[at], body[at + 1]]);
            let len = u32::from_le_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]);
            let value = body[at + 8..at + 8 + len as usize].to_vec();
            if tag == want {
                return Some(value);
            }
            at += 8 + len as usize;
        }
        None
    }
    for shared in [2u16, 7, 8, 14] {
        let kb = value_of(&entries[0].body, shared);
        let rules = value_of(&entries[1].body, shared);
        assert_eq!(
            kb, rules,
            "shared tag {shared} must be byte-identical in both Genesis bodies"
        );
    }
}

/// §7.5: a v2 `TermCreated` yields one entry whose key is the `u32` id LE, revision from tag 2,
/// and whose body retains every field except 1 and 13.
#[test]
fn v2_term_projects_to_one_entry_including_inherited_singletons() {
    let mut entries = Vec::new();
    assert_eq!(
        project_fixture("v2.e0.tag1.bin", &mut entries),
        FormatOutcome::Ok(1)
    );
    assert_eq!(entries[0].kind, 1);
    assert_eq!(entries[0].key, 1u32.to_le_bytes().to_vec(), "u32 id LE");
    assert_eq!(entries[0].revision, 1);

    // The accepted v2 digest covers the term PLUS the inherited singletons, so build the whole
    // set the way the fold would: genesis entries, then this one.
    let mut all = Vec::new();
    assert_eq!(
        project_fixture("genesis.e0.tag224.bin", &mut all),
        FormatOutcome::Ok(2)
    );
    all.extend(entries);
    let set = record_set(all, &mut Open);
    assert_eq!(
        hex_of(&state_digest(&set, &mut Open).expect("digest")),
        "381bd739b2978e45f457dcb0452fe88d96d8c653a8b620ad018ec4d638c89ad4"
    );
}

/// §7.5b: the revised term carries the complete after-image, and an updated KbMetadata leaves
/// the inherited RuleConfig untouched — so the whole set must reproduce the accepted v3 value.
#[test]
fn v3_revision_and_metadata_update_reproduce_the_accepted_digest() {
    let mut all = Vec::new();
    assert_eq!(
        project_fixture("genesis.e0.tag224.bin", &mut all),
        FormatOutcome::Ok(2)
    );

    let mut term = Vec::new();
    assert_eq!(
        project_fixture("v3.e0.tag2.bin", &mut term),
        FormatOutcome::Ok(1)
    );
    assert_eq!(term[0].kind, 1);
    assert_eq!(term[0].revision, 2, "revision comes from the event");

    let mut kb = Vec::new();
    assert_eq!(
        project_fixture("v3.e1.tag144.bin", &mut kb),
        FormatOutcome::Ok(1)
    );
    assert_eq!(kb[0].kind, 12);
    assert_eq!(kb[0].revision, 2);

    // Fold in the store's terms: the newer revision REPLACES the older one for kind 1 and
    // kind 12, and RuleConfig(13) is inherited from genesis unchanged at revision 1. This
    // replacement is the store's job, not the projection's — the projection appends what each
    // event says and never dedups or sorts.
    let mut genesis = Vec::new();
    assert_eq!(
        project_fixture("genesis.e0.tag224.bin", &mut genesis),
        FormatOutcome::Ok(2)
    );
    let rules_entry = genesis
        .iter()
        .find(|e| e.kind == 13)
        .cloned()
        .expect("genesis carries RuleConfig");

    // kind 1 rev 2, kind 12 rev 2, kind 13 rev 1 inherited
    let state: Vec<StateEntry> = vec![term.remove(0), kb.remove(0), rules_entry];
    let set = record_set(state, &mut Open);
    assert_eq!(
        hex_of(&state_digest(&set, &mut Open).expect("digest")),
        "ca16fcb6337bd3d8e9f2e709a1a74c11f95b39f3bcd440ad7cc7b4d04cb5abf7"
    );
}

/// §4.4.4: Asset is content-addressed and immutable, so its revision is the explicit constant
/// `1` — not the event's tag-2 value (which an Asset event may not even carry), and its key is
/// the raw 32-byte `sha256`.
#[test]
fn asset_uses_the_constant_revision_and_the_sha256_as_key() {
    let mut entries = Vec::new();
    assert_eq!(
        project_fixture("asset.e0.tag64.bin", &mut entries),
        FormatOutcome::Ok(1)
    );
    let entry = &entries[0];
    assert_eq!(entry.kind, 10, "Asset state kind");
    assert_eq!(
        entry.revision, 1,
        "immutable: the constant 1, never an event value"
    );
    assert_eq!(
        entry.key,
        (0u8..32).collect::<Vec<u8>>(),
        "the key is the raw sha256, not a length-prefixed string"
    );
    // The body retains the field set minus 1/13; Asset has no record_key, so nothing is dropped
    // for that reason, and there is no tag 13 to drop.
    assert!(!entry.body.is_empty());
    let first_tag = u16::from_le_bytes([entry.body[0], entry.body[1]]);
    assert_eq!(first_tag, 6, "lowest present tag is actor_origin");
}

// ---------------------------------------------------------------------------
// Zero-entry events: bookkeeping only
// ---------------------------------------------------------------------------

/// `192`, `208` and `176` are `bookkeeping_digest` inputs, not knowledge records.
#[test]
fn bookkeeping_events_project_to_zero_entries() {
    for name in [
        "v2.e2.tag192.bin",
        "v3.e3.tag192.bin",
        "genesis.e1.tag176.bin",
    ] {
        let mut entries = Vec::new();
        let outcome = project_fixture(name, &mut entries);
        assert_eq!(
            outcome,
            FormatOutcome::Ok(0),
            "{name} contributes no state entry"
        );
        assert!(entries.is_empty(), "{name} appended nothing");
    }
}

// ---------------------------------------------------------------------------
// Refusals and control
// ---------------------------------------------------------------------------

#[test]
fn refusals_are_never_reported_as_bytes_verdicts() {
    let bytes = event_bytes("genesis.e0.tag224.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
    let (tag, fields) = (event.tag(), event.fields().to_vec());

    match project_event(tag, &fields, &mut Vec::new(), &mut NoBytes) {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        other => panic!(
            "zero budget must be Refused(Budget), got {}",
            describe(&other)
        ),
    }
    match project_event(tag, &fields, &mut Vec::new(), &mut Halt) {
        FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled)) => {}
        other => panic!(
            "cancellation must be Refused(Stopped), got {}",
            describe(&other)
        ),
    }
}

/// The admission claims are measured: one entry per emitted record, and the exact key+body bytes.
#[test]
fn admissions_are_charged_for_the_entries_produced() {
    let bytes = event_bytes("genesis.e0.tag224.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
    let (tag, fields) = (event.tag(), event.fields().to_vec());

    let mut counted = Counted::default();
    let mut entries = Vec::new();
    assert_eq!(
        project_event(tag, &fields, &mut entries, &mut counted),
        FormatOutcome::Ok(2)
    );
    // Twelve kept fields in the KB body plus twelve in the rules body, as retained slots.
    assert!(
        counted.entries >= 2,
        "one entry admission per emitted record"
    );
    let owned: u64 = entries
        .iter()
        .map(|e| (e.key.len() + e.body.len()) as u64)
        .sum();
    assert!(
        counted.bytes >= owned,
        "the exact key+body extents must be admitted: charged {} for {} owned",
        counted.bytes,
        owned
    );
}

/// A field stream the real validator rejects must not be projected: the seam is called first.
#[test]
fn an_invalid_event_is_refused_by_the_real_validator() {
    let bytes = event_bytes("v2.e0.tag1.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
    // Drop `17 atom`, which row 1 requires.
    let mut fields: Vec<_> = event.fields().to_vec();
    fields.retain(|f| f.tag() != 17);
    let mut out = Vec::new();
    let outcome = project_event(event.tag(), &fields, &mut out, &mut Open);
    assert!(
        !matches!(outcome, FormatOutcome::Ok(_)),
        "a record missing a required field must not project: {}",
        describe(&outcome)
    );
    assert!(out.is_empty(), "nothing is appended for a refused event");
}

/// Sanity on the error type: the refusal arms are distinct from the input arms.
#[test]
fn projection_error_classes_stay_distinct() {
    let budget = ProjectionError::Budget(BudgetError::new(BudgetUnit::Bytes, 4, 1));
    let stop = ProjectionError::Stopped(ControlStop::Cancelled);
    let malformed = ProjectionError::MalformedFields { tag: 1 };
    assert_ne!(budget, stop);
    assert_ne!(budget, malformed);
    assert_ne!(stop, malformed);
}

// ---------------------------------------------------------------------------
// Blob references from a raw field slice
// ---------------------------------------------------------------------------

/// A sink that records what it was handed, admitting per reference through the caller's ledger.
struct Recording {
    refs: Vec<(u16, [u8; 32])>,
}

impl sage_db_types::framing::BlobRefSink for Recording {
    fn push(
        &mut self,
        tag: u16,
        sha256: [u8; 32],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        // The sink is where retained storage grows, so the admission belongs here.
        ledger.try_charge_entries(1)?;
        ledger.try_charge(32)?;
        ledger.checkpoint()?;
        self.refs.push((tag, sha256));
        Ok(())
    }
}

/// The raw-field enumerator must agree exactly with the `Event`-taking authority on the same
/// real event. Field-level agreement is what makes it usable without a policy duplicate.
#[test]
fn raw_field_blob_refs_match_the_event_path_exactly() {
    for name in [
        "v2.e0.tag1.bin",
        "v3.e0.tag2.bin",
        "v3.e1.tag144.bin",
        "genesis.e0.tag224.bin",
        "asset.e0.tag64.bin",
        "v2.e2.tag192.bin",
    ] {
        let bytes = event_bytes(name);
        let mut ledger = Open;
        let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");

        let mut via_event = Recording { refs: Vec::new() };
        sage_db_format::dispatch::for_each_blob_ref(&event, &mut via_event, &mut Open)
            .expect("event path");

        let mut via_fields = Recording { refs: Vec::new() };
        sage_db_format::projection::for_each_blob_ref_fields(
            event.fields(),
            &mut via_fields,
            &mut Open,
        )
        .expect("field path");

        assert_eq!(
            via_event.refs, via_fields.refs,
            "{name}: the two enumerations must not diverge"
        );
    }
}

/// Two specific rules the seam fixes, asserted so a future edit cannot quietly widen them.
#[test]
fn blob_ref_enumeration_keeps_its_documented_exclusions() {
    // An Asset's `84 sha256` IS the key, not a reference to fetch.
    let asset = event_bytes("asset.e0.tag64.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&asset), &mut ledger).expect("decodes");
    let mut sink = Recording { refs: Vec::new() };
    sage_db_format::projection::for_each_blob_ref_fields(event.fields(), &mut sink, &mut Open)
        .expect("enumeration");
    assert!(
        sink.refs.iter().all(|(tag, _)| *tag != 84),
        "84 sha256 must never be emitted as a blob reference"
    );

    // A Term's `23 body_ref` is a reference, and is forwarded.
    let term = event_bytes("v2.e0.tag1.bin");
    let event = read_event(&mut Reader::new(&term), &mut ledger).expect("decodes");
    let mut sink = Recording { refs: Vec::new() };
    sage_db_format::projection::for_each_blob_ref_fields(event.fields(), &mut sink, &mut Open)
        .expect("enumeration");
    assert!(
        sink.refs.iter().any(|(tag, _)| *tag == 23),
        "23 body_ref must be emitted: {:?}",
        sink.refs.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );
}

/// ENTRIES and BYTES are different units. A caller whose `out` already has spare capacity still
/// owes one ENTRY for the record being appended — prior capacity is a byte credit, not prepaid
/// work — while owing slot BYTES only for capacity actually added.
///
/// The first half is the discriminating case: with the entry allowance exhausted at the output
/// stage and `out` pre-allocated, a projection that charged entries only for capacity growth
/// would succeed, because no growth happens.
#[test]
fn preallocated_out_still_pays_one_entry_per_record() {
    let bytes = event_bytes("genesis.e0.tag224.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
    let (tag, fields) = (event.tag(), event.fields().to_vec());

    // A ledger that allows byte charges but exactly N entries.
    struct EntriesOnly(u64);
    impl SharedLedger for EntriesOnly {
        fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, c: u64) -> Result<(), BudgetError> {
            if c > self.0 {
                return Err(BudgetError::new(BudgetUnit::Entries, c, self.0));
            }
            self.0 -= c;
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    // `out` already has room for both Genesis entries, so the output stage adds NO capacity.
    // That is what makes this discriminating: a projection that charged entries only for
    // capacity growth would charge nothing here. The ledger records every entry charge so the
    // assertion can name what was actually taken.
    struct Watched {
        seen: Vec<u64>,
    }
    impl SharedLedger for Watched {
        fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, c: u64) -> Result<(), BudgetError> {
            self.seen.push(c);
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }
    let mut out: Vec<StateEntry> = Vec::with_capacity(2);
    let mut watched = Watched { seen: Vec::new() };
    assert_eq!(
        project_event(tag, &fields, &mut out, &mut watched),
        FormatOutcome::Ok(2)
    );
    assert_eq!(out.len(), 2, "both Genesis records were appended");
    // Two records appended into capacity that already existed: each still owes ONE entry, so at
    // least two unit charges must have been made at the output stage.
    let unit_charges = watched.seen.iter().filter(|c| **c == 1).count();
    assert!(
        unit_charges >= 2,
        "each appended record owes one entry into pre-allocated capacity; charges were {:?}",
        watched.seen
    );

    // With enough allowance the projection succeeds and both entries land. The allowance must
    // cover the FIELD TABLES too, not just the two output slots: a body is a retained
    // reference table as well, and the budget is per-attempt, not per-kind.
    let mut out: Vec<StateEntry> = Vec::with_capacity(2);
    let mut allowed = EntriesOnly(64);
    assert_eq!(
        project_event(tag, &fields, &mut out, &mut allowed),
        FormatOutcome::Ok(2)
    );
    assert_eq!(out.len(), 2);
    assert!(
        allowed.0 < 64,
        "entry charges were taken for the output records as well as the field tables"
    );
}

/// Normal growth: the entry count tracks appended records, and slot bytes track capacity added,
/// so the two counters move independently.
#[test]
fn entry_and_byte_counters_move_independently() {
    let bytes = event_bytes("v2.e0.tag1.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
    let (tag, fields) = (event.tag(), event.fields().to_vec());

    #[derive(Default)]
    struct Split {
        entries: u64,
        bytes: u64,
    }
    impl SharedLedger for Split {
        fn try_charge(&mut self, b: u64) -> Result<(), BudgetError> {
            self.bytes += b;
            Ok(())
        }
        fn try_charge_entries(&mut self, c: u64) -> Result<(), BudgetError> {
            self.entries += c;
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    // Growing buffer: one record, so exactly one output entry charge on top of the field table
    // and blob-less body work.
    let mut out: Vec<StateEntry> = Vec::new();
    let mut counted = Split::default();
    assert_eq!(
        project_event(tag, &fields, &mut out, &mut counted),
        FormatOutcome::Ok(1)
    );
    assert_eq!(out.len(), 1);
    assert!(
        counted.entries >= 1,
        "at least the appended record was charged"
    );
    assert!(counted.bytes > 0, "slot and key bytes were charged");

    // Pre-allocated buffer: the same record still costs one entry, but no slot bytes at all for
    // capacity, so the byte total must not increase by a slot.
    let mut out: Vec<StateEntry> = Vec::with_capacity(1);
    let mut counted = Split::default();
    assert_eq!(
        project_event(tag, &fields, &mut out, &mut counted),
        FormatOutcome::Ok(1)
    );
    assert!(
        counted.entries >= 1,
        "a pre-allocated out still owes the entry"
    );
}

// ---------------------------------------------------------------------------
// Blob-ref enumeration: control on empty operations
// ---------------------------------------------------------------------------

/// A cancelled or expired ledger must be observed even when the event declares no references.
///
/// This is the case the helper got wrong: with no ref tags present the loop body never ran, so
/// nothing called `checkpoint` and the function returned `Ok` — an empty operation reporting
/// success without ever consulting the caller's policy.
#[test]
fn blob_ref_enumeration_observes_control_without_any_references() {
    struct Stopping(ControlStop);
    impl SharedLedger for Stopping {
        fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Err(self.0)
        }
    }

    // An Asset event declares no blob references at all (`84 sha256` is its key).
    let asset = event_bytes("asset.e0.tag64.bin");
    let mut ledger = Open;
    let event = read_event(&mut Reader::new(&asset), &mut ledger).expect("decodes");
    let mut sink = Recording { refs: Vec::new() };

    for stop in [ControlStop::Cancelled, ControlStop::DeadlineExceeded] {
        let mut stopping = Stopping(stop);
        match sage_db_format::projection::for_each_blob_ref_fields(
            event.fields(),
            &mut sink,
            &mut stopping,
        ) {
            Err(Refusal::Stopped(got)) if got == stop => {}
            other => panic!("an empty reference set must still observe {stop:?}, got {other:?}"),
        }
    }
    assert!(sink.refs.is_empty(), "no reference was emitted");

    // Also true for a field slice with no fields at all.
    let mut stopping = Stopping(ControlStop::Cancelled);
    match sage_db_format::projection::for_each_blob_ref_fields(&[], &mut sink, &mut stopping) {
        Err(Refusal::Stopped(ControlStop::Cancelled)) => {}
        other => panic!("an empty field slice must observe cancellation, got {other:?}"),
    }
}

/// The positive control: an event that DOES declare references yields exactly those, in order,
/// so the control check above is not hiding a function that emits nothing.
#[test]
fn blob_ref_enumeration_emits_the_exact_references_in_order() {
    // v3's TermRevised carries `23 body_ref`; the golden genesis carries `166 rules_ref`.
    for (name, expected_tag) in [("v3.e0.tag2.bin", 23u16), ("genesis.e0.tag224.bin", 166)] {
        let bytes = event_bytes(name);
        let mut ledger = Open;
        let event = read_event(&mut Reader::new(&bytes), &mut ledger).expect("decodes");
        let mut sink = Recording { refs: Vec::new() };
        sage_db_format::projection::for_each_blob_ref_fields(event.fields(), &mut sink, &mut Open)
            .expect("enumeration");

        assert_eq!(sink.refs.len(), 1, "{name}: exactly one reference expected");
        assert_eq!(sink.refs[0].0, expected_tag, "{name}: reference tag");
        // The emitted bytes are the field's own value, not a re-encoding.
        let raw = event
            .fields()
            .iter()
            .find(|f| f.tag() == expected_tag)
            .map(|f| f.value())
            .expect("field present");
        assert_eq!(sink.refs[0].1, <[u8; 32]>::try_from(raw).expect("32 bytes"));
    }
}
