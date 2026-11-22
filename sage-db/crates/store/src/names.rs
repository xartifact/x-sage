//! Typed path names and containment.
//!
//! The store never hands a caller-supplied `&str` to the operating system. Every path it opens is
//! built from a closed set of [`Family`] directories plus an [`EntryName`] whose textual form is
//! produced by this module, so a traversal (`..`), an absolute path, a separator inside a name, or
//! an unexpected file family is **not representable** rather than merely rejected at runtime.
//!
//! Rationale, in one line: a validation function can be forgotten at one call site, but a type
//! that cannot be constructed wrongly fails at compile time. The accepted S2 review raised exactly
//! this against the earlier draft, where `CURRENT` and `LOCK` were `&'static str` that no typed
//! operation could accept.

use core::fmt;

// The family set is declared ONCE, in the seam (`crate::fs::Family`), and re-exported here.
// An earlier revision of this module declared its own identically-shaped enum; two `Family` types
// would force a conversion at every call site and could silently diverge, which is the same
// duplication the `ControlStop` re-export exists to prevent.
pub use crate::fs::Family;

/// Every family, in a stable order.
///
/// Used by capability checks and tests. Deliberately **not** used to select a commit root: only
/// `CURRENT` may name a root, never a directory listing.
pub const FAMILIES: [Family; 6] = [
    Family::Manifests,
    Family::Transactions,
    Family::Blobs,
    Family::Checkpoints,
    Family::Staging,
    Family::Cache,
];

/// Why a name was refused at construction.
///
/// These are *programming* errors surfaced as values, not data-corruption verdicts: a caller that
/// produces one has built a path wrongly, which is what the type exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NameError {
    /// Empty, or longer than the 64-byte entry cap.
    Length,
    /// Contains a byte outside `[0-9a-z.-]` for a manifest name, or outside the hex/ASCII set the
    /// specific constructor allows.
    Character,
    /// Contains a path separator, a NUL, or is `.`/`..`.
    Traversal,
}

impl NameError {
    /// Stable name for logs and typed payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Character => "character",
            Self::Traversal => "traversal",
        }
    }
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for NameError {}

/// A 128-bit attempt, transaction or record identity, rendered as 32 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Id128([u8; 16]);

impl Id128 {
    /// Wrap raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Render 16 bytes as 32 lowercase hex characters, without allocating a `String` from `format!`
/// in a hot path.
#[must_use]
pub fn encode_id128(id: &Id128) -> [u8; 32] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 32];
    for (i, byte) in id.as_bytes().iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    out
}

/// Fixed names in a KB root or its offline backup artifact.
///
/// `CURRENT` is the only mutable file in the layout; `CURRENT.tmp` is its fixed staging name (the
/// format forbids more than one `.tmp` name, and the KB lock gives a single writer); `LOCK` is the
/// zero-length file whose descriptor carries the exclusive lock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RootName {
    /// `CURRENT` — the unique commit root.
    Current,
    /// `CURRENT.tmp` — staged bytes awaiting an atomic replace.
    CurrentTmp,
    /// `LOCK` — held by the exclusive-lock descriptor for the handle's lifetime.
    Lock,
    /// `backup.inventory` — an offline backup's file inventory; never a commit root.
    BackupInventory,
    /// `backup.complete` — written last after the offline backup is verified and synced.
    BackupComplete,
}

impl RootName {
    /// The exact file name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "CURRENT",
            Self::CurrentTmp => "CURRENT.tmp",
            Self::Lock => "LOCK",
            Self::BackupInventory => "backup.inventory",
            Self::BackupComplete => "backup.complete",
        }
    }
}

impl fmt::Display for RootName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a staged file will become when published.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StagingKind {
    /// Staged blob bytes (`<sha256>` once published).
    Blob,
    /// Staged transaction segment.
    Tx,
    /// Staged manifest.
    Manifest,
    /// Staged `CURRENT` bytes.
    Current,
}

impl StagingKind {
    /// The prefix used in the staging file name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tx => "tx",
            Self::Manifest => "manifest",
            Self::Current => "current",
        }
    }
}

/// Every file the store may address, with its textual form derived here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryName {
    /// One of the fixed root or offline-backup metadata files.
    Root(RootName),
    /// `blobs/<64 hex>`.
    Blob([u8; 32]),
    /// `transactions/tx-<32 hex>.sgl`.
    Transaction(Id128),
    /// `manifests/v<version>-<32 hex>.sgm`.
    Manifest {
        /// Committed version this manifest describes; must be ≥ 1.
        version: u64,
        /// Manifest identity.
        id: Id128,
    },
    /// `checkpoints/cp-<32 hex>.sgb` (reserved in major 0).
    Checkpoint(Id128),
    /// `staging/<kind>-<32 hex>.part`.
    Staging {
        /// What the staged bytes will become.
        kind: StagingKind,
        /// Attempt identity.
        id: Id128,
    },
}

impl EntryName {
    /// Which family directory the entry lives in, or `None` for a root file.
    #[must_use]
    pub const fn family(self) -> Option<Family> {
        match self {
            Self::Root(_) => None,
            Self::Blob(_) => Some(Family::Blobs),
            Self::Transaction(_) => Some(Family::Transactions),
            Self::Manifest { .. } => Some(Family::Manifests),
            Self::Checkpoint(_) => Some(Family::Checkpoints),
            Self::Staging { .. } => Some(Family::Staging),
        }
    }

    /// The exact file name, as an **unbounded convenience** renderer.
    ///
    /// This allocates a `String` infallibly and takes **no ledger**: the capacity is reserved by
    /// the allocator, not admitted by a budget, and a refusal is impossible. Use
    /// `render_name_bounded` on any path where a refusal must be observable before memory is
    /// committed — in particular inside a commit attempt, where an abort here would be an
    /// unrefusable allocation after the fence point.
    ///
    /// The two renderers share one emitter, so the bytes are identical; only the accounting
    /// differs.
    #[must_use]
    pub fn to_file_name(self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    /// Append this name to `out` using the canonical grammar.
    ///
    /// The single emitter: `to_file_name` and `render_name_bounded` both call this, so the bytes
    /// and the grammar cannot diverge between the infallible and the admitted path. Private
    /// because it has exactly two local callers and no consumer: a public method here would
    /// invite a third renderer that skips whichever accounting its caller needed.
    fn write_into(self, out: &mut String) {
        match self {
            Self::Root(name) => out.push_str(name.as_str()),
            Self::Blob(digest) => push_hex32(out, &digest),
            Self::Transaction(id) => {
                out.push_str("tx-");
                push_hex16(out, &id);
                out.push_str(".sgl");
            }
            // §2.4: the numeric segment is the decimal version, zero-padded to AT LEAST six digits, and
            // at most 20 (u64's width). `v1-...` is malformed; `v000001-...` is canonical.
            Self::Manifest { version, id } => {
                out.push('v');
                push_padded_u64(out, version);
                out.push('-');
                push_hex16(out, &id);
                out.push_str(".sgm");
            }
            Self::Checkpoint(id) => {
                out.push_str("cp-");
                push_hex16(out, &id);
                out.push_str(".sgb");
            }
            Self::Staging { kind, id } => {
                out.push_str(kind.as_str());
                out.push('-');
                push_hex16(out, &id);
                out.push_str(".part");
            }
        }
    }

    /// The family-relative path, or the bare name for a root file.
    #[must_use]
    pub fn to_relative_path(self) -> String {
        match self.family() {
            None => self.to_file_name(),
            Some(family) => format!("{}/{}", family.as_str(), self.to_file_name()),
        }
    }
}

/// Append the 32 lowercase hex characters of an `Id128`.
///
/// `encode_id128` already yields those 32 ASCII characters; an earlier version hex-encoded its output a second time
/// and produced 64 characters, which made every transaction name fail its own grammar.
fn push_hex16(out: &mut String, id: &Id128) {
    for byte in encode_id128(id) {
        out.push(byte as char);
    }
}

/// Append the 64 lowercase hex characters of a digest without an intermediate `String`.
fn push_hex32(out: &mut String, digest: &[u8; 32]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

/// Append a `u64` zero-padded to at least six decimal digits.
///
/// Written by hand so the renderer allocates nothing of its own: `format!("{v:06}")` would build a temporary.
fn push_padded_u64(out: &mut String, value: u64) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    let mut v = value;
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // Pad to six digits.
    while buf.len() - at < 6 {
        at -= 1;
        buf[at] = b'0';
    }
    for byte in &buf[at..] {
        out.push(*byte as char);
    }
}

/// Maximum filename length the format allows (protocol cap, not a configuration knob).
pub const MAX_NAME_BYTES: usize = 64;

/// Whether `name` is a syntactically valid manifest file name: `v<digits>-<32 hex>.sgm`.
///
/// This is the read-side check: a name that does not match is refused before any index into it, so
/// a malformed `CURRENT` cannot steer the store to an arbitrary file.
#[must_use]
pub fn is_valid_entry_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return false;
    }
    // No separators, no NUL, no dot-only forms: those are traversal, not names.
    if name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return false;
    }
    validate_manifest_name(name)
        || validate_tx_name(name)
        || validate_checkpoint_name(name)
        || validate_staging_name(name)
        || validate_blob_name(name)
        || name == RootName::Current.as_str()
        || name == RootName::CurrentTmp.as_str()
        || name == RootName::Lock.as_str()
        || name == RootName::BackupInventory.as_str()
        || name == RootName::BackupComplete.as_str()
}

/// What a walked on-disk name looks like, **without ever failing**.
///
/// Distinct from [`is_valid_entry_name`]: that is a gate (refuse or accept), while this is a
/// *classifier* for reporting. A directory walk may encounter names the engine would never write —
/// an operator's stray file, a stale staging entry from an older build — and an orphan report has
/// to group them rather than fail on them.
///
/// `Unrecognised` is a first-class outcome, not an error: it means "present on disk, not
/// engine-generated". Reporting such a name is the whole reason the walk yields `&str`.
///
/// Using this for a **commit decision** is forbidden; only the content of `CURRENT` may name a
/// root. It exists for diagnostics and orphan grouping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalkedName<'a> {
    /// Looks like `v<version>-<32 hex>.sgm`.
    Manifest {
        /// The version digits, as written.
        version: &'a str,
    },
    /// Looks like `tx-<32 hex>.sgl`.
    Transaction,
    /// Looks like `cp-<32 hex>.sgb` (reserved in this phase).
    Checkpoint,
    /// Looks like `<kind>-<32 hex>.part`.
    Staging {
        /// The staging kind as written (`blob`, `tx`, `manifest`, `current`).
        kind: &'a str,
    },
    /// A 64-hex content name.
    Blob,
    /// One of the fixed root or offline-backup metadata files.
    Root(RootName),
    /// Present on disk but not engine-generated. Preserved verbatim for the report.
    Unrecognised(&'a str),
}

/// Classify a walked name. Total: every input yields a variant.
#[must_use]
pub fn classify_walked(name: &str) -> WalkedName<'_> {
    if name == RootName::Current.as_str() {
        return WalkedName::Root(RootName::Current);
    }
    if name == RootName::CurrentTmp.as_str() {
        return WalkedName::Root(RootName::CurrentTmp);
    }
    if name == RootName::Lock.as_str() {
        return WalkedName::Root(RootName::Lock);
    }
    if name == RootName::BackupInventory.as_str() {
        return WalkedName::Root(RootName::BackupInventory);
    }
    if name == RootName::BackupComplete.as_str() {
        return WalkedName::Root(RootName::BackupComplete);
    }
    // One grammar, two consumers: validity comes from the delegated validator, and this arm only extracts the
    // version substring for the report. The previous inline copy carried its own `!starts_with('0')`, so the
    // walk classifier rejected every padded name the renderer produces and accepted the unpadded form format
    // refuses — the same contradiction, in the second entry point.
    if validate_manifest_name(name)
        && let Some(version) = name
            .strip_prefix('v')
            .and_then(|rest| rest.split_once('-'))
            .map(|(version, _)| version)
    {
        return WalkedName::Manifest { version };
    }
    if name
        .strip_prefix("tx-")
        .and_then(|r| r.strip_suffix(".sgl"))
        .is_some_and(is_hex32)
    {
        return WalkedName::Transaction;
    }
    if name
        .strip_prefix("cp-")
        .and_then(|r| r.strip_suffix(".sgb"))
        .is_some_and(is_hex32)
    {
        return WalkedName::Checkpoint;
    }
    if let Some(rest) = name.strip_suffix(".part")
        && let Some((kind, id)) = rest.split_once('-')
        && matches!(kind, "blob" | "tx" | "manifest" | "current")
        && is_hex32(id)
    {
        return WalkedName::Staging { kind };
    }
    if is_hex(name, 64) {
        return WalkedName::Blob;
    }
    WalkedName::Unrecognised(name)
}

fn validate_manifest_name(name: &str) -> bool {
    // One grammar, owned by format. A store-side re-implementation disagreed with it in both directions:
    // it rejected the canonical `v000001-...` that D10 uses (leading-zero check) and accepted `v1-...`
    // that format refuses. Delegating removes the possibility of another divergence.
    sage_db_format::file::is_valid_manifest_name(name.as_bytes())
}

fn validate_tx_name(name: &str) -> bool {
    name.strip_prefix("tx-")
        .and_then(|r| r.strip_suffix(".sgl"))
        .is_some_and(is_hex32)
}

fn validate_checkpoint_name(name: &str) -> bool {
    name.strip_prefix("cp-")
        .and_then(|r| r.strip_suffix(".sgb"))
        .is_some_and(is_hex32)
}

fn validate_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".part") else {
        return false;
    };
    let Some((kind, id)) = rest.split_once('-') else {
        return false;
    };
    matches!(kind, "blob" | "tx" | "manifest" | "current") && is_hex32(id)
}

fn validate_blob_name(name: &str) -> bool {
    // A blob name is the hex form of a 32-BYTE digest: 64 hex characters, not 32.
    // (My first revision reused `is_hex32`, which made every real blob name invalid; the
    // render/validate round-trip test caught it.)
    is_hex(name, 64)
}

fn is_hex32(s: &str) -> bool {
    is_hex(s, 32)
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Render a name into a caller-owned `String`, admitting the capacity first.
///
/// [`EntryName::to_file_name`] uses `format!`/`to_owned`, which allocates infallibly and uncharged. On the commit
/// path that is the wrong shape: an allocation failure there aborts the process instead of refusing. This renders
/// the SAME canonical grammar (the byte rule and the grammar stay owned by [`EntryName`]) into an admitted buffer,
/// so it can refuse.
///
/// The longest name is a manifest's `v` + 20 digits + `-` + 32 hex + `.sgm` = 58 bytes, so the capacity is exact.
///
/// # Errors
///
/// [`sage_db_types::framing::Refusal::Budget`] when the ledger refuses the admission,
/// [`sage_db_types::framing::Refusal::Stopped`] when the caller stops,
/// [`sage_db_types::framing::Refusal::Allocation`] when the buffer cannot be reserved.
pub fn render_name_bounded(
    name: EntryName,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<String, sage_db_types::framing::Refusal> {
    use sage_db_types::framing::Refusal;

    // Exact worst case per variant, so the reservation never needs to grow.
    let capacity = match name {
        EntryName::Root(root) => root.as_str().len(),
        EntryName::Blob(_) => 64,
        EntryName::Transaction(_) | EntryName::Checkpoint(_) => 39,
        EntryName::Manifest { .. } => 58,
        EntryName::Staging { kind, .. } => kind.as_str().len() + 1 + 32 + 5,
    };

    ledger
        .try_charge(capacity as u64)
        .map_err(Refusal::Budget)?;
    ledger.checkpoint().map_err(Refusal::Stopped)?;
    let mut out = String::new();
    if out.try_reserve_exact(capacity).is_err() {
        return Err(Refusal::Allocation {
            requested: capacity,
        });
    }
    // The canonical emitter renders into `out`; there is no temporary `String` and no `format!`.
    name.write_into(&mut out);
    Ok(out)
}

/// Copy a string, admitting its length and reserving before the copy.
///
/// `String::clone` on a bounded path allocates infallibly and uncharged. This does the same work in the order the
/// resource rule requires: admit, check control, reserve exactly, then copy.
///
/// # Errors
///
/// As [`render_name_bounded`].
pub fn copy_value_bounded(
    value: &str,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<String, sage_db_types::framing::Refusal> {
    use sage_db_types::framing::Refusal;

    ledger
        .try_charge(value.len() as u64)
        .map_err(Refusal::Budget)?;
    ledger.checkpoint().map_err(Refusal::Stopped)?;
    let mut out = String::new();
    if out.try_reserve_exact(value.len()).is_err() {
        return Err(Refusal::Allocation {
            requested: value.len(),
        });
    }
    out.push_str(value);
    Ok(out)
}
