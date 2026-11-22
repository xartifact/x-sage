//! Manifest file (kind = 1): the per-version commitment record.
//!
//! A manifest is `field_count:u16 | Field[field_count]` (§5) inside an `SGLB` envelope. It names the
//! transaction for its version and pins four digests, plus the previous manifest, forming the
//! committed chain that `CURRENT` points into.
//!
//! Pure: no filesystem access. Whether a manifest can be *found* is the store's question; whether the
//! bytes are a legal manifest is this module's.
//!
//! ## Two intentional asymmetries
//!
//! 1. **No self-hash.** `tx_file_sha256` (204) hashes *another* file, so nothing here is self-referential.
//!    A manifest cannot contain its own hash without a fixed-point search, which is why the file hash
//!    lives in `CURRENT` instead.
//! 2. **No build identity.** `engine_version` is deliberately absent (§5, storage-digests §4.8): a
//!    build string in a committed file would make golden vectors depend on the compiler and could not
//!    prove the semantics were unchanged.
//!
//! ## Cross-checks are split by who owns the fact
//!
//! Fields that must match the transaction it references (`8, 144, 163, 192, 196`) cannot be compared
//! inside this module: the transaction is a different file. [`ManifestHeader::check_against_transaction`]
//! takes those values from the caller, so the comparison exists in exactly one place and does not
//! require reading two files at once here.

use crate::wire::ReadError;
use sage_db_types::framing::{
    CorruptReason, FormatError, FormatOutcome, InternalReason, LimitKind, Limits, OperatorAction,
    Refusal, UnsupportedReason, classify,
};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;

use crate::file::Kind;
use crate::transaction::{Envelope, read_envelope};

/// Tag of `manifest.created_at` (§5).
pub const TAG_CREATED_AT: u16 = 8;
/// Tag of `manifest.kb_uuid` (§5).
pub const TAG_KB_UUID: u16 = 144;
/// Tag of `manifest.rules_digest` (§5).
pub const TAG_RULES_DIGEST: u16 = 163;
/// Tag of `manifest.version` (§5).
pub const TAG_VERSION: u16 = 192;
/// Tag of `manifest.rules_profile` (§5).
pub const TAG_RULES_PROFILE: u16 = 196;
/// Tag of `manifest.state_digest` (§5).
pub const TAG_STATE_DIGEST: u16 = 198;
/// Tag of `manifest.graph_digest` (§5).
pub const TAG_GRAPH_DIGEST: u16 = 199;
/// Tag of `manifest.chain_digest` (§5).
pub const TAG_CHAIN_DIGEST: u16 = 200;
/// Tag of `manifest.prev_manifest_name` (§5, optional).
pub const TAG_PREV_MANIFEST_NAME: u16 = 201;
/// Tag of `manifest.prev_manifest_sha256` (§5, optional, paired with 201).
pub const TAG_PREV_MANIFEST_SHA256: u16 = 202;
/// Tag of `manifest.tx_file_name` (§5).
pub const TAG_TX_FILE_NAME: u16 = 203;
/// Tag of `manifest.tx_file_sha256` (§5).
pub const TAG_TX_FILE_SHA256: u16 = 204;
/// Tag of `manifest.bookkeeping_digest` (§5).
pub const TAG_BOOKKEEPING_DIGEST: u16 = 205;

/// Bytes before the first manifest field: `field_count:u16`.
const MANIFEST_COUNT_LEN: usize = 2;

/// Tags §5 forbids in a manifest.
///
/// These belong to the transaction header or to the build, and accepting one would create a second
/// authority for a value that already has an owner.
const FORBIDDEN_IN_MANIFEST: [u16; 6] = [
    177, // request_id: transaction header
    178, // payload_digest: transaction header
    176, // principal_id: transaction header
    193, // tx_id: transaction header
    194, // prev_version: transaction header
    206, // request_canon_version: transaction header
];

/// A decoded manifest header.
///
/// The two names borrow the file's bytes rather than allocating: a manifest is read once per version
/// and both names are short and fixed-format, so an owned `String` would be an allocation per commit
/// for a value the caller only compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ManifestHeader<'a> {
    /// `192 version`.
    pub version: u64,
    /// `8 created_at`.
    pub created_at: i64,
    /// `144 kb_uuid`.
    pub kb_uuid: KbUuid,
    /// `163 rules_digest`.
    pub rules_digest: [u8; 32],
    /// `196 rules_profile`.
    pub rules_profile: u16,
    /// `198 state_digest`.
    pub state_digest: [u8; 32],
    /// `199 graph_digest`.
    pub graph_digest: [u8; 32],
    /// `200 chain_digest`.
    pub chain_digest: [u8; 32],
    /// `205 bookkeeping_digest`.
    pub bookkeeping_digest: [u8; 32],
    /// `203 tx_file_name`, borrowed.
    pub tx_file_name: &'a str,
    /// `204 tx_file_sha256`.
    pub tx_file_sha256: [u8; 32],
    /// `201 prev_manifest_name`, borrowed. Present iff `prev_manifest_sha256` is present.
    pub prev_manifest_name: Option<&'a str>,
    /// `202 prev_manifest_sha256`.
    pub prev_manifest_sha256: Option<[u8; 32]>,
}

impl ManifestHeader<'_> {
    /// Whether this is the genesis manifest, which has no predecessor.
    #[must_use]
    pub const fn is_genesis(&self) -> bool {
        self.version == 1
    }

    /// Whether the optional predecessor pair is consistently present or absent.
    ///
    /// §5 requires 201 and 202 to appear together or not at all. One without the other would leave a
    /// chain link that cannot be verified, which is worse than a broken link because it looks fine.
    #[must_use]
    pub const fn prev_pair_consistent(&self) -> bool {
        self.prev_manifest_name.is_some() == self.prev_manifest_sha256.is_some()
    }

    /// Cross-check the fields that must equal the referenced transaction's (§5).
    ///
    /// The caller supplies the transaction's values because the transaction is a different file.
    /// Comparing here keeps the rule in one place instead of in every caller.
    ///
    /// # Errors
    ///
    /// * `Corrupt(ManifestTxMismatch)` when any of the five shared fields differs, or when the
    ///   genesis/non-genesis predecessor rule is violated;
    /// * `Corrupt(PathName)` when a name does not satisfy its grammar.
    pub fn check_against_transaction(&self, tx: &TransactionFacts) -> FormatOutcome<()> {
        // `8, 144, 163, 192, 196` must be byte-for-byte equal.
        let agrees = self.created_at == tx.created_at
            && self.kb_uuid == tx.kb_uuid
            && self.rules_digest == tx.rules_digest
            && self.version == tx.version
            && self.rules_profile == tx.rules_profile;
        if !agrees {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch));
        }

        // The referenced file name and hash must agree with the transaction actually read.
        if tx
            .file_sha256
            .is_some_and(|expected_sha| self.tx_file_sha256 != expected_sha)
        {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch));
        }

        // §5: genesis has no predecessor; every later version must have one.
        if self.is_genesis() {
            if self.prev_manifest_name.is_some() || self.prev_manifest_sha256.is_some() {
                return FormatOutcome::Format(FormatError::Corrupt(
                    CorruptReason::ManifestTxMismatch,
                ));
            }
        } else if !self.prev_pair_consistent() || self.prev_manifest_name.is_none() {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ManifestTxMismatch));
        }

        if !crate::file::is_valid_manifest_name(self.tx_file_name.as_bytes())
            && !is_tx_file_name(self.tx_file_name)
        {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
                TAG_TX_FILE_NAME,
            )));
        }
        if self
            .prev_manifest_name
            .is_some_and(|name| !crate::file::is_valid_manifest_name(name.as_bytes()))
        {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
                TAG_PREV_MANIFEST_NAME,
            )));
        }

        FormatOutcome::Ok(())
    }
}

/// The transaction facts a manifest is checked against.
///
/// A struct rather than six positional arguments, so a caller cannot silently swap two same-typed
/// values (five of these are 32-byte digests or integers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionFacts {
    /// `192 version` of the transaction.
    pub version: u64,
    /// `8 created_at` of the transaction.
    pub created_at: i64,
    /// `144 kb_uuid` of the transaction.
    pub kb_uuid: KbUuid,
    /// `163 rules_digest` of the transaction.
    pub rules_digest: [u8; 32],
    /// `196 rules_profile` of the transaction.
    pub rules_profile: u16,
    /// SHA-256 of the whole transaction file, when the caller has computed it.
    pub file_sha256: Option<[u8; 32]>,
}

/// `^tx-[0-9a-f]{32}\.sgl$` — §2.4.
#[must_use]
pub fn is_tx_file_name(name: &str) -> bool {
    let b = name.as_bytes();
    // `tx-` is 3 bytes, the id 32, `.sgl` 4: 39 total.
    if b.len() != 3 + 32 + 4 {
        return false;
    }
    b.starts_with(b"tx-")
        && b.ends_with(b".sgl")
        && b[3..35]
            .iter()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// The result of reading a whole manifest file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ManifestSummary<'a> {
    /// Decoded header.
    pub header: ManifestHeader<'a>,
    /// The trailer's declared SHA-256, which `CURRENT` records as `manifest_sha256`.
    pub trailer_sha256: [u8; 32],
    /// Number of fields the header declared; must equal the number present.
    pub field_count: u16,
}

/// Read a manifest file.
///
/// Verifies the envelope (§2), then the header field set (§5). Field order must be strictly
/// ascending, the required set must be complete, and forbidden tags must be absent.
///
/// # Errors
///
/// * `UnsupportedFormat(FileKind)` when the file is not kind 1;
/// * `Corrupt` for envelope damage, field order, a missing/duplicate field, or a forbidden tag;
/// * `Internal(MissingRequired)` if the required set is incomplete after the policy check — that is
///   this build's own defect, not a claim about the file;
/// * `Limit` for a header over `max_header_fields` or a string over `max_string_bytes`;
/// * `Refused` for a ledger budget or caller stop.
pub fn read_manifest<'a>(
    bytes: &'a [u8],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<ManifestSummary<'a>> {
    let envelope: Envelope<'a> = match read_envelope(bytes, limits, ledger) {
        FormatOutcome::Ok(e) => e,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    if envelope.header.kind != Kind::Manifest {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::FileKind(
            Kind::Transaction.code(),
        )));
    }

    let body: &'a [u8] = match &envelope.body {
        std::borrow::Cow::Borrowed(b) => b,
        // A multi-block manifest is not expected, but the borrow must still be tied to 'a. The
        // envelope is moved into this scope, so re-borrow through it instead of the local.
        std::borrow::Cow::Owned(_) => {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::BlockBounds));
        }
    };
    if body.len() < MANIFEST_COUNT_LEN {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    let field_count = u16::from_le_bytes([body[0], body[1]]);
    if field_count == 0 {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }
    if u64::from(field_count) > limits.max_header_fields as u64 {
        return FormatOutcome::Format(FormatError::Limit(LimitKind::HeaderFields));
    }

    // The header field envelope (`tag:u16 | flags:u16 | encoded_len:u32 | value`) is owned by
    // `crate::event::read_header`: it already enforces the count ceiling, ascending strict order,
    // zero flags and non-zero lengths. Re-implementing it here would be a second authority on the
    // same bytes, which is exactly the drift this module avoids elsewhere.
    let mut reader = crate::wire::Reader::new(body);
    let fields = match crate::event::read_header(&mut reader, ledger) {
        Ok(fields) => fields,
        Err(error) => {
            return FormatOutcome::Format(match error {
                // A refusal is not a verdict: propagate it unchanged.
                crate::wire::ReadError::Budget(_) | crate::wire::ReadError::Stopped(_) => {
                    return match crate::dispatch::from_read_error(error) {
                        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
                        FormatOutcome::Format(e) => FormatOutcome::Format(e),
                        FormatOutcome::Ok(()) => {
                            FormatOutcome::Refused(Refusal::Allocation { requested: 0 })
                        }
                    };
                }
                crate::wire::ReadError::Data(crate::wire::DecodeError::CountTooLong { .. }) => {
                    FormatError::Limit(LimitKind::HeaderFields)
                }
                crate::wire::ReadError::Allocation { requested } => {
                    return FormatOutcome::Refused(Refusal::Allocation { requested });
                }
                _ => FormatError::Corrupt(CorruptReason::FieldCount),
            });
        }
    };
    if fields.len() != field_count as usize {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }

    let mut version = None;
    let mut created_at = None;
    let mut kb_uuid = None;
    let mut rules_digest = None;
    let mut rules_profile = None;
    let mut state_digest = None;
    let mut graph_digest = None;
    let mut chain_digest = None;
    let mut bookkeeping_digest = None;
    let mut tx_file_name = None;
    let mut tx_file_sha256 = None;
    let mut prev_manifest_name = None;
    let mut prev_manifest_sha256 = None;

    for field in &fields {
        let tag = field.tag();
        if FORBIDDEN_IN_MANIFEST.contains(&tag) {
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
        }
        let mut value = crate::wire::Reader::new(field.value());
        if let Err(error) = read_manifest_field(
            tag,
            &mut value,
            limits,
            &mut version,
            &mut created_at,
            &mut kb_uuid,
            &mut rules_digest,
            &mut rules_profile,
            &mut state_digest,
            &mut graph_digest,
            &mut chain_digest,
            &mut bookkeeping_digest,
            &mut tx_file_name,
            &mut tx_file_sha256,
            &mut prev_manifest_name,
            &mut prev_manifest_sha256,
        ) {
            return manifest_read_error(error);
        }
        if !value.is_empty() {
            // A field value with slack bytes means the value was not what its type says.
            return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
        }
    }

    // §5: no trailing bytes. A duplicate tag is caught by the ascending check; extra bytes mean the
    // declared field_count is wrong.
    if !reader.is_empty() {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::FieldCount));
    }

    macro_rules! required {
        ($value:expr, $tag:expr) => {
            match $value {
                Some(v) => v,
                None => {
                    return FormatOutcome::Format(FormatError::Internal(
                        InternalReason::MissingRequired {
                            event: None,
                            tag: $tag,
                        },
                    ));
                }
            }
        };
    }

    let header = ManifestHeader {
        version: required!(version, TAG_VERSION),
        created_at: required!(created_at, TAG_CREATED_AT),
        kb_uuid: required!(kb_uuid, TAG_KB_UUID),
        rules_digest: required!(rules_digest, TAG_RULES_DIGEST),
        rules_profile: required!(rules_profile, TAG_RULES_PROFILE),
        state_digest: required!(state_digest, TAG_STATE_DIGEST),
        graph_digest: required!(graph_digest, TAG_GRAPH_DIGEST),
        chain_digest: required!(chain_digest, TAG_CHAIN_DIGEST),
        bookkeeping_digest: required!(bookkeeping_digest, TAG_BOOKKEEPING_DIGEST),
        tx_file_name: required!(tx_file_name, TAG_TX_FILE_NAME),
        tx_file_sha256: required!(tx_file_sha256, TAG_TX_FILE_SHA256),
        prev_manifest_name,
        prev_manifest_sha256,
    };

    // §5: 201 and 202 appear together or not at all.
    if !header.prev_pair_consistent() {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
            TAG_PREV_MANIFEST_NAME,
        )));
    }
    if !is_tx_file_name(header.tx_file_name) {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
            TAG_TX_FILE_NAME,
        )));
    }
    if header
        .prev_manifest_name
        .is_some_and(|name| !crate::file::is_valid_manifest_name(name.as_bytes()))
    {
        return FormatOutcome::Format(FormatError::Corrupt(CorruptReason::PathName(
            TAG_PREV_MANIFEST_NAME,
        )));
    }

    FormatOutcome::Ok(ManifestSummary {
        header,
        trailer_sha256: envelope.trailer_sha256,
        field_count,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_manifest_field<'a>(
    tag: u16,
    reader: &mut crate::wire::Reader<'a>,
    limits: &Limits,
    version: &mut Option<u64>,
    created_at: &mut Option<i64>,
    kb_uuid: &mut Option<KbUuid>,
    rules_digest: &mut Option<[u8; 32]>,
    rules_profile: &mut Option<u16>,
    state_digest: &mut Option<[u8; 32]>,
    graph_digest: &mut Option<[u8; 32]>,
    chain_digest: &mut Option<[u8; 32]>,
    bookkeeping_digest: &mut Option<[u8; 32]>,
    tx_file_name: &mut Option<&'a str>,
    tx_file_sha256: &mut Option<[u8; 32]>,
    prev_manifest_name: &mut Option<&'a str>,
    prev_manifest_sha256: &mut Option<[u8; 32]>,
) -> Result<(), ReadError> {
    // A malformed field body is corruption of the field stream, not a limit or a foreign version.
    // A malformed value is corruption; a refusal from the ledger is not, and must travel intact.
    let corrupt = || {
        ReadError::Data(crate::wire::DecodeError::Truncated {
            offset: 0,
            wanted: 0,
        })
    };

    match tag {
        TAG_CREATED_AT => *created_at = Some(reader.i64().map_err(|_| corrupt())?),
        TAG_KB_UUID => {
            let raw = reader.take_exact(16).map_err(|_| corrupt())?;
            let mut b = [0u8; 16];
            b.copy_from_slice(raw);
            *kb_uuid = Some(KbUuid::from_bytes(b));
        }
        TAG_RULES_DIGEST => *rules_digest = Some(reader.digest32().map_err(|_| corrupt())?),
        TAG_VERSION => *version = Some(reader.u64().map_err(|_| corrupt())?),
        TAG_RULES_PROFILE => *rules_profile = Some(reader.u16().map_err(|_| corrupt())?),
        TAG_STATE_DIGEST => *state_digest = Some(reader.digest32().map_err(|_| corrupt())?),
        TAG_GRAPH_DIGEST => *graph_digest = Some(reader.digest32().map_err(|_| corrupt())?),
        TAG_CHAIN_DIGEST => *chain_digest = Some(reader.digest32().map_err(|_| corrupt())?),
        TAG_BOOKKEEPING_DIGEST => {
            *bookkeeping_digest = Some(reader.digest32().map_err(|_| corrupt())?)
        }
        TAG_TX_FILE_SHA256 => *tx_file_sha256 = Some(reader.digest32().map_err(|_| corrupt())?),
        TAG_PREV_MANIFEST_SHA256 => {
            *prev_manifest_sha256 = Some(reader.digest32().map_err(|_| corrupt())?)
        }
        TAG_TX_FILE_NAME => {
            let max = limits.max_string_bytes;
            *tx_file_name = Some(crate::field::read_str_borrowed(reader, max).map_err(
                |e| match e {
                    // Over-length is a ceiling refusal, not damage: never "restore your backup".
                    ReadError::Data(crate::wire::DecodeError::StringTooLong { .. }) => e,
                    other => other,
                },
            )?);
        }
        TAG_PREV_MANIFEST_NAME => {
            *prev_manifest_name = Some(crate::field::read_str_borrowed(
                reader,
                limits.max_string_bytes,
            )?);
        }
        // An unregistered manifest field is a foreign file, not an ignorable extra.
        _ => {
            return Err(ReadError::Uninterpretable(
                crate::registry::SchemaError::UnknownField {
                    event: 0,
                    field: tag,
                },
            ));
        }
    }
    Ok(())
}

/// Map a manifest field failure, preserving refusals and mapping a too-long string to a ceiling.
fn manifest_read_error<T>(error: ReadError) -> FormatOutcome<T> {
    match error {
        // The wire error carries the length and the maximum, so the ceiling refusal is
        // exact. It does NOT carry the field tag, and this bridge sits below the call
        // site, so naming one of the two manifest name fields would be a guess.
        ReadError::Data(crate::wire::DecodeError::StringTooLong { len, max }) => {
            FormatOutcome::Format(FormatError::Limit(LimitKind::String {
                tag: None,
                len,
                max,
            }))
        }
        // Everything else goes through the shared bridge, which keeps Budget/Stopped/Allocation as
        // refusals rather than folding them into a bytes verdict.
        other => crate::dispatch::from_read_error(other),
    }
}

/// The operator action implied by a manifest read.
#[must_use]
pub fn classify_manifest<T>(outcome: &FormatOutcome<T>) -> OperatorAction {
    classify(outcome)
}

/// Explicitly reject a refusal as a classification.
///
/// Present so a caller cannot accidentally write `matches!(x, Format(..))` and treat a budget refusal
/// as a verdict; the type system does the rest.
#[must_use]
pub fn is_refusal<T>(outcome: &FormatOutcome<T>) -> bool {
    outcome.as_refusal().is_some()
}
