//! File framing: `SGLB`/`SGLC` layout invariants against real golden bytes.
//!
//! Expected values are **independent** of any encoder: the golden files come verbatim from the
//! immutable D10 contract (provenance in `fixtures/file-codec/PROVENANCE.txt`), and the literal
//! `CURRENT` vector is checked against a CRC computed outside this crate.
//!
//! A bundle is required, not optional: a missing fixture **fails** rather than skipping, because a
//! skip would let this suite report green while checking nothing.

use std::path::PathBuf;

use sage_db_format::file::{
    BLOCK_FLAGS_RESERVED_MASK, BLOCK_HEADER_LEN, BlockHeader, CODEC_NONE, CURRENT_LAYOUT,
    CURRENT_MAGIC, CURRENT_MAX_NAME_BYTES, FILE_HEADER_LEN, Kind, Layout, TRAILER_LEN, block_codec,
    current_crc_coverage, current_payload_len, current_total_len, is_valid_manifest_name,
};

/// Load a bundled fixture, failing loudly if the bundle is incomplete.
fn fixture(name: &str) -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/file-codec");
    path.push(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("mandatory fixture {} is missing: {e}", path.display()))
}

fn crc32c(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

// ---------------------------------------------------------------------------
// Layout/magic
// ---------------------------------------------------------------------------

#[test]
fn layout_magics_and_versions_are_exact() {
    assert_eq!(Layout::Sglb.magic(), *b"SGLB");
    assert_eq!(Layout::Sglc.magic(), *b"SGLC");
    assert_eq!(
        Layout::Sglb.version(),
        0,
        "SGLB stores major, which is 0 for this draft"
    );
    assert_eq!(Layout::Sglc.version(), 1, "SGLC stores layout, fixed at 1");
    assert!(Layout::Sglb.matches_magic(b"SGLB"));
    assert!(
        !Layout::Sglb.matches_magic(b"SGLC"),
        "the two must not be interchangeable"
    );
}

#[test]
fn kind_codes_are_closed_and_checkpoint_is_reserved() {
    assert_eq!(Kind::Manifest.code(), 1);
    assert_eq!(Kind::Transaction.code(), 2);
    assert_eq!(Kind::ReservedCheckpoint.code(), 3);
    assert_eq!(Kind::from_code(1), Some(Kind::Manifest));
    assert_eq!(Kind::from_code(2), Some(Kind::Transaction));
    assert_eq!(Kind::from_code(3), Some(Kind::ReservedCheckpoint));
    // Unregistered codes must not acquire a default.
    for bad in [0u16, 4, 99, u16::MAX] {
        assert_eq!(
            Kind::from_code(bad),
            None,
            "code {bad} must not resolve to a kind"
        );
    }
    assert!(Kind::Manifest.is_writable());
    assert!(Kind::Transaction.is_writable());
    assert!(
        !Kind::ReservedCheckpoint.is_writable(),
        "kind 3 is reserved: writers must never emit it"
    );
}

#[test]
fn header_crc_input_zeroes_the_crc_field_rather_than_skipping_it() {
    let h = sage_db_format::file::FileHeader {
        layout: Layout::Sglb,
        kind: Kind::Transaction,
        payload_len: 1234,
        header_crc32c: 0xDEAD_BEEF,
    };
    let input = h.crc_input();
    assert_eq!(
        input.len(),
        FILE_HEADER_LEN,
        "the whole 32-byte header is hashed"
    );
    assert_eq!(&input[0..4], b"SGLB");
    assert_eq!(le16(&input, 4), 0, "major 0");
    assert_eq!(le16(&input, 6), 0, "minor must be 0");
    assert_eq!(le16(&input, 8), 2, "transaction kind");
    assert_eq!(le16(&input, 10), 0, "flags must be 0");
    assert_eq!(le32(&input, 12), 32, "header_len is exactly 32");
    assert_eq!(le64(&input, 16), 1234, "payload_len round-trips");
    assert_eq!(
        &input[24..28],
        &[0u8; 4],
        "the CRC field is ZEROED and still included in the input"
    );
    assert_eq!(&input[28..32], &[0u8; 4], "reserved must be 0");
}

// ---------------------------------------------------------------------------
// Block headers
// ---------------------------------------------------------------------------

#[test]
fn block_header_arithmetic_and_flag_decoding() {
    let b = BlockHeader {
        kind: 1,
        flags: 0b0001, // REQUIRED, codec 0
        encoded_len: 100,
        raw_len: 100,
        crc32c: 0,
    };
    assert_eq!(b.total_len(), Some(BLOCK_HEADER_LEN + 100));
    assert_eq!(b.codec(), CODEC_NONE);
    assert!(b.reserved_flags_clear());

    // Codec 1 (zstd) is reserved: decode it, and let the caller refuse it.
    let zstd = BlockHeader { flags: 0b0011, ..b };
    assert_eq!(zstd.codec(), 1, "codec bits are 1-2, shifted once");

    // A reserved flag bit must be visible as such.
    let bad = BlockHeader {
        flags: BLOCK_FLAGS_RESERVED_MASK,
        ..b
    };
    assert!(!bad.reserved_flags_clear());

    assert_eq!(block_codec(0b0110), 3, "both codec bits");
}

#[test]
fn block_total_len_cannot_overflow() {
    let huge = BlockHeader {
        kind: 1,
        flags: 1,
        encoded_len: u32::MAX,
        raw_len: 1,
        crc32c: 0,
    };
    // Fits in usize on 64-bit; the point is that it is computed with checked arithmetic.
    assert_eq!(huge.total_len(), Some(BLOCK_HEADER_LEN + u32::MAX as usize));
}

// ---------------------------------------------------------------------------
// CURRENT arithmetic: §3 rules 1-4
// ---------------------------------------------------------------------------

#[test]
fn current_arithmetic_matches_the_four_normative_invariants() {
    // Rule 1: fixed prefix 12 B, then 16+8+2 fixed, then n, 32, 4.
    assert_eq!(current_payload_len(44), 16 + 8 + 2 + 44 + 32 + 4);
    // Rule 2.
    assert_eq!(current_payload_len(44), 62 + 44);
    // Rule 3.
    assert_eq!(current_total_len(44), 74 + 44);
    // Rule 4.
    assert_eq!(current_crc_coverage(44), 70 + 44);
    // The value in the D10 vector table.
    assert_eq!(current_payload_len(44), 106);
    assert_eq!(current_total_len(44), 118);
    assert_eq!(current_crc_coverage(44), 114);
}

#[test]
fn current_arithmetic_holds_across_the_whole_name_length_range() {
    for n in 1..=CURRENT_MAX_NAME_BYTES {
        assert_eq!(current_payload_len(n), 62 + n, "rule 2 at n={n}");
        assert_eq!(current_total_len(n), 74 + n, "rule 3 at n={n}");
        assert_eq!(current_crc_coverage(n), 70 + n, "rule 4 at n={n}");
        // The CRC must fit exactly at the end.
        assert_eq!(
            current_crc_coverage(n) + 4,
            current_total_len(n),
            "CRC is the last 4 bytes"
        );
    }
}

// ---------------------------------------------------------------------------
// Real golden files: layout and CRC agree with what is on disk
// ---------------------------------------------------------------------------

#[test]
fn golden_current_files_satisfy_every_layout_invariant() {
    for name in [
        "golden.genesis.current",
        "golden.v2.current",
        "golden.v3.current",
    ] {
        let b = fixture(name);

        assert_eq!(&b[0..4], &CURRENT_MAGIC, "{name}: SGLC magic");
        assert_eq!(le16(&b, 4), CURRENT_LAYOUT, "{name}: layout 1");
        assert_eq!(le16(&b, 6), 0, "{name}: flags must be 0");

        let payload_len = le32(&b, 8) as usize;
        assert_eq!(payload_len, b.len() - 12, "{name}: payload_len == len-12");
        assert_eq!(
            payload_len,
            current_payload_len(44),
            "{name}: payload_len == 62+44"
        );
        assert_eq!(b.len(), current_total_len(44), "{name}: total == 74+44");

        let name_len = le16(&b, 36) as usize;
        assert_eq!(name_len, 44, "{name}: name_len");
        assert_eq!(payload_len, current_payload_len(name_len), "{name}: rule 2");

        let crc_at = current_crc_coverage(name_len);
        let stored = le32(&b, crc_at);
        let computed = crc32c(&b[..crc_at]);
        assert_eq!(
            stored, computed,
            "{name}: stored CRC must match the covered bytes"
        );
    }
}

#[test]
fn the_three_golden_currents_have_distinct_valid_crcs() {
    // Same shape, different content: proves the CRC depends on content, not just on length.
    let mut crcs = Vec::new();
    for name in [
        "golden.genesis.current",
        "golden.v2.current",
        "golden.v3.current",
    ] {
        let b = fixture(name);
        let n = le16(&b, 36) as usize;
        crcs.push(le32(&b, current_crc_coverage(n)));
    }
    crcs.sort_unstable();
    crcs.dedup();
    assert_eq!(
        crcs.len(),
        3,
        "three distinct files must yield three distinct CRCs"
    );
}

#[test]
fn golden_current_manifest_names_are_valid() {
    for name in [
        "golden.genesis.current",
        "golden.v2.current",
        "golden.v3.current",
    ] {
        let b = fixture(name);
        let n = le16(&b, 36) as usize;
        let nm = &b[38..38 + n];
        assert!(
            is_valid_manifest_name(nm),
            "{name}: {:?}",
            String::from_utf8_lossy(nm)
        );
    }
}

#[test]
fn manifest_name_grammar_is_strict() {
    let hex32 = "0".repeat(32);

    // VALID: zero-padded to at least 6 digits. `v000001` is canonical, not malformed — an earlier
    // version of my validator rejected it and the real golden CURRENT files caught me.
    for good in [
        format!("v000001-{hex32}.sgm"),
        format!("v000042-{hex32}.sgm"),
        format!("v999999-{hex32}.sgm"),
        format!("v12345678901234567890-{hex32}.sgm"), // 20 digits: the maximum
    ] {
        assert!(
            is_valid_manifest_name(good.as_bytes()),
            "must accept {good:?}"
        );
    }

    let bad = [
        format!("v1-{hex32}.sgm"),                     // fewer than 6 digits
        format!("v00001-{hex32}.sgm"),                 // 5 digits
        format!("v000000-{hex32}.sgm"),                // value 0 does not exist
        format!("v123456789012345678901-{hex32}.sgm"), // 21 digits: over the maximum
        format!("v-{hex32}.sgm"),                      // empty version
        format!("v000001-{}.sgm", "A".repeat(32)),     // uppercase hex
        format!("v000001-{}.sgm", "0".repeat(31)),     // short hex
        format!("v000001-{hex32}.sgl"),                // wrong suffix
        format!("{hex32}.sgm"),                        // no version
        String::new(),                                 // empty
        format!("v000001-{hex32}.sgm\0"),              // NUL
        format!("v000001-{}é.sgm", "0".repeat(30)),    // non-ASCII
    ];
    for b in &bad {
        assert!(!is_valid_manifest_name(b.as_bytes()), "must refuse {b:?}");
    }

    // Length boundary: 64 bytes total, so a 20-digit version is the longest legal form.
    let longest = format!("v12345678901234567890-{hex32}.sgm");
    assert_eq!(longest.len(), 58);
    assert!(is_valid_manifest_name(longest.as_bytes()));
}

// ---------------------------------------------------------------------------
// The D10 literal CURRENT vector — and its documented CRC erratum
// ---------------------------------------------------------------------------

/// The documented literal inputs must produce `0x914FBD87`, **not** the doc's `0x735D7266`.
///
/// Independent evidence (see `t62-evidence/CURRENT-CRC-ERRATUM.md`): with `payload_len = 106` the
/// CRC is `0x914FBD87`; `0x735D7266` only appears if `payload_len` is written as 102. Three real
/// golden files with the same shape verify against their own stored CRCs, so the layout is right and
/// the doc's literal value is the erratum.
#[test]
fn documented_current_input_yields_the_corrected_crc() {
    let kb_uuid: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let name = b"v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm";
    let manifest_sha = [0xABu8; 32];
    let n = name.len();
    assert_eq!(n, 44);

    let mut buf = Vec::with_capacity(current_total_len(n));
    buf.extend_from_slice(&CURRENT_MAGIC);
    buf.extend_from_slice(&CURRENT_LAYOUT.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(current_payload_len(n) as u32).to_le_bytes());
    buf.extend_from_slice(&kb_uuid);
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.extend_from_slice(&(n as u16).to_le_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(&manifest_sha);

    assert_eq!(
        buf.len(),
        current_crc_coverage(n),
        "coverage excludes only the CRC"
    );
    let computed = crc32c(&buf);
    assert_eq!(
        computed, 0x914F_BD87,
        "documented inputs must give the corrected CRC; 0x735D7266 only arises from \
         payload_len = 102, which violates §3 rule 2"
    );
    assert_ne!(
        computed, 0x735D_7266,
        "the doc's literal value must NOT be reproduced by a correct encoder"
    );

    // And the CRC covers exactly [0, 70+n): appending it yields the full 118-byte file.
    buf.extend_from_slice(&computed.to_le_bytes());
    assert_eq!(buf.len(), current_total_len(n));
}

#[test]
fn crc32c_known_answer_holds() {
    // The contract's own check value; if the primitive drifts, every framing byte test is suspect.
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

// ---------------------------------------------------------------------------
// SGLB golden files: header/block/trailer structure
// ---------------------------------------------------------------------------

#[test]
fn golden_sglb_files_have_valid_headers_blocks_and_trailers() {
    for name in [
        "golden.genesis.tx.sgl",
        "golden.v2.tx.sgl",
        "golden.genesis.manifest.sgm",
        "golden.v2.manifest.sgm",
    ] {
        let b = fixture(name);
        assert!(b.len() > FILE_HEADER_LEN + TRAILER_LEN, "{name}: too short");

        assert_eq!(&b[0..4], b"SGLB", "{name}: magic");
        assert_eq!(le16(&b, 4), 0, "{name}: major 0");
        assert_eq!(le16(&b, 6), 0, "{name}: minor 0");
        assert_eq!(le16(&b, 10), 0, "{name}: header flags 0");
        assert_eq!(
            le32(&b, 12),
            FILE_HEADER_LEN as u32,
            "{name}: header_len 32"
        );
        assert_eq!(&b[28..32], &[0u8; 4], "{name}: reserved 0");

        // The header CRC covers the whole 32-byte header with its own field zeroed.
        let mut zeroed = [0u8; FILE_HEADER_LEN];
        zeroed.copy_from_slice(&b[..FILE_HEADER_LEN]);
        zeroed[24..28].fill(0);
        assert_eq!(
            le32(&b, 24),
            crc32c(&zeroed),
            "{name}: header CRC must cover the zeroed 32-byte header"
        );

        // payload_len is everything after the header, blocks + trailer.
        let payload_len = le64(&b, 16) as usize;
        assert_eq!(
            payload_len,
            b.len() - FILE_HEADER_LEN,
            "{name}: payload_len == len-32"
        );

        // Walk blocks; they must tile the payload exactly, with the trailer last.
        let payload_end = FILE_HEADER_LEN + payload_len;
        let mut at = FILE_HEADER_LEN;
        let mut blocks = 0;
        while at < payload_end - TRAILER_LEN {
            assert!(
                at + BLOCK_HEADER_LEN <= payload_end - TRAILER_LEN,
                "{name}: block header fits"
            );
            let kind = le16(&b, at);
            let flags = le16(&b, at + 2);
            let encoded = le32(&b, at + 4) as usize;
            let raw = le32(&b, at + 8) as usize;
            let crc = le32(&b, at + 12);

            assert_eq!(kind, 1, "{name}: only block kind 1 is permitted");
            assert!(
                flags & BLOCK_FLAGS_RESERVED_MASK == 0,
                "{name}: reserved flag bits clear"
            );
            assert_eq!(
                block_codec(flags),
                CODEC_NONE,
                "{name}: codec must be none in this major"
            );
            assert!(encoded > 0, "{name}: encoded_len must not be 0");
            assert!(raw > 0, "{name}: raw_len must not be 0");

            let data_start = at + BLOCK_HEADER_LEN;
            let data_end = data_start + encoded;
            assert!(
                data_end <= payload_end - TRAILER_LEN,
                "{name}: block must not cross the trailer"
            );
            assert_eq!(
                crc,
                crc32c(&b[data_start..data_end]),
                "{name}: block CRC over encoded data"
            );

            at = data_end;
            blocks += 1;
        }
        assert!(blocks >= 1, "{name}: at least one body block");
        assert_eq!(
            at,
            payload_end - TRAILER_LEN,
            "{name}: blocks tile the payload exactly"
        );

        // Trailer: SHA-256 over header + all block bytes, excluding the trailer itself.
        let trailer_stored = &b[payload_end - TRAILER_LEN..payload_end];
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut hasher, &b[..payload_end - TRAILER_LEN]);
        let computed: [u8; 32] = sha2::Digest::finalize(hasher).into();
        assert_eq!(
            trailer_stored, &computed,
            "{name}: trailer covers header + blocks"
        );
    }
}

#[test]
fn sglb_kind_field_distinguishes_transaction_from_manifest() {
    let tx = fixture("golden.genesis.tx.sgl");
    let mf = fixture("golden.genesis.manifest.sgm");
    assert_eq!(le16(&tx, 8), Kind::Transaction.code());
    assert_eq!(le16(&mf, 8), Kind::Manifest.code());
}
