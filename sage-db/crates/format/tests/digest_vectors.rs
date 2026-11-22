//! Digest-assembly vectors: `state_digest` (§4.4), `graph_digest` (§4.5) and
//! `bookkeeping_digest` (§4.6).
//!
//! Expected bytes are the frozen payloads in `fixtures/digest-assembly`, derived from the
//! accepted D10 `golden.*.tx.sgl` binaries by an independent script (see `PROVENANCE.md`)
//! — not from the encoder under test. The nine accepted digest values are quoted from
//! `storage-digests.md` §7.4–7.5b and cross-checked against `gen_vectors.py`'s own output.
//!
//! Every `.mutant.bin` is a discriminator: the same payload with exactly one rule changed,
//! so a wrong-but-plausible assembler fails a specific assertion instead of passing because
//! it is self-consistent. `.synthetic.bin` covers rules no D10 vector exercises (two record
//! keys in one kind, an import row, a `2^64` watermark) and says so in its name.

#![allow(clippy::indexing_slicing)]

use sage_db_format::bookkeeping::{bookkeeping_digest, encode_bookkeeping_payload};
use sage_db_format::graph::{encode_graph_payload, graph_digest};
use sage_db_format::state::{
    DigestAssemblyError, StructureDefect, encode_state_payload, state_digest,
};
use sage_db_types::confidence::Confidence;
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, CountPair, CreationAudit, Extensions, ImportSessionRecord, ImportStage, RecordSet,
    RecordSetError, RecordSetRejection, RequestOutcomeEvent, StateEntry, StateKind, Watermark,
};
use sage_db_types::query::ControlStop;
use sage_db_types::record::{AssertionRecord, TermMeta};
use sage_db_types::relation::RelationId;
use sage_db_types::{AssertionId, IdSpace, TermId};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("fixtures/digest-assembly/", $name))
    };
}

/// A ledger that records every charge and can be told to refuse or to stop.
struct Recorder {
    bytes_left: u64,
    entries_left: u64,
    stop: Option<ControlStop>,
    byte_charges: Vec<u64>,
}

impl Recorder {
    fn open() -> Self {
        Self {
            bytes_left: u64::MAX,
            entries_left: u64::MAX,
            stop: None,
            byte_charges: Vec::new(),
        }
    }

    fn with_bytes(bytes: u64) -> Self {
        Self {
            bytes_left: bytes,
            ..Self::open()
        }
    }

    fn stopping(stop: ControlStop) -> Self {
        Self {
            stop: Some(stop),
            ..Self::open()
        }
    }
}

impl SharedLedger for Recorder {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.bytes_left {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes_left));
        }
        self.bytes_left -= bytes;
        self.byte_charges.push(bytes);
        Ok(())
    }

    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        if count > self.entries_left {
            return Err(BudgetError::new(
                BudgetUnit::Entries,
                count,
                self.entries_left,
            ));
        }
        self.entries_left -= count;
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        match self.stop {
            Some(stop) => Err(stop),
            None => Ok(()),
        }
    }
}

fn revision(value: u64) -> RecordRevision {
    RecordRevision::new(value).expect("non-zero revision")
}

fn entry(kind: u16, key: &[u8], revision: u64, body: &[u8]) -> StateEntry {
    StateEntry {
        kind,
        key: key.to_vec(),
        revision,
        body: body.to_vec(),
    }
}

fn set(entries: Vec<StateEntry>) -> RecordSet {
    let mut ledger = Recorder::open();
    RecordSet::from_sorted(entries, &mut ledger).expect("fixtures are in canonical order")
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The §2 domain framing, written out here so a wrong payload cannot be excused by a
/// wrong-but-shared helper.
fn domain_digest(domain: &str, payload: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"sage-db/v1/");
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    hasher.update((payload.len() as u64).to_be_bytes());
    hasher.update(payload);
    hasher.finalize().into()
}

fn audit() -> CreationAudit {
    CreationAudit {
        recorded_by: None,
        actor_origin: ActorOrigin::Unavailable,
        reason: None,
        created_at: 0,
    }
}

// ---------------------------------------------------------------------------
// Genesis, v2 and v3: the three accepted generations
// ---------------------------------------------------------------------------

/// §7.4: two records, both singletons, `revision = 1`, tag 163 only in RuleConfig.
#[test]
fn genesis_state_payload_and_digest_match_the_accepted_vector() {
    let records = set(vec![
        entry(12, b"", 1, fixture!("body.genesis.kb-metadata.bin")),
        entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
    ]);
    let mut ledger = Recorder::open();
    let mut payload = Vec::new();
    encode_state_payload(&records, &mut payload, &mut ledger).expect("encode");

    assert_eq!(payload.as_slice(), fixture!("payload.state.genesis.bin"));
    assert_eq!(payload.len(), 337, "quoted payload length");
    assert_eq!(
        ledger.byte_charges,
        vec![337],
        "the payload extent is admitted exactly once, before it is written"
    );

    let digest = state_digest(&records, &mut Recorder::open()).expect("digest");
    assert_eq!(
        hex32(&digest),
        "b6422a4ac1ef53598c2259991facb9bf16079b479aee20c97ced428859aacc88",
        "accepted genesis state_digest (storage-digests 7.4)"
    );
    assert_eq!(
        digest,
        domain_digest("state", &payload),
        "domain framing agrees"
    );
}

/// §7.4's numeric discriminator: putting tag 163 in **both** bodies yields `aace0923…`,
/// which is what an implementation that reads the split table loosely produces.
#[test]
fn genesis_rule_digest_belongs_to_rule_config_only() {
    let correct = state_digest(
        &set(vec![
            entry(12, b"", 1, fixture!("body.genesis.kb-metadata.bin")),
            entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
        ]),
        &mut Recorder::open(),
    )
    .expect("digest");

    let mutant = fixture!("payload.state.genesis.163-both-bodies.mutant.bin");
    assert_eq!(
        hex32(&domain_digest("state", mutant)),
        "aace0923cf4f09cbf0709082e370d2bef39bcaf87b1d0e4c48a60ece01ea3ced",
        "the quoted counterexample for 163 in both bodies"
    );
    assert_ne!(
        hex32(&correct),
        hex32(&domain_digest("state", mutant)),
        "a 163-in-both-bodies implementation must not match the accepted digest"
    );
}

/// §4.4.6 is a normative table, so the split must be reproducible from the bytes: the
/// KbMetadata body must not contain tag 163, and the RuleConfig body must.
#[test]
fn genesis_split_is_visible_in_the_bodies_themselves() {
    fn tags(body: &[u8]) -> Vec<u16> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < body.len() {
            let tag = u16::from_le_bytes([body[offset], body[offset + 1]]);
            let len = u32::from_le_bytes([
                body[offset + 4],
                body[offset + 5],
                body[offset + 6],
                body[offset + 7],
            ]) as usize;
            out.push(tag);
            offset += 8 + len;
        }
        out
    }
    let kb = tags(fixture!("body.genesis.kb-metadata.bin"));
    let rules = tags(fixture!("body.genesis.rule-config.bin"));
    assert!(!kb.contains(&163), "163 must not enter KbMetadata: {kb:?}");
    assert!(rules.contains(&163), "163 belongs to RuleConfig: {rules:?}");
    for shared in [2u16, 8] {
        assert!(
            kb.contains(&shared) && rules.contains(&shared),
            "tag {shared} is shared"
        );
    }
    for kb_only in [144u16, 145, 146, 147, 148] {
        assert!(
            kb.contains(&kb_only) && !rules.contains(&kb_only),
            "tag {kb_only}"
        );
    }
    for rules_only in [166u16, 196] {
        assert!(
            rules.contains(&rules_only) && !kb.contains(&rules_only),
            "tag {rules_only}"
        );
    }
}

/// §7.5: a new Term plus the inherited singletons. Inheritance is the fold's job, but the
/// digest must be sensitive to it: a projection over only this version's events differs.
#[test]
fn v2_state_digest_includes_inherited_singletons() {
    let inherited_only = set(vec![
        entry(12, b"", 1, fixture!("body.genesis.kb-metadata.bin")),
        entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
    ]);
    let with_term = set(vec![
        entry(1, &1u32.to_le_bytes(), 1, fixture!("body.v2.term-1.bin")),
        entry(12, b"", 1, fixture!("body.genesis.kb-metadata.bin")),
        entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
    ]);
    let mut ledger = Recorder::open();
    let mut payload = Vec::new();
    encode_state_payload(&with_term, &mut payload, &mut ledger).expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.state.v2.bin"));
    assert_eq!(payload.len(), 592);

    let digest = state_digest(&with_term, &mut Recorder::open()).expect("digest");
    assert_eq!(
        hex32(&digest),
        "381bd739b2978e45f457dcb0452fe88d96d8c653a8b620ad018ec4d638c89ad4"
    );
    assert_ne!(
        digest,
        state_digest(&inherited_only, &mut Recorder::open()).expect("digest"),
        "a digest over only this version's events is not the current state"
    );
}

/// §7.5b: a revision with a complete after-image, and a natural-key update that keeps one
/// kind-12 row while kind 13 is inherited unchanged.
#[test]
fn v3_state_digest_covers_full_after_image_and_inheritance() {
    let records = set(vec![
        entry(1, &1u32.to_le_bytes(), 2, fixture!("body.v3.term-1.bin")),
        entry(12, b"", 2, fixture!("body.v3.kb-metadata.bin")),
        entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
    ]);
    let mut ledger = Recorder::open();
    let mut payload = Vec::new();
    encode_state_payload(&records, &mut payload, &mut ledger).expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.state.v3.bin"));
    assert_eq!(payload.len(), 892);

    let digest = state_digest(&records, &mut Recorder::open()).expect("digest");
    assert_eq!(
        hex32(&digest),
        "ca16fcb6337bd3d8e9f2e709a1a74c11f95b39f3bcd440ad7cc7b4d04cb5abf7"
    );

    // Tags 5 (recorded_by) and 6 (actor_origin) are record values on an ordinary revision,
    // not Genesis-only fields. A fold that dropped them lands on this other digest.
    assert_ne!(
        digest,
        domain_digest(
            "state",
            fixture!("payload.state.v3.drop-audit-tags.mutant.bin")
        ),
        "dropping tags 5/6 from a revised body must change the digest"
    );
}

// ---------------------------------------------------------------------------
// Ordering: bytewise key order (state) versus numeric id order (graph)
// ---------------------------------------------------------------------------

/// §4.4.2: keys sort **bytewise**. For a little-endian `u32`, `0x0100` sorts before
/// `0x00FF`, so numeric key order is a different payload and a different digest.
#[test]
fn state_orders_by_key_bytes_not_by_numeric_id() {
    // Bodies are real `Field` sequences (`revision` then `atom`), so the discrimination is
    // about key order and nothing else.
    let bytewise = set(vec![
        entry(1, &0x0100u32.to_le_bytes(), 1, &atom_body("high")),
        entry(1, &0x00FFu32.to_le_bytes(), 1, &atom_body("low")),
    ]);
    let mut payload = Vec::new();
    encode_state_payload(&bytewise, &mut payload, &mut Recorder::open()).expect("encode");
    assert_eq!(
        payload.as_slice(),
        fixture!("payload.state.key-bytewise.synthetic.bin"),
        "bytewise key order is the canonical state order"
    );
    assert_ne!(
        domain_digest("state", &payload),
        domain_digest("state", fixture!("payload.state.key-numeric.mutant.bin")),
        "numeric key order is a different digest"
    );

    // The numeric order is refused rather than silently sorted into the canonical one.
    let mut ledger = Recorder::open();
    assert_eq!(
        RecordSet::from_sorted(
            vec![
                entry(1, &0x00FFu32.to_le_bytes(), 1, b"body-of-0x00FF"),
                entry(1, &0x0100u32.to_le_bytes(), 1, b"body-of-0x0100"),
            ],
            &mut ledger,
        ),
        Err(RecordSetRejection::Order(RecordSetError::NotSorted)),
        "sorting happens in the fold, never in the digest path"
    );
}

/// A minimal valid body: `2 revision = 1` then `17 atom`, both as `Field` envelopes.
fn atom_body(atom: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&8u32.to_le_bytes());
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&17u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(atom.len() as u32).to_le_bytes());
    body.extend_from_slice(atom.as_bytes());
    body
}

fn term(id: u32, layer: SemanticLayer) -> TermMeta {
    TermMeta {
        id: TermId::new(id),
        layer,
    }
}

fn assertion(id: u128, rev: u64, from: u32, target: u32) -> AssertionRecord {
    AssertionRecord {
        id: AssertionId::new(id),
        revision: revision(rev),
        from: TermId::new(from),
        relation: RelationId::DependsOn,
        target: TermId::new(target),
        confidence: Confidence::Confirmed,
    }
}

/// §4.5: the graph projection sorts by **numeric** id, the opposite rule from `state`.
#[test]
fn graph_orders_by_numeric_id() {
    let low = 0x00FF;
    let high = 0x0100;
    let terms = [term(low, SemanticLayer::L2), term(high, SemanticLayer::L0)];
    let mut assertions = [
        assertion(0x00FF, 1, low, high),
        assertion(0x0100, 4, high, low),
    ];
    assertions[1].relation = RelationId::IsA;
    assertions[1].confidence = Confidence::Hypothetical;

    let mut payload = Vec::new();
    encode_graph_payload(&terms, &assertions, &mut payload, &mut Recorder::open()).expect("encode");
    assert_eq!(
        payload.as_slice(),
        fixture!("payload.graph.id-numeric.synthetic.bin"),
        "numeric id order is the canonical graph order"
    );
    assert_eq!(
        graph_digest(&terms, &assertions, &mut Recorder::open())
            .map(|d| hex32(&d))
            .expect("digest"),
        hex32(&domain_digest(
            "graph",
            fixture!("payload.graph.id-numeric.synthetic.bin")
        )),
        "digest agrees with its own payload under the graph domain"
    );
    assert_ne!(
        domain_digest("graph", &payload),
        domain_digest("graph", fixture!("payload.graph.id-bytewise.mutant.bin")),
        "bytewise id order is a different graph digest"
    );

    // The bytewise order is refused, not sorted.
    let mut ledger = Recorder::open();
    assert_eq!(
        encode_graph_payload(&[terms[1], terms[0]], &[], &mut Vec::new(), &mut ledger),
        Err(DigestAssemblyError::Structure(
            StructureDefect::TermsNotAscending { index: 1 }
        ))
    );
    assert_eq!(
        encode_graph_payload(
            &terms,
            &[assertions[1], assertions[0]],
            &mut Vec::new(),
            &mut ledger
        ),
        Err(DigestAssemblyError::Structure(
            StructureDefect::AssertionsNotAscending { index: 1 }
        ))
    );
}

// ---------------------------------------------------------------------------
// graph: the three accepted generations, and the projection's narrowness
// ---------------------------------------------------------------------------

#[test]
fn graph_digests_match_the_three_accepted_vectors() {
    let empty: [TermMeta; 0] = [];
    let none: [AssertionRecord; 0] = [];
    let mut ledger = Recorder::open();
    let mut payload = Vec::new();
    encode_graph_payload(&empty, &none, &mut payload, &mut ledger).expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.graph.genesis.bin"));
    assert_eq!(payload.len(), 8, "two u32 zero counts");
    assert_eq!(
        hex32(&graph_digest(&empty, &none, &mut Recorder::open()).expect("digest")),
        "35efdf863b38edc1ac307e20f95b7990e35c3f1c5505eff5c9f4b4ddafb42d4e"
    );

    // Term id 1 is authored at L1 in the v2 golden; the projection carries only that.
    let terms = [term(1, SemanticLayer::L1)];
    let mut payload = Vec::new();
    encode_graph_payload(&terms, &none, &mut payload, &mut Recorder::open()).expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.graph.v2.bin"));
    assert_eq!(
        payload.len(),
        13,
        "u32 count + (u32 id, u8 layer) + u32 count"
    );

    for (generation, quoted) in [
        (
            "v2",
            "7c9f89cc6e83bb47fdfabaef8ce13b8435eba7c02412ec8f8afae85372f0210a",
        ),
        (
            "v3",
            "7c9f89cc6e83bb47fdfabaef8ce13b8435eba7c02412ec8f8afae85372f0210a",
        ),
    ] {
        let digest = graph_digest(&terms, &none, &mut Recorder::open()).expect("digest");
        assert_eq!(hex32(&digest), quoted, "accepted {generation} graph_digest");
    }
}

/// `revision` is a semantic input (D2), so two revisions of one endpoint must differ.
#[test]
fn assertion_revision_changes_the_graph_digest() {
    let terms = [term(1, SemanticLayer::L1), term(2, SemanticLayer::L0)];
    let at = |value| [assertion(9, value, 1, 2)];
    let first = graph_digest(&terms, &at(1), &mut Recorder::open()).expect("digest");
    let second = graph_digest(&terms, &at(2), &mut Recorder::open()).expect("digest");
    assert_ne!(
        first, second,
        "a cache key without revision would return a proof for the superseded revision"
    );
}

// ---------------------------------------------------------------------------
// bookkeeping: cumulative outcomes, imports and u128 watermarks
// ---------------------------------------------------------------------------

fn outcome(
    principal: &str,
    request: &str,
    payload: [u8; 32],
    result: [u8; 32],
    version: u64,
) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: principal.to_string(),
        request_id: request.to_string(),
        payload_digest: payload,
        result_ref: result,
        commit_version: version,
        audit: audit(),
        extensions: Extensions::new(),
    }
}

/// The exact payload digests and result refs the D10 v2 fixture commits.
const V2_PAYLOAD: [u8; 32] = [
    0x7f, 0x37, 0xfe, 0xca, 0xcf, 0x9b, 0xea, 0x01, 0xfa, 0x46, 0x21, 0xe3, 0xbb, 0x24, 0xad, 0x5c,
    0x0d, 0x79, 0x8d, 0xc3, 0x67, 0x79, 0x16, 0xa2, 0xeb, 0x20, 0xeb, 0xf8, 0x21, 0xe9, 0x40, 0xbb,
];
const V2_RESULT: [u8; 32] = [
    0x29, 0x9e, 0x6e, 0x88, 0x1b, 0xcb, 0x39, 0xd4, 0x9e, 0x7c, 0xd8, 0x3a, 0xec, 0xb7, 0x18, 0x0f,
    0x72, 0x97, 0xe2, 0x23, 0x90, 0x11, 0x65, 0xe4, 0x78, 0xb5, 0xc9, 0x01, 0xc1, 0x14, 0x25, 0x1d,
];
const V3_PAYLOAD: [u8; 32] = [
    0x5a, 0xe5, 0x04, 0x15, 0x2d, 0xa8, 0x55, 0x30, 0x6d, 0xe0, 0xe2, 0x2e, 0xf3, 0x7e, 0x63, 0x31,
    0x5b, 0x39, 0xcb, 0xb0, 0x2d, 0xf2, 0x8e, 0xe2, 0x01, 0xc8, 0x5f, 0x61, 0x2d, 0xb4, 0x34, 0x12,
];
const V3_RESULT: [u8; 32] = [
    0x92, 0xdf, 0x35, 0x68, 0x24, 0x29, 0x10, 0x8a, 0x37, 0x11, 0x61, 0xb9, 0x5e, 0x4d, 0x45, 0x55,
    0x1c, 0x57, 0x4a, 0xe0, 0x01, 0x3c, 0x2f, 0x8a, 0x62, 0x62, 0x76, 0x3a, 0x10, 0xbc, 0xa8, 0xa4,
];

fn genesis_counters() -> Vec<Watermark> {
    IdSpace::ALL
        .iter()
        .map(|space| Watermark {
            space: *space,
            hwm: 0,
        })
        .collect()
}

#[test]
fn bookkeeping_digests_match_the_three_accepted_vectors() {
    let counters = genesis_counters();
    let mut payload = Vec::new();
    encode_bookkeeping_payload(&[], &[], &counters, &mut payload, &mut Recorder::open())
        .expect("encode");
    assert_eq!(
        payload.as_slice(),
        fixture!("payload.bookkeeping.genesis.bin")
    );
    assert_eq!(
        payload.len(),
        174,
        "8 bytes of zero counts + 9 x 18-byte counters"
    );
    assert_eq!(
        hex32(&bookkeeping_digest(&[], &[], &counters, &mut Recorder::open()).expect("digest")),
        "756daff9afe2bad1df7caf317f0c58d72a9013e172b0de5e58fc4e5a935ae902"
    );

    let req1 = outcome("tester", "req-1", V2_PAYLOAD, V2_RESULT, 2);
    let mut counters_v2 = counters.clone();
    counters_v2[0].hwm = 1;
    assert_eq!(counters_v2[0].space, IdSpace::TermId);
    let mut payload = Vec::new();
    encode_bookkeeping_payload(
        std::slice::from_ref(&req1),
        &[],
        &counters_v2,
        &mut payload,
        &mut Recorder::open(),
    )
    .expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.bookkeeping.v2.bin"));
    assert_eq!(payload.len(), 267);
    assert_eq!(
        hex32(
            &bookkeeping_digest(
                std::slice::from_ref(&req1),
                &[],
                &counters_v2,
                &mut Recorder::open()
            )
            .expect("digest")
        ),
        "45099a57f1c3f65138f6839dceb802f580f18fb2ada25f61827b861a76bbfc68"
    );

    // §7.5b: the digest is CUMULATIVE. v3 still carries req-1 and adds req-3.
    let req3 = outcome("tester", "req-3", V3_PAYLOAD, V3_RESULT, 3);
    let mut counters_v3 = counters_v2.clone();
    counters_v3[0].hwm = 3;
    let both = [req1, req3];
    let mut payload = Vec::new();
    encode_bookkeeping_payload(
        &both,
        &[],
        &counters_v3,
        &mut payload,
        &mut Recorder::open(),
    )
    .expect("encode");
    assert_eq!(payload.as_slice(), fixture!("payload.bookkeeping.v3.bin"));
    assert_eq!(payload.len(), 360);
    assert_eq!(
        hex32(
            &bookkeeping_digest(&both, &[], &counters_v3, &mut Recorder::open()).expect("digest")
        ),
        "377bf5c189b55d0989e9950e30632ea561a0ec0cfef974a767d465a52fbfdede"
    );

    // The discriminator: "this transaction's outcomes" instead of the accumulated set.
    assert_ne!(
        bookkeeping_digest(&both, &[], &counters_v3, &mut Recorder::open()).expect("digest"),
        domain_digest(
            "bookkeeping",
            fixture!("payload.bookkeeping.v3.single-outcome.mutant.bin")
        ),
        "v3's digest is cumulative, not per-transaction"
    );
}

/// §4.6's `Counter` row is `counter:u16 | high_water_mark:u128`. A `u64` width truncates
/// `2^64` to zero, which is the delivery-4 defect the section records.
#[test]
fn watermark_width_is_u128_not_u64() {
    let counters = vec![Watermark {
        space: IdSpace::AssertionId,
        hwm: 1u128 << 64,
    }];
    let mut payload = Vec::new();
    encode_bookkeeping_payload(&[], &[], &counters, &mut payload, &mut Recorder::open())
        .expect("encode");
    assert_eq!(
        payload.as_slice(),
        fixture!("payload.bookkeeping.hwm-2p64.synthetic.bin")
    );
    assert_eq!(payload.len(), 30, "8 header bytes + 2 + 16 + 4 count");
    assert_ne!(
        domain_digest("bookkeeping", &payload),
        domain_digest(
            "bookkeeping",
            fixture!("payload.bookkeeping.hwm-2p64-u64.mutant.bin")
        ),
        "a u64 watermark would collide with a zero high-water mark"
    );
}

/// The `Import` arm, which no D10 golden carries: `stage`, both digests, `expected_counts`
/// as `[CountPair]`, then a `bool`.
#[test]
fn bookkeeping_import_row_uses_state_kinds_and_a_bool() {
    let import = ImportSessionRecord {
        import_session_id: "import-2026-11-17".to_string(),
        stage: ImportStage::Import,
        source_digest: [0xbb; 32],
        plan_ref: [0xcc; 32],
        expected_counts: vec![
            CountPair {
                kind: StateKind::Term,
                count: 12,
            },
            CountPair {
                kind: StateKind::Assertion,
                count: 3,
            },
        ],
        import_complete: true,
        audit: audit(),
        extensions: Extensions::new(),
    };
    let req1 = outcome("tester", "req-1", V2_PAYLOAD, V2_RESULT, 2);
    let req3 = outcome("tester", "req-3", V3_PAYLOAD, V3_RESULT, 3);
    let mut counters = genesis_counters();
    counters[0].hwm = 3;
    let mut payload = Vec::new();
    encode_bookkeeping_payload(
        &[req1, req3],
        std::slice::from_ref(&import),
        &counters,
        &mut payload,
        &mut Recorder::open(),
    )
    .expect("encode");
    assert_eq!(
        payload.as_slice(),
        fixture!("payload.bookkeeping.v3.one-import.synthetic.bin")
    );
    assert_eq!(payload.len(), 472);

    // A CountPair kind is a STATE kind, so Asset/Model/KbMetadata/RuleConfig (10..=13) are
    // expressible here even though they have no `RecordKind`. Parse the row back instead of
    // hard-coding an offset: the import block sits immediately before the counter block.
    let counter_bytes = 4 + counters.len() * 18;
    let import_bytes = 4
        + import.import_session_id.len()
        + 2
        + 32
        + 32
        + 4
        + import.expected_counts.len() * 10
        + 1;
    let import_start = payload.len() - counter_bytes - import_bytes;
    let counts_at = import_start + 4 + import.import_session_id.len() + 2 + 32 + 32;
    assert_eq!(
        u32::from_le_bytes(payload[counts_at..counts_at + 4].try_into().unwrap()),
        2
    );
    let pairs = &payload[counts_at + 4..counts_at + 4 + 20];
    assert_eq!(u16::from_le_bytes(pairs[0..2].try_into().unwrap()), 1);
    assert_eq!(u64::from_le_bytes(pairs[2..10].try_into().unwrap()), 12);
    assert_eq!(u16::from_le_bytes(pairs[10..12].try_into().unwrap()), 2);
    assert_eq!(u64::from_le_bytes(pairs[12..20].try_into().unwrap()), 3);
    assert_eq!(
        payload[import_start + import_bytes - 1],
        1,
        "import_complete = true"
    );
}

// ---------------------------------------------------------------------------
// Refusals: distinct classes, nothing published on failure
// ---------------------------------------------------------------------------

#[test]
fn state_refuses_extent_before_writing_anything() {
    let records = set(vec![
        entry(12, b"", 1, fixture!("body.genesis.kb-metadata.bin")),
        entry(13, b"", 1, fixture!("body.genesis.rule-config.bin")),
    ]);
    let mut payload = Vec::new();
    let mut ledger = Recorder::with_bytes(336);
    assert!(matches!(
        encode_state_payload(&records, &mut payload, &mut ledger),
        Err(DigestAssemblyError::Budget(error))
            if error.requested() == 337 && error.remaining() == 336
    ));
    assert!(payload.is_empty(), "a refused encode publishes no prefix");
}

#[test]
fn state_observes_cancellation_on_an_empty_and_a_non_empty_set() {
    let empty = set(Vec::new());
    let mut payload = Vec::new();
    assert_eq!(
        encode_state_payload(
            &empty,
            &mut payload,
            &mut Recorder::stopping(ControlStop::Cancelled)
        ),
        Err(DigestAssemblyError::Stopped(ControlStop::Cancelled)),
        "a zero-entry operation must still observe the caller's policy"
    );

    let non_empty = set(vec![entry(
        12,
        b"",
        1,
        fixture!("body.genesis.kb-metadata.bin"),
    )]);
    let mut payload = Vec::new();
    assert_eq!(
        encode_state_payload(
            &non_empty,
            &mut payload,
            &mut Recorder::stopping(ControlStop::DeadlineExceeded)
        ),
        Err(DigestAssemblyError::Stopped(ControlStop::DeadlineExceeded)),
        "cancellation is not a bytes verdict"
    );
}

#[test]
fn state_refuses_unregistered_kinds_and_wrong_key_forms() {
    let cases: [(StateEntry, StructureDefect); 5] = [
        (
            entry(14, b"", 1, b""),
            StructureDefect::UnregisteredKind { index: 0, kind: 14 },
        ),
        (
            entry(0, b"", 1, b""),
            StructureDefect::UnregisteredKind { index: 0, kind: 0 },
        ),
        (
            entry(1, &[0u8; 16], 1, b""),
            StructureDefect::WrongKeyWidth {
                index: 0,
                kind: 1,
                found: 16,
            },
        ),
        (
            entry(1, b"", 1, b""),
            StructureDefect::EmptyNaturalKey { index: 0, kind: 1 },
        ),
        (
            entry(12, &[0u8; 4], 1, b""),
            StructureDefect::WrongKeyWidth {
                index: 0,
                kind: 12,
                found: 4,
            },
        ),
    ];
    for (candidate, defect) in cases {
        let records = set(vec![candidate]);
        let mut payload = Vec::new();
        assert_eq!(
            encode_state_payload(&records, &mut payload, &mut Recorder::open()),
            Err(DigestAssemblyError::Structure(defect))
        );
        assert!(payload.is_empty());
    }
}

#[test]
fn state_refuses_zero_and_immutable_revisions() {
    let zero = set(vec![entry(
        12,
        b"",
        0,
        fixture!("body.genesis.kb-metadata.bin"),
    )]);
    let mut payload = Vec::new();
    assert_eq!(
        encode_state_payload(&zero, &mut payload, &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::ZeroRevision { index: 0 }
        )),
        "Entry.revision has no Option, so zero is never a legal value"
    );

    // §4.4.4: Asset is content-addressed and immutable, so its revision is the constant 1.
    let asset = set(vec![entry(10, &[7u8; 32], 2, b"")]);
    let mut payload = Vec::new();
    assert_eq!(
        encode_state_payload(&asset, &mut payload, &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::ImmutableRevision {
                index: 0,
                revision: 2
            }
        ))
    );
    let asset_ok = set(vec![entry(10, &[7u8; 32], 1, b"")]);
    assert!(encode_state_payload(&asset_ok, &mut Vec::new(), &mut Recorder::open()).is_ok());
}

#[test]
fn state_refuses_bodies_that_smuggle_omitted_tags() {
    // §4.4.3 omits exactly tag 1 (identity) and tag 13 (reconstructible).
    let body: Vec<u8> = [
        &1u16.to_le_bytes()[..],
        &0u16.to_le_bytes()[..],
        &4u32.to_le_bytes()[..],
        &1u32.to_le_bytes()[..],
    ]
    .concat();
    let records = set(vec![entry(1, &1u32.to_le_bytes(), 1, &body)]);
    let mut payload = Vec::new();
    assert_eq!(
        encode_state_payload(&records, &mut payload, &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::BodyForbiddenTag { index: 0, tag: 1 }
        ))
    );

    let with_tag_13: Vec<u8> = [
        &13u16.to_le_bytes()[..],
        &0u16.to_le_bytes()[..],
        &2u32.to_le_bytes()[..],
        &7u16.to_le_bytes()[..],
    ]
    .concat();
    let records = set(vec![entry(1, &1u32.to_le_bytes(), 1, &with_tag_13)]);
    assert_eq!(
        encode_state_payload(&records, &mut Vec::new(), &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::BodyForbiddenTag { index: 0, tag: 13 }
        ))
    );

    // A body that is not a Field sequence at all is the malformed class, not the forbidden
    // one: the distinction is which rule was broken.
    let records = set(vec![entry(1, &1u32.to_le_bytes(), 1, &[0xFFu8; 3])]);
    assert!(matches!(
        encode_state_payload(&records, &mut Vec::new(), &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::BodyMalformed { index: 0, .. }
        ))
    ));
}

#[test]
fn graph_refuses_extent_cancellation_and_a_non_empty_buffer() {
    let terms = [term(1, SemanticLayer::L1)];
    let none: [AssertionRecord; 0] = [];

    let mut payload = Vec::new();
    assert!(matches!(
        encode_graph_payload(&terms, &none, &mut payload, &mut Recorder::with_bytes(12)),
        Err(DigestAssemblyError::Budget(error)) if error.requested() == 13
    ));
    assert!(payload.is_empty());

    let mut payload = Vec::new();
    assert_eq!(
        encode_graph_payload(
            &[],
            &[],
            &mut payload,
            &mut Recorder::stopping(ControlStop::Cancelled)
        ),
        Err(DigestAssemblyError::Stopped(ControlStop::Cancelled))
    );

    let mut payload = vec![0u8];
    assert_eq!(
        encode_graph_payload(&terms, &none, &mut payload, &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::BufferNotEmpty
        )),
        "a caller's buffer is never appended to after the fact"
    );
}

#[test]
fn bookkeeping_refuses_the_sentinel_empty_identity_and_disorder() {
    let counters = genesis_counters();

    let sentinel = outcome("tester", "req-1", [0u8; 32], V2_RESULT, 2);
    assert_eq!(
        encode_bookkeeping_payload(
            std::slice::from_ref(&sentinel),
            &[],
            &counters,
            &mut Vec::new(),
            &mut Recorder::open()
        ),
        Err(DigestAssemblyError::Structure(
            StructureDefect::SentinelPayloadDigest { index: 0 }
        )),
        "the all-zero payload digest is the genesis sentinel, reserved for non-requests"
    );

    let anonymous = outcome("", "req-1", V2_PAYLOAD, V2_RESULT, 2);
    assert_eq!(
        encode_bookkeeping_payload(
            std::slice::from_ref(&anonymous),
            &[],
            &counters,
            &mut Vec::new(),
            &mut Recorder::open()
        ),
        Err(DigestAssemblyError::Structure(
            StructureDefect::EmptyIdentity {
                index: 0,
                field: "principal_id"
            }
        ))
    );

    let req3 = outcome("tester", "req-3", V3_PAYLOAD, V3_RESULT, 3);
    let req1 = outcome("tester", "req-1", V2_PAYLOAD, V2_RESULT, 2);
    assert_eq!(
        encode_bookkeeping_payload(
            &[req3, req1],
            &[],
            &counters,
            &mut Vec::new(),
            &mut Recorder::open()
        ),
        Err(DigestAssemblyError::Structure(
            StructureDefect::OutcomesNotAscending { index: 1 }
        )),
        "the idempotency key order is the caller's, never re-sorted here"
    );

    let beyond = vec![Watermark {
        space: IdSpace::TermId,
        hwm: u128::from(u32::MAX) + 1,
    }];
    assert_eq!(
        encode_bookkeeping_payload(&[], &[], &beyond, &mut Vec::new(), &mut Recorder::open()),
        Err(DigestAssemblyError::Structure(
            StructureDefect::WatermarkBeyondSpace { index: 0 }
        )),
        "term ids are u32, so a watermark above that is not a state any fold can reach"
    );
}

#[test]
fn bookkeeping_observes_cancellation_on_empty_sets() {
    let counters = genesis_counters();
    assert_eq!(
        encode_bookkeeping_payload(
            &[],
            &[],
            &counters,
            &mut Vec::new(),
            &mut Recorder::stopping(ControlStop::Cancelled)
        ),
        Err(DigestAssemblyError::Stopped(ControlStop::Cancelled))
    );
    assert_eq!(
        encode_bookkeeping_payload(
            &[],
            &[],
            &[],
            &mut Vec::new(),
            &mut Recorder::stopping(ControlStop::DeadlineExceeded)
        ),
        Err(DigestAssemblyError::Stopped(ControlStop::DeadlineExceeded)),
        "all three sets empty is still an operation"
    );
}

/// Tag 21's persisted code has one declaration. The graph projection must emit the same
/// bytes the field codec does, for all three levels.
#[test]
fn confidence_codes_match_the_field_codec() {
    use sage_db_format::field::write_confidence;
    use sage_db_format::wire::Writer;

    for level in Confidence::ALL {
        let mut out = Vec::new();
        let mut ledger = Recorder::open();
        write_confidence(&mut Writer::new(&mut out, &mut ledger), level).expect("encode");
        assert_eq!(
            out,
            level.as_u16().to_le_bytes(),
            "graph digest must emit the persisted confidence code for {level:?}"
        );
    }
}

/// A paid rejection must never look like corruption, and a fold defect must never be
/// dressed up as a refusal: the three classes stay distinct with their own remedies.
#[test]
fn refusals_and_structure_defects_stay_distinct() {
    let budget = DigestAssemblyError::Budget(BudgetError::new(BudgetUnit::Bytes, 4, 1));
    let stop = DigestAssemblyError::Stopped(ControlStop::Cancelled);
    let defect = DigestAssemblyError::Structure(StructureDefect::TooLarge);
    assert_ne!(budget, stop);
    assert_ne!(budget, defect);
    assert_ne!(stop, defect);

    use sage_db_types::framing::{OperatorAction, Refusal};
    assert!(matches!(
        budget.as_refusal(),
        Some(Refusal::Budget(refused)) if refused.requested() == 4
    ));
    assert!(matches!(
        stop.as_refusal(),
        Some(Refusal::Stopped(ControlStop::Cancelled))
    ));
    assert_eq!(
        defect.as_refusal(),
        None,
        "a non-canonical projection has no refusal remedy, so none is fabricated"
    );
    // And a genuine refusal classifies as retry-or-abandon, never as restore-backup.
    let classified = sage_db_types::framing::classify(&budget.as_refusal().map_or(
        sage_db_types::framing::FormatOutcome::Ok(()),
        sage_db_types::framing::FormatOutcome::Refused,
    ));
    assert!(matches!(
        classified,
        OperatorAction::RetryWithBudget | OperatorAction::AbandonOrRetry
    ));
}
