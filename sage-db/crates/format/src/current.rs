//! `CURRENT` (`SGLC`) codec: the root pointer file.
//!
//! `CURRENT` is not an `SGLB` file. It is the one mutable file in the layout and the **sole commit
//! authority**, so its encoding is exactly specified by offset and byte-identical for identical
//! content (`storage-format.md` §3).
//!
//! Pure: no filesystem, clock or environment access. Writing `CURRENT` to disk — and the
//! `CURRENT.tmp` → `rename` → directory-sync protocol around it — is the store's job.
//!
//! ## Shapes
//!
//! * [`DecodedCurrent`] is the decoded form: fixed inline storage, no heap, so a hostile `name_len`
//!   cannot drive an allocation.
//! * [`CurrentToEncode`] is the *write input*. Reading and writing have different obligations:
//!   encoding must reject a name the layout could never contain, while decoding must accept exactly
//!   what the bytes say and call malformed bytes `Corrupt`. Both are fallible, and neither exposes a
//!   field a caller could set to an invalid value.
//!
//! ## Why the ceiling comes from `decode_current_bound`
//!
//! `74 + name_len` is an arithmetic bound, but a configured `max_name_bytes` must not manufacture a
//! larger exemption than the protocol allows. [`decode_current_bound`] already resolves that (protocol
//! cap, and the smaller of the two file ceilings), so this module asks it instead of recomputing — a
//! second computation is how two owners come to disagree about the maximum size of one file.

use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, LimitKind, Limits, OperatorAction, Refusal,
    UnsupportedReason, classify, decode_current_bound,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;

use crate::file::{
    CURRENT_CRC_LEN, CURRENT_FIXED_AFTER_PREFIX, CURRENT_LAYOUT, CURRENT_MAGIC,
    CURRENT_MAX_NAME_BYTES, CURRENT_NAME_LEN_OFFSET, CURRENT_NAME_OFFSET, CURRENT_PREFIX_LEN,
    CURRENT_SHA_LEN, current_crc_coverage, current_payload_len, current_total_len,
    is_valid_manifest_name,
};

/// A decoded root pointer with fixed inline storage.
///
/// The name lives in a `[u8; 64]` rather than a `String`: the maximum is a protocol constant, so
/// decoding has no reason to allocate, and an owned name would be one more thing mutable after
/// validation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedCurrent {
    /// Immutable KB identity.
    pub kb_uuid: KbUuid,
    /// Committed version this pointer names.
    pub version: u64,
    /// Digest of the manifest file named here (file SHA-256, not a domain digest).
    pub manifest_sha256: [u8; 32],
    /// Inline name bytes; only `name_len` of them are meaningful.
    name: [u8; CURRENT_MAX_NAME_BYTES],
    /// Length of the meaningful prefix of `name`.
    name_len: u16,
}

impl DecodedCurrent {
    /// The manifest file name, borrowed from inline storage.
    ///
    /// Cannot fail: both construction paths validate ASCII and length first.
    #[must_use]
    pub fn manifest_name(&self) -> &str {
        // Validated at construction. A failure would be a bug in this module; the empty string is
        // the only total fallback that does not panic on a decode path.
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }

    /// Length of the manifest file name in bytes.
    #[must_use]
    pub const fn name_len(&self) -> u16 {
        self.name_len
    }

    /// Bytes a re-encoding of this value must produce, per §3.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        current_total_len(self.name_len as usize)
    }

    /// Build from already-validated parts. Private: both public paths validate first.
    fn from_validated(
        kb_uuid: KbUuid,
        version: u64,
        name: &[u8],
        manifest_sha256: [u8; 32],
    ) -> Self {
        let mut inline = [0u8; CURRENT_MAX_NAME_BYTES];
        inline[..name.len()].copy_from_slice(name);
        Self {
            kb_uuid,
            version,
            manifest_sha256,
            name: inline,
            name_len: name.len() as u16,
        }
    }
}

/// The write input for `CURRENT`.
///
/// Separate from [`DecodedCurrent`] because a writer names a version that does not exist on disk
/// yet, so it starts from parts rather than bytes. Construction is fallible and enforces the same
/// grammar the decoder enforces, so this type cannot represent a `CURRENT` that [`decode_current`]
/// would reject.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CurrentToEncode {
    /// Immutable KB identity.
    pub kb_uuid: KbUuid,
    /// Version being published; must be >= 1.
    ///
    /// Private, with [`CurrentToEncode::version`] as the read accessor.
    ///
    /// **What privacy buys, exactly:** it makes *direct* `c.version = 0` uncompilable. It does **not**
    /// make a zero version unrepresentable, because [`DecodedCurrent::version`] is public and
    /// [`CurrentToEncode::from_decoded`] copies it — so a caller can still reach an illegal write input
    /// by decoding a valid file, mutating the decoded value, and converting. The encoder's explicit
    /// `version == 0` check is therefore **load-bearing, not defence in depth**, and
    /// `a_zero_version_reached_through_from_decoded_is_refused` is the test that proves it.
    version: u64,
    /// Digest of the manifest file being named.
    pub manifest_sha256: [u8; 32],
    /// Inline name bytes.
    name: [u8; CURRENT_MAX_NAME_BYTES],
    /// Meaningful length.
    name_len: u16,
}

impl CurrentToEncode {
    /// Build a write input, validating the manifest name and version.
    ///
    /// # Errors
    ///
    /// `Corrupt(PathName)` when the name cannot be a manifest file name; `Corrupt(RevisionNotMonotonic)`
    /// when `version` is 0, which has no representation because `CURRENT` names a committed version.
    pub fn new(
        kb_uuid: KbUuid,
        version: u64,
        manifest_name: &str,
        manifest_sha256: [u8; 32],
    ) -> FormatOutcome<Self> {
        if version == 0 {
            return FormatOutcome::Format(FormatError::Corrupt(
                CorruptReason::RevisionNotMonotonic {
                    expected: 1,
                    found: 0,
                },
            ));
        }
        if !is_valid_manifest_name(manifest_name.as_bytes()) {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(0)));
        }
        let mut inline = [0u8; CURRENT_MAX_NAME_BYTES];
        inline[..manifest_name.len()].copy_from_slice(manifest_name.as_bytes());
        FormatOutcome::Ok(Self {
            kb_uuid,
            version,
            manifest_sha256,
            name: inline,
            name_len: manifest_name.len() as u16,
        })
    }

    /// Derive a write input from an already-decoded value (e.g. re-publishing a version).
    #[must_use]
    pub fn from_decoded(c: &DecodedCurrent) -> Self {
        Self {
            kb_uuid: c.kb_uuid,
            version: c.version,
            manifest_sha256: c.manifest_sha256,
            name: c.name,
            name_len: c.name_len,
        }
    }

    /// The manifest file name this pointer will name.
    #[must_use]
    pub fn manifest_name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }

    /// The version this pointer will name.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

/// Decode a `CURRENT` file.
///
/// Order is deliberate: **size, then structure, then CRC**. The ceiling is checked against
/// [`decode_current_bound`] before any field is interpreted, so an oversized buffer is a limit
/// rather than a parse; and the CRC is verified only once the structure is legal, because a CRC over
/// a malformed layout has nothing meaningful to report.
///
/// # Errors
///
/// * `UnsupportedFormat(Magic)` when the magic is not `SGLC` — including an `SGLB` header, which is a
///   different file, not an older root pointer;
/// * `Limit(CurrentFile)` when the buffer exceeds the effective ceiling;
/// * `Corrupt` for truncation, bad layout, non-zero flags, a `payload_len`/length disagreement, an
///   invalid name, or a failed CRC;
/// * `Refused` for a ledger budget or caller stop.
pub fn decode_current(
    bytes: &[u8],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<DecodedCurrent> {
    // Admission before interpretation: even a small file is a scan the caller budgets for.
    if let Err(error) = ledger.try_charge(bytes.len() as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }

    let bound = decode_current_bound(limits);
    if bytes.len() as u64 > bound {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::CurrentFile));
    }

    let fixed_min =
        CURRENT_PREFIX_LEN + CURRENT_FIXED_AFTER_PREFIX + CURRENT_SHA_LEN + CURRENT_CRC_LEN;
    if bytes.len() < fixed_min {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
    }

    if bytes[0..4] != CURRENT_MAGIC {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::Magic));
    }
    let layout = u16::from_le_bytes([bytes[4], bytes[5]]);
    if layout != CURRENT_LAYOUT {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags));
    }
    let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
    if flags != 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderFlags));
    }

    let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let name_len = u16::from_le_bytes([
        bytes[CURRENT_NAME_LEN_OFFSET],
        bytes[CURRENT_NAME_LEN_OFFSET + 1],
    ]) as usize;

    // `payload_len = 62 + name_len` and `total = 74 + name_len` are one identity; check both
    // against the actual length instead of trusting either independently.
    if payload_len != current_payload_len(name_len) || bytes.len() != current_total_len(name_len) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PayloadLenMismatch));
    }
    if name_len == 0 || name_len > CURRENT_MAX_NAME_BYTES {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(0)));
    }

    let name_end = CURRENT_NAME_OFFSET + name_len;
    let name = &bytes[CURRENT_NAME_OFFSET..name_end];
    if !is_valid_manifest_name(name) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(0)));
    }

    let crc_at = current_crc_coverage(name_len);
    let stored = u32::from_le_bytes([
        bytes[crc_at],
        bytes[crc_at + 1],
        bytes[crc_at + 2],
        bytes[crc_at + 3],
    ]);
    if stored != crc32c::crc32c(&bytes[..crc_at]) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::HeaderCrc));
    }

    let mut kb = [0u8; 16];
    kb.copy_from_slice(&bytes[12..28]);
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&bytes[name_end..name_end + CURRENT_SHA_LEN]);

    FormatOutcome::Ok(DecodedCurrent::from_validated(
        KbUuid::from_bytes(kb),
        u64::from_le_bytes([
            bytes[28], bytes[29], bytes[30], bytes[31], bytes[32], bytes[33], bytes[34], bytes[35],
        ]),
        name,
        sha,
    ))
}

/// Encode a `CURRENT` file into a caller-provided buffer.
///
/// The buffer is the caller's, not a returned `Vec`, so its growth is visible to the caller's ledger.
/// The size is exact from `name_len`, so it is reserved once and cannot be re-grown by hostile input.
///
/// # Errors
///
/// * `Limit(CurrentFile)` if the result would exceed the effective ceiling (unreachable for a
///   validated name under `Limits::V1`, but checked rather than assumed);
/// * `Refused(Allocation)` if the buffer cannot be reserved;
/// * `Refused(Budget)` / `Refused(Stopped)` from the ledger.
pub fn encode_current(
    c: &CurrentToEncode,
    out: &mut Vec<u8>,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<()> {
    // ---- Contract, checked before anything is charged or mutated ----
    //
    // `out` must be EMPTY. The returned bytes are a whole `CURRENT` file, and its length, its
    // `payload_len` and its CRC coverage are all derived from `name_len`. If `out` already held bytes,
    // appending would produce a file whose real length disagrees with every one of those fields, and the
    // CRC would be computed over the caller's prior content as well.
    //
    // Silently clearing `out` is deliberately NOT done: a caller that passed a buffer with content is
    // telling us something is wrong with its expectations, and wiping that evidence would hide it.
    // Refusing leaves the buffer exactly as the caller left it.
    if !out.is_empty() {
        // `Internal`, not `Corrupt`: nothing is wrong with the *data*. An encoder that requires a
        // pristine destination buffer is stating a usage invariant, and calling that "your file is
        // damaged" would send an operator to a backup for a caller-side mistake.
        return FormatOutcome::Format(FormatError::Internal(
            sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
        ));
    }

    let name_len = c.name_len as usize;
    let total = current_total_len(name_len);

    // ---- Revalidate caller-supplied values ----
    //
    // This check is **load-bearing**, not defence in depth. `version` is private, so direct mutation is
    // uncompilable — but `DecodedCurrent::version` is public and `CurrentToEncode::from_decoded` copies
    // it, so an illegal write input IS reachable from outside this module. Removing this check would make
    // `a_zero_version_reached_through_from_decoded_is_refused` fail. The encoder must never emit a root
    // pointer that `decode_current` would refuse.
    if c.version == 0 {
        return FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::RevisionNotMonotonic {
                expected: 1,
                found: 0,
            },
        ));
    }
    if !is_valid_manifest_name(&c.name[..name_len]) {
        return FormatOutcome::Format(FormatError::Corrupt(
            sage_db_types::framing::CorruptReason::PathName(0),
        ));
    }

    if total as u64 > decode_current_bound(limits) {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::CurrentFile));
    }
    if let Err(error) = ledger.try_charge(total as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    if out.try_reserve_exact(total).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation { requested: total });
    }

    out.extend_from_slice(&CURRENT_MAGIC);
    out.extend_from_slice(&CURRENT_LAYOUT.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(current_payload_len(name_len) as u32).to_le_bytes());
    out.extend_from_slice(c.kb_uuid.as_bytes());
    out.extend_from_slice(&c.version.to_le_bytes());
    out.extend_from_slice(&(name_len as u16).to_le_bytes());
    out.extend_from_slice(&c.name[..name_len]);
    out.extend_from_slice(&c.manifest_sha256);

    debug_assert_eq!(
        out.len(),
        current_crc_coverage(name_len),
        "the CRC must cover exactly the bytes emitted so far"
    );
    out.extend_from_slice(&crc32c::crc32c(out).to_le_bytes());
    debug_assert_eq!(out.len(), total);

    FormatOutcome::Ok(())
}

/// The operator action implied by a `CURRENT` outcome.
#[must_use]
pub fn classify_current<T>(outcome: &FormatOutcome<T>) -> OperatorAction {
    classify(outcome)
}
