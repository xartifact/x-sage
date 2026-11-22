//! `SGLB`/`SGLC` file framing: outer header layout, block layout, trailer coverage, `CURRENT`.
//!
//! Pure: no filesystem, clock or environment access. Every item here either describes a layout or
//! computes a property of bytes; whether a *file* is reachable is the store's problem.
//!
//! Normative source: `docs/design/storage-format.md` §1.3 (limits), §2.1–2.3 (header, blocks,
//! trailer), §3 (`CURRENT`). Where this module and that document disagree, the document wins.
//!
//! ## Declaration ownership
//!
//! File-specific declarations only: [`Layout`] (a magic+version pair), [`Kind`], [`FileHeader`],
//! [`BlockHeader`], and the `CURRENT` field constants. The **error taxonomy** (`FormatError` and
//! its reason enums) and the **`Limits`** ceilings are *not* declared here — they belong to the
//! shared spine owner (`t61`). This module therefore exposes arithmetic and layout as data and
//! predicates, not as verdicts, so it can be proven correct before those declarations land without
//! inventing a look-alike.
//!
//! Verdict-returning entry points are also deliberately absent for now: a function that returned
//! `Result<_, LocalFormatError>` would *be* the competing declaration the task forbids.

/// A file family's magic plus layout version.
///
/// `SGLB` covers transactions and manifests (they differ by [`Kind`]); `SGLC` is the `CURRENT`
/// root pointer, which is not an `SGLB` file at all (`storage-format.md` §3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// `SGLB`, layout major 0 — transaction and manifest bodies.
    Sglb,
    /// `SGLC`, layout 1 — the root pointer.
    Sglc,
}

impl Layout {
    /// The exact 4-byte magic.
    #[must_use]
    pub const fn magic(self) -> [u8; 4] {
        match self {
            Self::Sglb => *b"SGLB",
            Self::Sglc => *b"SGLC",
        }
    }

    /// The layout/version number stored at offset 4.
    ///
    /// `SGLB` writes `major` (0 for this draft); `SGLC` writes `layout` (fixed at 1). They are the
    /// same 16-bit field position but different meanings, which is why they share one accessor.
    #[must_use]
    pub const fn version(self) -> u16 {
        match self {
            Self::Sglb => 0,
            Self::Sglc => 1,
        }
    }

    /// Whether `magic` names this layout.
    #[must_use]
    pub fn matches_magic(self, magic: &[u8; 4]) -> bool {
        *magic == self.magic()
    }
}

/// The `kind` field of an `SGLB` file.
///
/// A closed set. Kind 3 (checkpoint) is **reserved**: writers must not emit it and readers must
/// refuse it as unsupported rather than treat it as a body (`storage-format.md` §2.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A committed manifest.
    Manifest,
    /// One transaction segment.
    Transaction,
    /// Reserved checkpoint; refused in this major.
    ReservedCheckpoint,
}

impl Kind {
    /// Numeric code as stored.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Manifest => 1,
            Self::Transaction => 2,
            Self::ReservedCheckpoint => 3,
        }
    }

    /// The kind a numeric code denotes, or `None` when the code is unregistered.
    ///
    /// Returning `None` rather than defaulting is the point: an unknown kind must become an
    /// unsupported-format verdict at the caller, never "assume manifest".
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::Manifest),
            2 => Some(Self::Transaction),
            3 => Some(Self::ReservedCheckpoint),
            _ => None,
        }
    }

    /// Whether a writer may emit this kind in the current major.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::Manifest | Self::Transaction)
    }
}

/// The fixed size of the common `SGLB` header (`storage-format.md` §2.1).
pub const FILE_HEADER_LEN: usize = 32;

/// Offset of `header_crc32c` within the `SGLB` header.
pub const HEADER_CRC_OFFSET: usize = 24;

/// Length of the header CRC field itself.
pub const HEADER_CRC_LEN: usize = 4;

/// Offset of `payload_len` within the `SGLB` header.
pub const HEADER_PAYLOAD_LEN_OFFSET: usize = 16;

/// Size of the trailer SHA-256 appended to every `SGLB` payload (`§2.3`).
pub const TRAILER_LEN: usize = 32;

/// The fixed part of a block header, before its data (`§2.2`).
///
/// `Block := kind:u16 | flags:u16 | encoded_len:u32 | raw_len:u32 | crc32c:u32 | data[encoded_len]`
pub const BLOCK_HEADER_LEN: usize = 16;

/// Bit 0 of `Block.flags`: an unknown block kind must be refused, not skipped.
pub const BLOCK_FLAG_REQUIRED: u16 = 0b0001;

/// The only codec permitted in this major: `0 = none`. Codec 1 (zstd) is reserved and must be
/// refused, which is what makes "declared zstd support but silently skipped it" testable.
pub const CODEC_NONE: u16 = 0;

/// Mask covering the codec bits (1–2) of `Block.flags`.
pub const BLOCK_CODEC_MASK: u16 = 0b0110;

/// Mask of `Block.flags` bits that are reserved and must be zero in this major.
pub const BLOCK_FLAGS_RESERVED_MASK: u16 = 0b1111_1000;

/// Decode the codec selector out of `Block.flags`.
#[must_use]
pub const fn block_codec(flags: u16) -> u16 {
    (flags & BLOCK_CODEC_MASK) >> 1
}

/// The header fields of one `SGLB` file.
///
/// Parsed or produced by the framing layer; the wire accessors live with the shared `Writer`/
/// `Reader` primitives so there is one encoding of fixed-width integers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileHeader {
    /// Which magic/layout family this file is.
    pub layout: Layout,
    /// `SGLB` kind. Meaningless for `SGLC`, whose layout has no kind field.
    pub kind: Kind,
    /// Bytes after the header, **including** blocks and the trailer.
    pub payload_len: u64,
    /// The stored header CRC-32C.
    pub header_crc32c: u32,
}

impl FileHeader {
    /// The value written to the `header_crc32c` field, given the other fields.
    ///
    /// Returns the 32-byte header with the CRC field zeroed, which is exactly what the computation
    /// must cover (`§2.1`): "the whole 32 B header, with the 4 CRC bytes zeroed first", **not**
    /// "skip those 4 bytes". The two are different inputs and produce different values.
    ///
    /// This returns the *input bytes* rather than a CRC value so the caller can hash them with the
    /// mature `crc32c` crate; nothing here re-implements the algorithm.
    #[must_use]
    pub fn crc_input(&self) -> [u8; FILE_HEADER_LEN] {
        let mut h = [0u8; FILE_HEADER_LEN];
        h[0..4].copy_from_slice(&self.layout.magic());
        h[4..6].copy_from_slice(&self.layout.version().to_le_bytes());
        h[6..8].copy_from_slice(&0u16.to_le_bytes()); // minor: always 0 in this major
        h[8..10].copy_from_slice(&self.kind.code().to_le_bytes());
        h[10..12].copy_from_slice(&0u16.to_le_bytes()); // flags: must be 0
        h[12..16].copy_from_slice(&(FILE_HEADER_LEN as u32).to_le_bytes());
        h[HEADER_PAYLOAD_LEN_OFFSET..HEADER_CRC_OFFSET]
            .copy_from_slice(&self.payload_len.to_le_bytes());
        // offsets 24..28 stay zero: that is the documented CRC-input construction
        h[28..32].copy_from_slice(&0u32.to_le_bytes()); // reserved: must be 0
        h
    }
}

/// One block header from an `SGLB` payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockHeader {
    /// Block kind; only 1 (body) is permitted in this major.
    pub kind: u16,
    /// Required bit and codec selector.
    pub flags: u16,
    /// Bytes of encoded data following the header.
    pub encoded_len: u32,
    /// Length after decompression; used to refuse oversized blocks *before* decoding.
    pub raw_len: u32,
    /// CRC-32C over the **encoded** data.
    pub crc32c: u32,
}

impl BlockHeader {
    /// Total bytes this block occupies, header included.
    ///
    /// Returns `None` on overflow so a hostile `encoded_len` cannot wrap into a small allocation
    /// request.
    #[must_use]
    pub const fn total_len(&self) -> Option<usize> {
        match (self.encoded_len as usize).checked_add(BLOCK_HEADER_LEN) {
            Some(total) => Some(total),
            None => None,
        }
    }

    /// The codec selector.
    #[must_use]
    pub const fn codec(&self) -> u16 {
        block_codec(self.flags)
    }

    /// Whether the reserved flag bits are all zero.
    #[must_use]
    pub const fn reserved_flags_clear(&self) -> bool {
        self.flags & BLOCK_FLAGS_RESERVED_MASK == 0
    }
}

// ---------------------------------------------------------------------------
// CURRENT (`SGLC`) layout — §3, offsets are normative
// ---------------------------------------------------------------------------

/// Magic of the root pointer file.
pub const CURRENT_MAGIC: [u8; 4] = *b"SGLC";

/// The only `CURRENT` layout accepted here.
pub const CURRENT_LAYOUT: u16 = 1;

/// Fixed prefix before `payload_len`: magic, layout, flags.
pub const CURRENT_PREFIX_LEN: usize = 12;

/// Fixed bytes after the prefix and before the name: `kb_uuid` 16 + `version` 8 + `name_len` 2.
pub const CURRENT_FIXED_AFTER_PREFIX: usize = 26;

/// Bytes of `manifest_sha256` in `CURRENT`.
pub const CURRENT_SHA_LEN: usize = 32;

/// Bytes of `crc32c` at the end of `CURRENT`.
pub const CURRENT_CRC_LEN: usize = 4;

/// Offset of `name_len`.
pub const CURRENT_NAME_LEN_OFFSET: usize = 36;

/// Offset where the name bytes begin.
pub const CURRENT_NAME_OFFSET: usize = 38;

/// Protocol cap on the manifest name length, independent of any caller configuration.
pub const CURRENT_MAX_NAME_BYTES: usize = 64;

/// `payload_len` for a given name length: `62 + name_len` (`§3` rule 2).
#[must_use]
pub const fn current_payload_len(name_len: usize) -> usize {
    CURRENT_FIXED_AFTER_PREFIX + name_len + CURRENT_SHA_LEN + CURRENT_CRC_LEN
}

/// Total file length for a given name length: `12 + payload_len = 74 + name_len` (`§3` rule 3).
#[must_use]
pub const fn current_total_len(name_len: usize) -> usize {
    CURRENT_PREFIX_LEN + current_payload_len(name_len)
}

/// Number of leading bytes the `CURRENT` CRC covers: `70 + name_len` (`§3` rule 4).
///
/// The coverage includes `payload_len`, which is fine: `payload_len` is a function of `name_len`, and
/// `name_len` lies inside the covered range, so there is no circularity — determine `n`, compute
/// `payload_len`, then compute the CRC.
#[must_use]
pub const fn current_crc_coverage(name_len: usize) -> usize {
    CURRENT_NAME_OFFSET + name_len + CURRENT_SHA_LEN
}

/// Minimum digits in a manifest name's version segment (`§2.4`).
///
/// The grammar is `^v[0-9]{6,20}-[0-9a-f]{32}\.sgm$`: the number is the decimal form of the
/// manifest's `version`, **zero-padded to at least 6 digits**, and its value must be >= 1.
pub const MANIFEST_VERSION_MIN_DIGITS: usize = 6;

/// Maximum digits in a manifest name's version segment.
pub const MANIFEST_VERSION_MAX_DIGITS: usize = 20;

/// Validate a manifest file name against the grammar `CURRENT` requires (`§2.4`).
///
/// `^v[0-9]{6,20}-[0-9a-f]{32}\.sgm$`, with the numeric segment's value `>= 1`.
///
/// Two things this deliberately does **not** do, both of which an earlier revision of this
/// function got wrong and the golden files caught:
///
/// * it does not reject a leading zero — `v000001-…` is the canonical form, since the segment is
///   zero-padded by construction;
/// * it does not accept fewer than 6 digits — `v1-…` is malformed even though its value is fine.
///
/// Written as a predicate over bytes so it cannot allocate and cannot be fooled by a NUL.
#[must_use]
pub fn is_valid_manifest_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > CURRENT_MAX_NAME_BYTES {
        return false;
    }
    // A manifest name is ASCII-only by grammar; reject any byte outside the permitted set up front
    // so a NUL or a non-ASCII byte cannot ride through the component checks below.
    if !name
        .iter()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'.' | b'-'))
    {
        return false;
    }
    let Some(rest) = name.strip_prefix(b"v") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(b".sgm") else {
        return false;
    };
    let Some(dash) = rest.iter().position(|b| *b == b'-') else {
        return false;
    };
    let (version, id) = (&rest[..dash], &rest[dash + 1..]);
    if version.len() < MANIFEST_VERSION_MIN_DIGITS
        || version.len() > MANIFEST_VERSION_MAX_DIGITS
        || !version.iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    // Value must be >= 1: an all-zero segment denotes version 0, which does not exist.
    if version.iter().all(|b| *b == b'0') {
        return false;
    }
    id.len() == 32 && id.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
