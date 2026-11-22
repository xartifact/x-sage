//! `CURRENT` codec: round-trip, ceilings, and every refusal class.
//!
//! Expectations come from three places, none of them an encoder in this crate: the immutable
//! `golden.*.current` bytes (D10, provenance in `fixtures/file-codec/PROVENANCE.txt`), the §3
//! offset arithmetic, and the hand-computed literal in the contract.

use sage_db_format::current::{CurrentToEncode, DecodedCurrent, decode_current, encode_current};
use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, OperatorAction, Refusal,
    UnsupportedReason, classify, decode_current_bound,
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

/// Allows exactly `n` bytes in total, then refuses.
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

/// Refuses at the checkpoint: models a caller cancelling mid-operation.
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/file-codec")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The three real root pointers, whose names and digests come from the D10 contract.
const GOLDEN_CURRENTS: [&str; 3] = [
    "golden.genesis.current",
    "golden.v2.current",
    "golden.v3.current",
];

// ---------------------------------------------------------------------------
// Decoding real files
// ---------------------------------------------------------------------------

#[test]
fn every_golden_current_decodes_and_re_encodes_byte_identically() {
    for name in GOLDEN_CURRENTS {
        let bytes = fixture(name);
        let decoded = match decode_current(&bytes, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(v) => v,
            other => panic!("{name}: expected Ok, got {other:?}"),
        };

        // The name in the bytes is the name that comes back out.
        assert_eq!(
            decoded.encoded_len(),
            bytes.len(),
            "{name}: encoded_len must be the actual file size"
        );
        assert!(decoded.manifest_name().ends_with(".sgm"));
        assert!(decoded.version >= 1);

        // Round-trip: re-encoding must reproduce the file exactly, CRC included. This is the
        // property that makes the layout unambiguous — if any offset or coverage were wrong, the
        // CRC would land elsewhere and the bytes would differ.
        let mut out = Vec::new();
        match encode_current(
            &CurrentToEncode::from_decoded(&decoded),
            &mut out,
            &Limits::V1,
            &mut Open,
        ) {
            FormatOutcome::Ok(()) => {}
            other => panic!("{name}: encode refused {other:?}"),
        }
        assert_eq!(out, bytes, "{name}: re-encoding must be byte-identical");
    }
}

#[test]
fn golden_currents_carry_the_versions_their_names_claim() {
    // Version and name are cross-checked against each other, so a decoder that read the wrong
    // offset could not pass this by simply being self-consistent.
    for (name, expected_version) in [
        ("golden.genesis.current", 1u64),
        ("golden.v2.current", 2),
        ("golden.v3.current", 3),
    ] {
        let bytes = fixture(name);
        let decoded = match decode_current(&bytes, &Limits::V1, &mut Open) {
            FormatOutcome::Ok(v) => v,
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(decoded.version, expected_version, "{name}");
        // §2.4: the numeric segment is the decimal of the version, zero-padded to >= 6 digits.
        let expected_prefix = format!("v{expected_version:06}-");
        assert!(
            decoded.manifest_name().starts_with(&expected_prefix),
            "{name}: name {:?} must start with {expected_prefix:?}",
            decoded.manifest_name()
        );
    }
}

#[test]
fn golden_currents_name_the_manifest_bytes_they_digest() {
    // The SHA-256 recorded in CURRENT is the SHA-256 of the manifest file at that version. That is
    // an independent property: the digest is computed by the filesystem-facing tooling that produced
    // D10, not by this codec.
    for (current_name, manifest_name) in [
        ("golden.genesis.current", "golden.genesis.manifest.sgm"),
        ("golden.v2.current", "golden.v2.manifest.sgm"),
    ] {
        let decoded = match decode_current(&fixture(current_name), &Limits::V1, &mut Open) {
            FormatOutcome::Ok(v) => v,
            other => panic!("{current_name}: {other:?}"),
        };

        // Recompute SHA-256 of the real manifest bytes with a plain implementation.
        let manifest = fixture(manifest_name);
        let mut sha = Sha256::new();
        sha.update(&manifest);
        assert_eq!(
            decoded.manifest_sha256,
            sha.finish(),
            "{current_name}: recorded digest must be the manifest's SHA-256"
        );
    }
}

/// Minimal SHA-256, written here rather than pulled from a dependency, so the expectation is
/// produced by an independent implementation on the test side.
struct Sha256 {
    h: [u32; 8],
    buf: Vec<u8>,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: Vec::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn finish(mut self) -> [u8; 32] {
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
        let bit_len = (self.buf.len() as u64) * 8;
        self.buf.push(0x80);
        while self.buf.len() % 64 != 56 {
            self.buf.push(0);
        }
        self.buf.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in self.buf.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
                self.h[0], self.h[1], self.h[2], self.h[3], self.h[4], self.h[5], self.h[6],
                self.h[7],
            );
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            self.h[0] = self.h[0].wrapping_add(a);
            self.h[1] = self.h[1].wrapping_add(b);
            self.h[2] = self.h[2].wrapping_add(c);
            self.h[3] = self.h[3].wrapping_add(d);
            self.h[4] = self.h[4].wrapping_add(e);
            self.h[5] = self.h[5].wrapping_add(f);
            self.h[6] = self.h[6].wrapping_add(g);
            self.h[7] = self.h[7].wrapping_add(h);
        }

        let mut out = [0u8; 32];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

#[test]
fn sha256_test_double_is_correct_before_it_verifies_anything() {
    // A hand-rolled digest used as an oracle must itself be checked, or it proves nothing.
    let mut sha = Sha256::new();
    sha.update(b"abc");
    assert_eq!(
        sha.finish(),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn kb(n: u8) -> KbUuid {
    KbUuid::from_bytes([n; 16])
}

#[test]
fn encoded_current_satisfies_the_documented_arithmetic() {
    // §3: total = 74 + name_len, payload_len = 62 + name_len, CRC covers [0, 70+name_len).
    for name in [
        "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",
        "v000042-00112233445566778899aabbccddeeff.sgm",
    ] {
        let to_encode = match CurrentToEncode::new(kb(1), 42, name, [0xAB; 32]) {
            FormatOutcome::Ok(v) => v,
            other => panic!("{name}: {other:?}"),
        };
        let mut out = Vec::new();
        assert!(matches!(
            encode_current(&to_encode, &mut out, &Limits::V1, &mut Open),
            FormatOutcome::Ok(())
        ));
        assert_eq!(out.len(), 74 + name.len(), "{name}");
        assert_eq!(
            u32::from_le_bytes([out[8], out[9], out[10], out[11]]) as usize,
            62 + name.len(),
            "{name}: payload_len"
        );
        // The CRC must be the last four bytes and must verify.
        let crc_at = 70 + name.len();
        assert_eq!(crc_at + 4, out.len());
        assert_eq!(
            crc32c::crc32c(&out[..crc_at]),
            u32::from_le_bytes([
                out[crc_at],
                out[crc_at + 1],
                out[crc_at + 2],
                out[crc_at + 3]
            ]),
            "{name}: CRC must cover [0, 70+name_len)"
        );
    }
}

#[test]
fn version_zero_and_bad_names_cannot_be_encoded() {
    for (version, name) in [
        (0u64, "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm"),
        (1, "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm"), // fewer than 6 digits
        (1, "v000000-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm"), // version 0 in the name
        (1, "not-a-manifest.sgm"),
        (1, ""),
        (1, "v000001-0F1E2D3C4B5A69788796A5B4C3D2E1F0.sgm"), // uppercase
    ] {
        assert!(
            matches!(
                CurrentToEncode::new(kb(1), version, name, [0; 32]),
                FormatOutcome::Format(FormatError::Corrupt(_))
            ),
            "must refuse version={version} name={name:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Classification of damaged input
// ---------------------------------------------------------------------------

fn corpus() -> Vec<u8> {
    let mut out = Vec::new();
    let to_encode = CurrentToEncode::new(
        kb(0x5A),
        2,
        "v000002-00112233445566778899aabbccddeeff.sgm",
        [0x11; 32],
    )
    .into_result()
    .expect("valid corpus");
    encode_current(&to_encode, &mut out, &Limits::V1, &mut Open)
        .into_result()
        .expect("encode");
    out
}

#[test]
fn each_damage_site_maps_to_its_own_corrupt_cause() {
    // Every mutation targets one documented field, so the reason reported must be that field's.
    // A decoder that reported one generic Corrupt for everything could not pass this.
    let good = corpus();

    // Magic: `SGLB` here is a different file kind, not an older CURRENT.
    let mut m = good.clone();
    m[0..4].copy_from_slice(b"SGLB");
    match decode_current(&m, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Magic)) => {}
        other => panic!("SGLB magic must be UnsupportedFormat(Magic), got {other:?}"),
    }

    // Layout word.
    let mut m = good.clone();
    m[4] = 2;
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags))
    ));

    // Flags must be zero.
    let mut m = good.clone();
    m[6] = 1;
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags))
    ));

    // payload_len disagrees with name_len.
    let mut m = good.clone();
    m[8] = m[8].wrapping_add(1);
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PayloadLenMismatch))
    ));

    // Truncation.
    let mut m = good.clone();
    m.pop();
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PayloadLenMismatch))
    ));

    // Name grammar: corrupt a byte inside the name.
    let mut m = good.clone();
    let name_len = u16::from_le_bytes([good[36], good[37]]) as usize;
    assert_eq!(
        name_len, 44,
        "corpus name: v000002- (8) + 32 hex + .sgm (4)"
    );
    m[38] = b'S'; // 'v' -> 'S' breaks the leading 'v'
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(0)))
    ));

    // CRC: flip the final byte.
    let mut m = good.clone();
    let last = m.len() - 1;
    m[last] ^= 0xFF;
    assert!(matches!(
        decode_current(&m, &Limits::V1, &mut Open),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderCrc))
    ));

    // Sanity: the unmodified corpus decodes, so the mutations above are the only difference.
    assert!(matches!(
        decode_current(&good, &Limits::V1, &mut Open),
        FormatOutcome::Ok(_)
    ));
}

#[test]
fn oversized_input_is_a_limit_not_a_corruption() {
    // A file that grew beyond the ceiling is still structurally a CURRENT; the honest answer is a
    // ceiling refusal, not "your disk is damaged".
    let mut big = corpus();
    big.resize(4097, 0);
    match decode_current(&big, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Limit(LimitKind::CurrentFile)) => {}
        other => panic!("expected Limit(CurrentFile), got {other:?}"),
    }
    assert_eq!(
        classify(&decode_current(&big, &Limits::V1, &mut Open)),
        OperatorAction::RejectInput
    );
}

#[test]
fn a_raised_name_ceiling_cannot_manufacture_a_larger_current_exemption() {
    // The reason the ceiling comes from `decode_current_bound`: with `max_name_bytes` raised to the
    // type maximum, the bound must still be min(arithmetic, configured file ceiling), and the
    // protocol cap on names must still pin the arithmetic term.
    let raised = Limits {
        max_name_bytes: u16::MAX,
        ..Limits::V1
    };
    assert_eq!(raised.effective_name_bytes(), 64);
    assert_eq!(decode_current_bound(&raised), 74 + 64);

    // Lowering the file ceiling wins.
    let lowered = Limits {
        max_current_file_bytes: 100,
        ..Limits::V1
    };
    assert_eq!(decode_current_bound(&lowered), 100);
    assert_eq!(decode_current_bound(&Limits::V1), 138);
}

// ---------------------------------------------------------------------------
// Refusals are not verdicts
// ---------------------------------------------------------------------------

#[test]
fn budget_and_cancellation_never_become_a_bytes_verdict() {
    let bytes = corpus();

    // Budget too small for the file.
    match decode_current(&bytes, &Limits::V1, &mut Allow(10)) {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        other => panic!("expected Refused(Budget), got {other:?}"),
    }
    // Cancellation is reported as a stop, with its own operator action.
    match decode_current(&bytes, &Limits::V1, &mut Halt) {
        FormatOutcome::Refused(Refusal::Stopped(_)) => {}
        other => panic!("expected Refused(Stopped), got {other:?}"),
    }

    // The critical assertion: neither refusal may carry a FormatError, and neither may be classified
    // as "restore your backup".
    for outcome in [
        decode_current(&bytes, &Limits::V1, &mut Allow(10)),
        decode_current(&bytes, &Limits::V1, &mut Halt),
    ] {
        assert!(
            outcome.as_format_error().is_none(),
            "a refusal must not carry a bytes verdict: {outcome:?}"
        );
        assert_ne!(
            classify(&outcome),
            OperatorAction::RestoreBackup,
            "a refusal must never tell an operator the file is damaged: {outcome:?}"
        );
    }
}

#[test]
fn encoding_under_a_tight_budget_writes_nothing() {
    // A refused encode must not leave a partial file for the store to publish by accident.
    let to_encode = CurrentToEncode::new(
        kb(2),
        3,
        "v000003-00112233445566778899aabbccddeeff.sgm",
        [0x22; 32],
    )
    .into_result()
    .expect("valid");
    let mut out = Vec::new();
    match encode_current(&to_encode, &mut out, &Limits::V1, &mut Allow(4)) {
        FormatOutcome::Refused(Refusal::Budget(_)) => {}
        other => panic!("expected Refused(Budget), got {other:?}"),
    }
    assert!(out.is_empty(), "a refused encode must not emit bytes");
}

// ---------------------------------------------------------------------------
// Cross-check against the golden genesis file
// ---------------------------------------------------------------------------

#[test]
fn re_encoding_the_genesis_pointer_reproduces_the_golden_bytes_exactly() {
    // End-to-end: decode the real file, then encode from parts built out of the decoded values.
    // Byte equality here means the offsets, the identity fields and the CRC coverage all agree with
    // the bytes a different implementation produced.
    let bytes = fixture("golden.genesis.current");
    let decoded: DecodedCurrent = match decode_current(&bytes, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(v) => v,
        other => panic!("{other:?}"),
    };

    let rebuilt = CurrentToEncode::new(
        decoded.kb_uuid,
        decoded.version,
        decoded.manifest_name(),
        decoded.manifest_sha256,
    )
    .into_result()
    .expect("parts from a decoded value must be valid");

    let mut out = Vec::new();
    encode_current(&rebuilt, &mut out, &Limits::V1, &mut Open)
        .into_result()
        .expect("encode");
    assert_eq!(out, bytes);
}

// ---------------------------------------------------------------------------
// Write-boundary regressions (reproduced against current.rs 4293e096)
// ---------------------------------------------------------------------------

#[test]
fn a_non_empty_output_buffer_is_refused_and_left_untouched() {
    // The defect: `encode_current` appended with `extend_from_slice` and never checked the destination,
    // so a non-empty buffer produced CURRENT bytes glued onto the caller's prefix, and
    // `crc32c::crc32c(out)` hashed that prefix too. Every byte the function wrote could be correct while
    // the resulting file was not.
    //
    // A test that only ever passes an empty Vec cannot see this, which is why the prefix is in the test.
    let to_encode = CurrentToEncode::new(
        kb(1),
        42,
        "v000042-00112233445566778899aabbccddeeff.sgm",
        [0xAB; 32],
    )
    .into_result()
    .expect("valid");

    let mut out = vec![0x77u8; 8];
    match encode_current(&to_encode, &mut out, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Internal(
            sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
        )) => {}
        other => panic!("a non-empty buffer must be refused, got {other:?}"),
    }
    // Refused means untouched: no silent clearing, no partial append.
    assert_eq!(
        out,
        vec![0x77u8; 8],
        "the caller's buffer must be unchanged"
    );

    // The class must send the caller after their own bug, not after a damaged disk.
    let outcome = encode_current(&to_encode, &mut out, &Limits::V1, &mut Open);
    assert_eq!(classify(&outcome), OperatorAction::ReportDefect);
    assert_ne!(classify(&outcome), OperatorAction::RestoreBackup);
}

#[test]
fn an_empty_buffer_still_produces_a_decodable_current() {
    // The positive control for the test above: without it, "refuse non-empty" would be satisfied by an
    // encoder that refuses everything.
    let to_encode = CurrentToEncode::new(
        kb(2),
        7,
        "v000007-00112233445566778899aabbccddeeff.sgm",
        [0xCD; 32],
    )
    .into_result()
    .expect("valid");
    let mut out = Vec::new();
    encode_current(&to_encode, &mut out, &Limits::V1, &mut Open)
        .into_result()
        .expect("encode");
    match decode_current(&out, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(decoded) => {
            assert_eq!(decoded.version, 7);
            assert_eq!(decoded.manifest_name(), to_encode.manifest_name());
        }
        other => panic!("our own output must decode, got {other:?}"),
    }
}

#[test]
fn version_zero_cannot_be_encoded() {
    // The defect: `pub version: u64` let a caller set 0 after a successful `new(.., 1, ..)`, and
    // `encode_current` emitted a root pointer this build's decoder refuses. §3 makes version the commit
    // sequence number, so 0 is PRE_GENESIS: a CURRENT claiming it describes a KB that does not exist.
    //
    // `version` is private, so DIRECT mutation is uncompilable — but that is not the whole story:
    // `DecodedCurrent::version` is public and `from_decoded` copies it, so a zero version is still
    // reachable at runtime. The test below (`a_zero_version_reached_through_from_decoded_is_refused`)
    // is the one that proves the encoder's own check is load-bearing. This test covers only the
    // constructor's rejection and the accessor, and must not be cited as proof of unforgeability.
    assert!(matches!(
        CurrentToEncode::new(
            kb(3),
            0,
            "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",
            [0; 32]
        ),
        FormatOutcome::Format(FormatError::Corrupt(_))
    ));

    let valid = CurrentToEncode::new(
        kb(3),
        1,
        "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm",
        [0; 32],
    )
    .into_result()
    .expect("valid");
    // Readable through the accessor, and not writable from outside the module.
    // Readable through the accessor, and not writable from outside the module.
    assert_eq!(valid.version(), 1);
}

#[test]
fn a_zero_version_reached_through_from_decoded_is_refused() {
    // CONTROL for the encoder's `version == 0` check. Privacy of `CurrentToEncode::version` blocks only
    // DIRECT mutation; `DecodedCurrent::version` is public and `from_decoded` copies it, so this is a
    // real externally-reachable path to an illegal write input.
    //
    // If the encoder's explicit recheck is deleted, this test fails — which is what makes it a control
    // rather than a restatement of the constructor.
    let real = fixture("golden.genesis.current");
    let decoded = match decode_current(&real, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(d) => d,
        other => panic!("golden CURRENT must decode: {other:?}"),
    };

    // Positive: the unmutated conversion reproduces the exact bytes.
    let clean = CurrentToEncode::from_decoded(&decoded);
    let mut out = Vec::new();
    encode_current(&clean, &mut out, &Limits::V1, &mut Open)
        .into_result()
        .expect("unmutated from_decoded must encode");
    assert_eq!(
        out, real,
        "from_decoded must round-trip a valid CURRENT byte-identically"
    );

    // Negative: mutate the PUBLIC decoded version to 0 (PRE_GENESIS), convert, encode.
    let mut mutated = decoded;
    mutated.version = 0;
    let illegal = CurrentToEncode::from_decoded(&mutated);
    let mut out2 = vec![0u8; 0];
    match encode_current(&illegal, &mut out2, &Limits::V1, &mut Open) {
        FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::RevisionNotMonotonic { found, .. },
        )) => {
            assert_eq!(found, 0, "the refusal must name the version it saw");
        }
        other => panic!("a zero version through from_decoded must be refused, got {other:?}"),
    }
    assert!(out2.is_empty(), "a refused encode must write nothing");
}

#[test]
fn a_zero_version_through_from_decoded_leaves_a_caller_buffer_untouched() {
    // The same illegal input against a non-empty buffer must be refused by the buffer guard first, and
    // the buffer must survive unmodified. Two independent refusals, neither of which may write.
    let real = fixture("golden.genesis.current");
    let mut decoded = match decode_current(&real, &Limits::V1, &mut Open) {
        FormatOutcome::Ok(d) => d,
        other => panic!("{other:?}"),
    };
    decoded.version = 0;
    let illegal = CurrentToEncode::from_decoded(&decoded);

    let mut out = vec![0x5Au8; 4];
    match encode_current(&illegal, &mut out, &Limits::V1, &mut Open) {
        FormatOutcome::Format(_) => {}
        other => panic!("must be refused, got {other:?}"),
    }
    assert_eq!(
        out,
        vec![0x5Au8; 4],
        "the caller's buffer must be unchanged"
    );
}
