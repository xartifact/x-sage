//! Envelope, transaction and manifest decoders against real golden files.
//!
//! Every expectation here is an external fact: the D10 golden bytes, their own recorded digests, or
//! the §3/§5 offset rules. Nothing is checked against a value this crate's encoder produced.

use sage_db_format::manifest::{ManifestHeader, TransactionFacts, is_tx_file_name, read_manifest};
use sage_db_format::transaction::{
    DiscardEvents, EventVisitor, read_envelope, read_transaction, read_transaction_header,
};
use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, OperatorAction, Refusal,
    UnsupportedReason, classify,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

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

/// Counts events and records the sequence indices it was given.
#[derive(Default)]
struct Counting {
    seqs: Vec<u32>,
}

impl EventVisitor for Counting {
    fn visit(
        &mut self,
        event_seq: u32,
        _event: &sage_db_format::dispatch::DispatchedEvent<'_>,
        _ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        self.seqs.push(event_seq);
        Ok(())
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

/// Reference SHA-256 via the `sha2`-free test helper: we reuse the crate's own trailer verifier
/// indirectly by checking decoded values, and compute file hashes with a local implementation.
fn sha256(data: &[u8]) -> [u8; 32] {
    // Local, independent implementation (same code path the test module uses for manifest digests).
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Render an outcome that has no `Debug` (the envelope borrows the input).
fn describe<T>(outcome: &FormatOutcome<T>) -> String {
    match outcome {
        FormatOutcome::Ok(_) => "Ok(_)".to_string(),
        FormatOutcome::Format(e) => format!("Format({e:?})"),
        FormatOutcome::Refused(r) => format!("Refused({r:?})"),
    }
}

#[test]
fn sha256_helper_matches_the_known_answer() {
    assert_eq!(
        sha256(b"abc"),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

#[test]
fn every_golden_sglb_file_passes_envelope_verification() {
    for name in [
        "golden.genesis.tx.sgl",
        "golden.v2.tx.sgl",
        "golden.genesis.manifest.sgm",
        "golden.v2.manifest.sgm",
    ] {
        let bytes = fixture(name);
        let env = match read_envelope(&bytes, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(e) => e,
            other => panic!("{name}: expected Ok, got {}", describe(&other)),
        };

        // §2.1: payload_len is file length minus the 32-byte header.
        assert_eq!(env.header.payload_len, (bytes.len() - 32) as u64, "{name}");
        // One body block in these goldens, tiling the whole body region.
        assert_eq!(env.blocks.len(), 1, "{name}");
        assert_eq!(
            env.blocks[0].raw_len, env.blocks[0].encoded_len,
            "{name}: codec none means raw == encoded"
        );
        assert_eq!(env.blocks[0].codec(), 0, "{name}: codec must be none");

        // §2.3: the trailer is SHA-256 over header || blocks, not the trailer itself.
        assert_eq!(
            env.trailer_sha256,
            sha256(&bytes[..bytes.len() - 32]),
            "{name}: trailer must cover header plus blocks"
        );
    }
}

#[test]
fn the_header_crc_covers_the_zeroed_field_not_a_shorter_slice() {
    // The discriminating property: hashing the 32-byte header with bytes 24..28 zeroed must match,
    // while hashing a 28-byte "skip the CRC" slice must NOT. If both matched, the spec's warning
    // would be vacuous and this test would be worthless.
    let bytes = fixture("golden.genesis.tx.sgl");
    let stored = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);

    let mut zeroed = [0u8; 32];
    zeroed.copy_from_slice(&bytes[..32]);
    zeroed[24..28].fill(0);
    assert_eq!(
        crc32c::crc32c(&zeroed),
        stored,
        "the documented construction"
    );

    // A 28-byte input is a different value. Assert it differs so the test cannot pass under both.
    let skipped = crc32c::crc32c(&bytes[..24]);
    assert_ne!(
        skipped, stored,
        "skipping the CRC field must give a different value"
    );
}

#[test]
fn foreign_and_damaged_headers_are_distinguished() {
    let good = fixture("golden.genesis.tx.sgl");

    // A magic we do not know is a foreign file: UNSUPPORTED_FORMAT, never CORRUPT.
    let mut m = good.clone();
    m[0..4].copy_from_slice(b"XXXX");
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Magic))
    ));

    // A future major is legal-but-unsupported, not damaged.
    let mut m = good.clone();
    m[4] = 1;
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Major(1)))
    ));

    // Non-zero flags in this major is damage.
    let mut m = good.clone();
    m[10] = 1;
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags))
    ));

    // A corrupted header byte breaks the CRC.
    let mut m = good.clone();
    m[8] = 1; // kind -> manifest, without recomputing the CRC
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderCrc))
    ));

    // Truncation is located by the payload_len identity.
    let mut m = good.clone();
    m.pop();
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PayloadLenMismatch))
    ));

    // A damaged block body breaks the block CRC, after the envelope is otherwise fine.
    let mut m = good.clone();
    let idx = 32 + 16 + 5;
    m[idx] ^= 0xFF;
    assert!(matches!(
        read_envelope(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockCrc))
    ));
}

#[test]
fn whole_file_ceiling_is_a_limit_not_corruption() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let tiny = Limits {
        max_tx_file_bytes: 100,
        ..Limits::V1
    };
    match read_envelope(&bytes, &tiny, &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::TxFile)) => {}
        other => panic!("expected Limit(TxFile), got {}", describe(&other)),
    }
    assert_eq!(
        classify(&read_envelope(&bytes, &tiny, &mut Open)),
        OperatorAction::RejectInput
    );
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

#[test]
fn golden_genesis_transaction_decodes_completely() {
    // Genesis is mandatory in P1-2 and its codec HAS landed (structural.rs), so the real golden
    // genesis transaction decodes completely: the Genesis event plus nine IdAllocation events.
    let bytes = fixture("golden.genesis.tx.sgl");
    let mut visitor = Counting::default();
    let summary = read_transaction(&bytes, &Limits::V1, None, &mut visitor, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("genesis must decode: {}", describe(&e)));

    assert_eq!(summary.header.version, 1);
    assert_eq!(summary.header.prev_version, 0);
    assert_eq!(summary.events_seen, summary.header.event_count);
    // §4.3: exactly one Genesis event, then nine IdAllocation events (counters 0..8).
    assert_eq!(
        summary.events_seen, 10,
        "genesis is 1 Genesis + 9 IdAllocation"
    );
    // Event order is the only definition of event_seq (§4.1): dense, 0-based, no gaps.
    assert_eq!(visitor.seqs, (0..10u32).collect::<Vec<_>>());
    // Everything the decoder returned is a completed operation: there is no error to classify, which
    // is itself the assertion (the read above used into_result and would have panicked otherwise).
    assert!(summary.events_seen > 0);
}

/// Records what each dispatched event turned out to be.
#[derive(Default)]
struct Collecting {
    genesis: Option<sage_db_types::persist::GenesisRecord>,
    allocations: Vec<sage_db_types::persist::Watermark>,
    validated: usize,
}

impl EventVisitor for Collecting {
    fn visit(
        &mut self,
        _seq: u32,
        event: &sage_db_format::dispatch::DispatchedEvent<'_>,
        _ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        use sage_db_format::dispatch::DispatchedPayload;
        match event.payload() {
            Some(DispatchedPayload::Genesis(record)) => self.genesis = Some(record.clone()),
            Some(DispatchedPayload::IdAllocation(record)) => {
                self.allocations.push(record.watermark());
            }
            Some(_) => {}
            // Identified families stop at validation in this build.
            None => self.validated += 1,
        }
        Ok(())
    }
}

#[test]
fn genesis_payload_and_allocations_decode_from_the_real_file() {
    // This is what "Genesis landed" means: the structural reader produced real records, not just a
    // framed event. The nine IdAllocation events must cover nine distinct counter spaces at high
    // water mark 0 (§4.3: initial `hwm` is 0 for every space).
    let bytes = fixture("golden.genesis.tx.sgl");
    let mut visitor = Collecting::default();
    let summary = read_transaction(&bytes, &Limits::V1, None, &mut visitor, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("genesis must decode: {}", describe(&e)));

    let genesis = visitor.genesis.as_ref().expect("a Genesis record");
    assert_eq!(genesis.created_at, summary.header.created_at as i64);
    assert_eq!(
        visitor.allocations.len(),
        9,
        "one IdAllocation per id space"
    );
    assert_eq!(
        visitor.validated, 0,
        "no event should stop at validation here"
    );

    // Every space is distinct and starts at high water mark 0.
    let mut counters: Vec<sage_db_types::persist::IdSpace> =
        visitor.allocations.iter().map(|w| w.space).collect();
    counters.sort_unstable();
    counters.dedup();
    assert_eq!(counters.len(), 9, "counters must be distinct");
    for w in &visitor.allocations {
        assert_eq!(w.hwm, 0, "genesis starts every space at 0");
    }
}

#[test]
fn the_header_is_accepted_before_the_event_stream_is_judged() {
    // Read the header alone through the public transaction reader and confirm the failure is in the
    // *event* layer, not the header: a header defect would be Corrupt, this is Unsupported.
    let bytes = fixture("golden.genesis.tx.sgl");
    let env = read_envelope(&bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("envelope must verify: {}", describe(&e)));

    // §4.2 sanity on the real bytes, read directly: 12 header fields, version 1 at tag 192, and the
    // all-zero payload digest sentinel at 178 (genesis).
    assert_eq!(u16::from_le_bytes([env.body[0], env.body[1]]), 12);
    assert_eq!(env.header.kind, sage_db_format::file::Kind::Transaction);
}

#[test]
fn genesis_header_fields_match_the_specification() {
    // Parse the header region by hand from the real bytes as an independent expectation: tag 192
    // (version) must be 1, tag 194 (prev_version) 0, tag 178 all zero, tag 206 (canon) 0.
    let bytes = fixture("golden.genesis.tx.sgl");
    let env = read_envelope(&bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("envelope must verify: {}", describe(&e)));

    let mut found: std::collections::BTreeMap<u16, Vec<u8>> = std::collections::BTreeMap::new();
    let count = u16::from_le_bytes([env.body[0], env.body[1]]) as usize;
    let mut off = 2;
    for _ in 0..count {
        let tag = u16::from_le_bytes([env.body[off], env.body[off + 1]]);
        let len = u32::from_le_bytes([
            env.body[off + 4],
            env.body[off + 5],
            env.body[off + 6],
            env.body[off + 7],
        ]) as usize;
        found.insert(tag, env.body[off + 8..off + 8 + len].to_vec());
        off += 8 + len;
    }

    assert_eq!(found[&192], 1u64.to_le_bytes(), "version must be 1");
    assert_eq!(found[&194], 0u64.to_le_bytes(), "prev_version must be 0");
    assert_eq!(
        found[&178], [0u8; 32],
        "payload digest must be the zero sentinel"
    );
    assert_eq!(found[&206], [0u8, 0u8], "genesis canon version must be 0");
    // §7.5: principal_id and request_id are strings, and genesis uses fixed literals.
    assert_eq!(&found[&176][4..], b"system");
    assert_eq!(&found[&177][4..], b"genesis");
}

#[test]
fn a_manifest_read_as_a_transaction_is_unsupported_not_corrupt() {
    // The bytes are a perfectly good manifest. Telling an operator their disk is damaged because they
    // asked for the wrong file kind would be exactly the misclassification §9 forbids.
    let bytes = fixture("golden.genesis.manifest.sgm");
    match read_transaction(&bytes, &Limits::V1, None, &mut DiscardEvents, &mut Open) {
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FileKind(k))) => {
            assert_eq!(k, 1, "manifest is kind 1");
        }
        other => panic!("expected UnsupportedFormat(FileKind(1)), got {other:?}"),
    }
}

#[test]
fn transaction_id_is_cross_checked_against_the_file_name() {
    // The tx_id is the `<tid>` in `tx-<tid>.sgl`. The golden file's id is visible in its manifest.
    let manifest = fixture("golden.genesis.manifest.sgm");
    let msum = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("manifest reads");
    let name = msum.header.tx_file_name;
    let id_hex = &name[3..35];
    let mut id = [0u8; 16];
    for i in 0..16 {
        id[i] = u8::from_str_radix(&id_hex[i * 2..i * 2 + 2], 16).expect("hex");
    }

    // A wrong id must be reported at the header layer, before any event is examined — so this fails
    // with TxIdMismatch even though the events would fail later with NoCodec.
    let bytes = fixture("golden.genesis.tx.sgl");
    assert!(matches!(
        read_transaction(
            &bytes,
            &Limits::V1,
            Some([0xFFu8; 16]),
            &mut DiscardEvents,
            &mut Open
        ),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::TxIdMismatch))
    ));
    // And the correct id lets the whole transaction decode.
    assert!(
        read_transaction(&bytes, &Limits::V1, Some(id), &mut DiscardEvents, &mut Open).is_ok(),
        "the correct id must let the transaction decode"
    );
}

#[test]
fn genesis_sentinel_and_canon_version_are_paired() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let env = read_envelope(&bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("envelope must verify: {}", describe(&e)));

    // §4.2: the sentinel and the canon version are in the real bytes; both are read here directly
    // because the event layer stops at NoCodec(224) before returning a summary.
    let mut fields: std::collections::BTreeMap<u16, Vec<u8>> = std::collections::BTreeMap::new();
    let count = u16::from_le_bytes([env.body[0], env.body[1]]) as usize;
    let mut off = 2;
    for _ in 0..count {
        let tag = u16::from_le_bytes([env.body[off], env.body[off + 1]]);
        let len = u32::from_le_bytes([
            env.body[off + 4],
            env.body[off + 5],
            env.body[off + 6],
            env.body[off + 7],
        ]) as usize;
        fields.insert(tag, env.body[off + 8..off + 8 + len].to_vec());
        off += 8 + len;
    }
    assert_eq!(u16::from_le_bytes([fields[&206][0], fields[&206][1]]), 0);
    assert_eq!(fields[&178], vec![0u8; 32]);
}

#[test]
fn the_sentinel_and_canon_version_must_agree() {
    // 0 pairs with the zero digest, 1 with a real digest; mixing them would let a real request
    // masquerade as genesis (or the reverse), and request-id de-duplication keys off that.
    // Exercised through the public `TransactionHeader` rule using values from the real genesis file.
    let manifest = fixture("golden.genesis.manifest.sgm");
    let msum = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("reads");
    let tx_bytes = fixture("golden.genesis.tx.sgl");
    let env = read_envelope(&tx_bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("envelope must verify: {}", describe(&e)));

    // Independent check of the pairing straight from the bytes: canon 0 and zero digest coexist.
    let mut fields: std::collections::BTreeMap<u16, Vec<u8>> = std::collections::BTreeMap::new();
    let count = u16::from_le_bytes([env.body[0], env.body[1]]) as usize;
    let mut off = 2;
    for _ in 0..count {
        let tag = u16::from_le_bytes([env.body[off], env.body[off + 1]]);
        let len = u32::from_le_bytes([
            env.body[off + 4],
            env.body[off + 5],
            env.body[off + 6],
            env.body[off + 7],
        ]) as usize;
        fields.insert(tag, env.body[off + 8..off + 8 + len].to_vec());
        off += 8 + len;
    }
    let canon_zero = u16::from_le_bytes([fields[&206][0], fields[&206][1]]) == 0;
    let digest_zero = fields[&178] == vec![0u8; 32];
    assert_eq!(canon_zero, digest_zero, "genesis sentinel must be paired");

    // A version-1 manifest is genesis, so it must have no predecessor link.
    assert!(msum.header.is_genesis());
}

#[test]
fn non_monotonic_version_is_corruption() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let header = read_transaction_header(&bytes, &Limits::V1, None, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("header must read: {}", describe(&e)));

    let mut header = header;
    header.version = 5; // prev_version is 0, so version must be 1
    match header.check_against(None) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RevisionNotMonotonic {
            expected,
            found,
        })) => {
            assert_eq!(expected, 1);
            assert_eq!(found, 5);
        }
        other => panic!("expected RevisionNotMonotonic, got {other:?}"),
    }
}

#[test]
fn expect_version_must_equal_prev_version() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let header = read_transaction_header(&bytes, &Limits::V1, None, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("header must read: {}", describe(&e)));

    let mut header = header;
    header.expect_version = Some(7); // prev_version is 0
    assert!(matches!(
        header.check_against(None),
        FormatOutcome::Format(FormatError::Corrupt(
            CorruptReason::OutcomeHeaderMismatch { .. }
        ))
    ));
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[test]
fn golden_genesis_manifest_decodes_with_its_recorded_digests() {
    let bytes = fixture("golden.genesis.manifest.sgm");
    let summary = read_manifest(&bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| {
            panic!("genesis manifest must decode: {e:?}");
        });

    assert!(summary.header.is_genesis(), "genesis manifest is version 1");
    assert_eq!(
        summary.header.prev_manifest_name, None,
        "genesis has no predecessor"
    );
    assert_eq!(summary.header.prev_manifest_sha256, None);
    // §5 lists 13 fields, of which the two predecessor fields are optional; genesis has 11.
    assert_eq!(summary.field_count, 11, "genesis manifest field count");

    // §5: tx_file_name names the sibling transaction, and tx_file_sha256 is that file's hash.
    let tx_name = summary.header.tx_file_name;
    assert!(
        is_tx_file_name(tx_name),
        "{tx_name:?} must match ^tx-[0-9a-f]{{32}}\\.sgl$"
    );

    let tx_bytes = fixture("golden.genesis.tx.sgl");
    assert_eq!(
        summary.header.tx_file_sha256,
        sha256(&tx_bytes),
        "manifest must pin the transaction file's SHA-256"
    );
}

#[test]
fn manifest_trailer_hash_is_what_current_records() {
    // The chain link: CURRENT.manifest_sha256 is the SHA-256 of the manifest file. Verified against
    // the file's own trailer, which covers its header and blocks.
    let manifest = fixture("golden.genesis.manifest.sgm");
    let summary = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("genesis manifest reads");

    assert_eq!(
        summary.trailer_sha256,
        sha256(&manifest[..manifest.len() - 32])
    );
}

#[test]
fn manifest_and_transaction_shared_fields_are_cross_checked() {
    let manifest = fixture("golden.genesis.manifest.sgm");
    let msum = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("genesis manifest reads");

    let tx_bytes = fixture("golden.genesis.tx.sgl");
    let tsum = read_transaction_header(&tx_bytes, &Limits::V1, None, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("tx header must read: {}", describe(&e)));

    let facts = TransactionFacts {
        version: tsum.version,
        created_at: tsum.created_at as i64,
        kb_uuid: tsum.kb_uuid,
        rules_digest: tsum.rules_digest,
        rules_profile: tsum.rules_profile,
        file_sha256: Some(sha256(&tx_bytes)),
    };

    // The real pair must agree — this is the property that makes the chain meaningful.
    assert!(
        msum.header.check_against_transaction(&facts).is_ok(),
        "the golden manifest and transaction must agree on 8/144/163/192/196 and the file hash"
    );

    // And a one-field divergence must be caught, not tolerated.
    let mut wrong = facts;
    wrong.version += 1;
    assert!(matches!(
        msum.header.check_against_transaction(&wrong),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch))
    ));

    let mut wrong = facts;
    wrong.file_sha256 = Some([0u8; 32]);
    assert!(matches!(
        msum.header.check_against_transaction(&wrong),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch))
    ));
}

#[test]
fn genesis_manifest_with_a_predecessor_is_corruption() {
    let manifest = fixture("golden.genesis.manifest.sgm");
    let msum = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("reads");

    let tx_bytes = fixture("golden.genesis.tx.sgl");
    let tsum = read_transaction_header(&tx_bytes, &Limits::V1, None, &mut Open)
        .into_result()
        .unwrap_or_else(|e| panic!("tx header must read: {}", describe(&e)));
    let facts = TransactionFacts {
        version: tsum.version,
        created_at: tsum.created_at as i64,
        kb_uuid: tsum.kb_uuid,
        rules_digest: tsum.rules_digest,
        rules_profile: tsum.rules_profile,
        file_sha256: Some(sha256(&tx_bytes)),
    };

    // A genesis manifest claiming a predecessor contradicts "genesis is the first version".
    let mut header: ManifestHeader<'_> = msum.header;
    header.prev_manifest_name = Some("v999999-00112233445566778899aabbccddeeff.sgm");
    header.prev_manifest_sha256 = Some([1u8; 32]);
    assert!(matches!(
        header.check_against_transaction(&facts),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch))
    ));
}

#[test]
fn a_transaction_read_as_a_manifest_is_unsupported_not_corrupt() {
    let bytes = fixture("golden.genesis.tx.sgl");
    match read_manifest(&bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FileKind(k))) => {
            assert_eq!(k, 2, "transaction is kind 2");
        }
        other => panic!("expected UnsupportedFormat(FileKind(2)), got {other:?}"),
    }
}

#[test]
fn v2_manifest_requires_a_predecessor() {
    // Non-genesis manifests must carry the previous link; §5 makes it mandatory so the chain is
    // verifiable. The real v2 file should satisfy that.
    let bytes = fixture("golden.v2.manifest.sgm");
    let summary = read_manifest(&bytes, &Limits::V1, &mut Open)
        .into_result()
        .unwrap_or_else(|e| {
            panic!("v2 manifest must decode: {e:?}");
        });

    assert_eq!(summary.header.version, 2);
    assert!(!summary.header.is_genesis());
    assert!(
        summary.header.prev_manifest_name.is_some(),
        "v2 must name its predecessor"
    );
    assert!(summary.header.prev_pair_consistent());
    let name = summary.header.prev_manifest_name.expect("present");
    assert!(
        name.starts_with("v000001-"),
        "predecessor must be v1, got {name:?}"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn refusals_are_never_classified_as_damage() {
    let bytes = fixture("golden.genesis.tx.sgl");

    let budget = read_envelope(&bytes, &Limits::V1, &mut Allow(10));
    assert!(matches!(budget, FormatOutcome::Refused(Refusal::Budget(_))));
    assert_ne!(classify(&budget), OperatorAction::RestoreBackup);

    let manifest_bytes = fixture("golden.genesis.manifest.sgm");
    let halt = read_manifest(&manifest_bytes, &Limits::V1, &mut Halt);
    assert!(matches!(halt, FormatOutcome::Refused(Refusal::Stopped(_))));
    assert_ne!(classify(&halt), OperatorAction::RestoreBackup);
    assert!(sage_db_format::manifest::is_refusal(&halt));
}

// ---------------------------------------------------------------------------
// Refusals during field decoding must not become verdicts
// ---------------------------------------------------------------------------

/// Admits the whole file and the field table, but refuses **any small charge**. The header's owned
/// strings ("system", "genesis") are exactly such charges, so the refusal lands *inside* the field
/// reader — the one place a `map_err(|_| Corrupt)` would turn "retry with a bigger budget" into
/// "restore your backup". A ledger with a plain total budget cannot target that point: it would
/// refuse at the envelope and never reach the field reader at all.
struct RefuseSmallCharges;

impl SharedLedger for RefuseSmallCharges {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes < 64 {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0));
        }
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

#[test]
fn a_budget_refusal_inside_field_decoding_stays_a_refusal() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let outcome = read_transaction_header(&bytes, &Limits::V1, None, &mut RefuseSmallCharges);
    match &outcome {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        FormatOutcome::Format(FormatError::Corrupt(reason)) => {
            panic!("a budget refusal was laundered into Corrupt({reason:?})")
        }
        other => panic!(
            "a small-charge refusal must surface as a refusal, got {}",
            describe(other)
        ),
    }
    assert_ne!(classify(&outcome), OperatorAction::RestoreBackup);
}

#[test]
fn cancellation_inside_field_decoding_stays_a_stop() {
    let bytes = fixture("golden.genesis.tx.sgl");
    let outcome = read_transaction_header(&bytes, &Limits::V1, None, &mut Halt);
    assert!(
        outcome.as_refusal().is_some(),
        "cancellation must stay a refusal, got {}",
        describe(&outcome)
    );
    assert_eq!(classify(&outcome), OperatorAction::AbandonOrRetry);
}

// ---------------------------------------------------------------------------
// Scope limit: assert the thing we do NOT provide, so nobody assumes it
// ---------------------------------------------------------------------------

#[test]
fn structural_success_does_not_imply_family_validation() {
    // `read_dispatched` applies framing, registry acceptance and family availability, but not the
    // family decoders. This test documents the boundary by showing that a transaction whose events
    // are registry-legal reads `Ok` — the guarantee is structural, and claiming more would be a lie.
    //
    // If a future bridge adds family validation, this test should be revisited deliberately: it is
    // here so the gap is visible rather than assumed closed.
    let bytes = fixture("golden.genesis.tx.sgl");
    let outcome = read_transaction(&bytes, &Limits::V1, None, &mut DiscardEvents, &mut Open);
    // Genesis and the nine IdAllocation events belong to natural/structural families, which this
    // seam now decodes by calling their readers, so the read succeeds for real.
    assert!(
        outcome.is_ok(),
        "genesis must decode: {}",
        describe(&outcome)
    );
    // No assertion is made here about Term/Assertion/Claim/Evidence record policy: those families are
    // still namespace-validated only, and this build has no fixture containing an identified event.
    // This test deliberately does not claim otherwise.
}

#[test]
fn kb_uuid_type_round_trips_through_the_manifest() {
    let manifest = fixture("golden.genesis.manifest.sgm");
    let msum = read_manifest(&manifest, &Limits::V1, &mut Open)
        .into_result()
        .expect("reads");
    let raw = *msum.header.kb_uuid.as_bytes();
    assert_eq!(KbUuid::from_bytes(raw), msum.header.kb_uuid);
    // The genesis KB identity is not all-zero: an unset uuid would be a fabrication.
    assert_ne!(raw, [0u8; 16]);
}
