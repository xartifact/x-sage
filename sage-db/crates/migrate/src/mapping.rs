//! Legacy source extraction plan → canonical native record mapping.
//!
//! # What this crate is
//!
//! Import-only tooling. It consumes the **actual version-1 source extraction plan** produced by
//! the extraction tool, together with the source archive it names, and produces:
//!
//! * canonical native record events (`OwnedEvent`) built by the **canonical writers** through
//!   `sage_db_store::typed` — never a hand-rolled codec;
//! * genuine `BlobDeclaration`s for the content those records reference;
//! * one **self-contained import package** whose bytes carry the plan, the mapping, the
//!   provenance and every one of the source files' raw bytes.
//!
//! # What it is not
//!
//! * Not an executor: it writes no filesystem root, opens no `Store`, and commits nothing.
//!   Persistence, resume and completion are the next batch.
//! * Not a record codec: `serde`/`serde_json` here read the input plan and write the package.
//!   Every native record is a canonical type encoded by its canonical writer.
//! * Not a lossless-target claim. A successful mapping means "every source object became a
//!   canonical record with its provenance", not "the target reproduces the old system".
//!
//! # The attachment package, and why it exists
//!
//! The old corpus is 57 Markdown files, 15 JSON files and one `.bak` — no images. Evidence is
//! *not* a path alias for source files (`data-model` §2.1) and `MediaType` is image-only, so
//! manufacturing 73 Evidence or Asset records would misdescribe the data.
//!
//! Instead the 73 files live inside the **import package**, which is committed as the genuine
//! `ImportSession.plan_ref` blob. Tag 185 (`plan_ref`) is in the canonical required-ref set, so
//! the bytes survive replay rather than becoming an unreferenced orphan. A source path is a
//! package entry with its exact length, hash and bytes — never a hash-only reference to
//! something outside the package.
//!
//! Consequence, stated plainly: **attachment path count (73) and native typed row count are
//! different quantities.** The package carries 73 files and produces zero native
//! Evidence/Asset rows for them; that is the design, not a loss.

use sage_db_format::assertion::AssertionEvent;
use sage_db_format::term::TermEvent;
use sage_db_store::typed::{self, EventInput};
use sage_db_types::RelationId;
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::{FormatOutcome, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, CountPair, Extensions, FrontierEntryRecord,
    FrontierStatus, IdAllocationEvent, IdSpace, ImportSessionRecord, ImportStage, KbMetadataRecord,
    OptionalActorAudit, ProposalKind, ProposalRecord, ProposalStatus, RecordKey, RecordKind,
    RevPair, SessionCheckpointRecord, StateKind, TermRecord, TermSource, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent};

// ---------------------------------------------------------------------------
// Errors and refusals
// ---------------------------------------------------------------------------

/// Ceilings the caller sets before any growth.
///
/// Declared here rather than derived from the plan: the plan is *input*, and an input file must
/// not be able to raise the ceiling its own processing is allowed to spend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MappingLimits {
    /// Bytes of plan input this mapping may even parse.
    ///
    /// Checked BEFORE the JSON decoder runs; the decoder is a named allocator boundary and
    /// must not be the first thing a hostile plan reaches.
    pub max_plan_bytes: u64,
    /// Source files the package may carry.
    pub max_files: u64,
    /// Total raw source bytes the package may carry.
    pub max_bytes: u64,
    /// Native records the mapping may emit, across all families.
    pub max_records: u64,
}

/// Why a mapping did not produce a preparation.
///
/// The arms answer different questions, and an operator acts on them differently. In
/// particular a resource refusal is **not** an invalid source: the same input can succeed with a
/// larger budget, so reporting it as corruption would send an operator to restore a backup.
/// Which plan array an error was found in, plus its row.
///
/// This replaces cloning an arbitrary-length source path into the error: the caller indexes its
/// own plan with `(array, row)` and recovers the exact path, so nothing is truncated and nothing
/// is fabricated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceArray {
    /// `plan.terms`.
    Terms,
    /// `plan.assertions`.
    Assertions,
    /// `plan.proposals`.
    Proposals,
    /// `plan.checkpoints`.
    Checkpoints,
    /// `plan.frontier`.
    Frontier,
    /// `plan.attachments`.
    Attachments,
    /// `plan.unresolved`.
    Unresolved,
    /// `plan.errors`.
    Errors,
    /// A field of the plan's `source` block.
    Source,
    /// The plan as a whole.
    Plan,
    /// Another file the plan names (`kb.json`).
    File,
}

impl SourceArray {
    /// Stable name for diagnostics and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terms => "terms",
            Self::Assertions => "assertions",
            Self::Proposals => "proposals",
            Self::Checkpoints => "checkpoints",
            Self::Frontier => "frontier",
            Self::Attachments => "attachments",
            Self::Unresolved => "unresolved",
            Self::Errors => "errors",
            Self::Source => "source",
            Self::Plan => "plan",
            Self::File => "file",
        }
    }
}

/// Where in the source an error was detected, without owning any source string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceLocation {
    /// Which array or block.
    pub array: SourceArray,
    /// Row index within that array.
    pub row: usize,
    /// The plan field involved, when the error is about one.
    pub field: Option<&'static str>,
}

impl SourceLocation {
    /// A location at one row of one array.
    #[must_use]
    pub const fn row(array: SourceArray, row: usize) -> Self {
        Self {
            array,
            row,
            field: None,
        }
    }

    /// The same location naming the field involved.
    #[must_use]
    pub const fn with_field(self, field: &'static str) -> Self {
        Self {
            field: Some(field),
            ..self
        }
    }

    /// A whole-plan location.
    #[must_use]
    pub const fn plan() -> Self {
        Self::row(SourceArray::Plan, 0)
    }

    /// A field of the plan's `source` block.
    #[must_use]
    pub const fn source(field: &'static str) -> Self {
        Self {
            array: SourceArray::Source,
            row: 0,
            field: Some(field),
        }
    }

    /// A named file the plan depends on.
    #[must_use]
    pub const fn file(field: &'static str) -> Self {
        Self {
            array: SourceArray::File,
            row: 0,
            field: Some(field),
        }
    }
}

/// Why a mapping did not produce a preparation.
///
/// **No variant owns heap data.** Every arm is scalars, a `Copy` canonical payload, or
/// `&'static str`, so reporting a failure never allocates — which matters most exactly when the
/// ledger is exhausted and a refusal must still be describable. Callers recover the exact source
/// path by indexing their own plan with `SourceLocation`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MappingError {
    /// The caller's ledger or control refused the work.
    ///
    /// `Refusal` is `Copy` and `BudgetError` is three scalars: budget exhaustion needs no string.
    Refused(Refusal),
    /// The input is not a usable extraction plan.
    InvalidSource {
        /// Where it was found.
        location: SourceLocation,
        /// What is wrong, as a static label.
        detail: &'static str,
    },
    /// A source object requires a canonical capability this importer does not have.
    Unsupported {
        /// Where it was found.
        location: SourceLocation,
        /// Why, as a static label.
        detail: &'static str,
    },
    /// The caller is not an authenticated Admin on the target KB.
    Unauthorized {
        /// Which check failed.
        detail: &'static str,
    },
    /// A canonical writer rejected a value this crate constructed.
    ///
    /// The canonical `FormatError` is `Copy` and preserved verbatim, so the exact cause survives
    /// with no formatted string.
    CanonicalRejected {
        /// Where it happened.
        location: SourceLocation,
        /// The canonical refusal, preserved rather than described.
        reason: sage_db_types::framing::FormatError,
    },
    /// This crate's own invariant failed.
    ///
    /// Distinct from `CanonicalRejected`: here the defect is ours, and the label is a static
    /// identifier rather than a description of input data.
    Internal {
        /// Where it happened.
        location: SourceLocation,
        /// Which invariant, as a static label.
        invariant: &'static str,
    },
}

impl From<Refusal> for MappingError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<sage_db_types::query::ControlStop> for MappingError {
    fn from(stop: sage_db_types::query::ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}

impl From<sage_db_types::ledger::BudgetError> for MappingError {
    fn from(error: sage_db_types::ledger::BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}

// ---------------------------------------------------------------------------
// Executor context — the NEW construction facts, never source history
// ---------------------------------------------------------------------------

/// Who is performing the import, and when.
///
/// Every field here describes the **new** target construction. None of it is source data:
/// the source's own author, timestamp and revision, where unknown, stay separately recorded as
/// unavailable rather than being recovered from this context.
///
/// There is deliberately **no principal field**: the acting identity comes from
/// `Grants::authorize`, which is the only value that has actually been checked. A caller-supplied
/// name would be an unchecked claim, and recording it as `Authenticated` is exactly the shadow
/// authentication this crate must not offer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExecutorContext {
    /// The import event's commit time, in UTC microseconds — the target's `created_at`.
    ///
    /// This is when the import ran, never when the source document was written.
    pub import_time: i64,
    /// Why this construction is happening; recorded on every constructed record.
    pub reason: String,
    /// The import session identity this mapping belongs to.
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// The import package
// ---------------------------------------------------------------------------

/// One source file inside the package: a path, its exact length, hash and bytes.
///
/// The bytes are carried **inside** the package. A package that named a hash and expected the
/// reader to find the bytes elsewhere would be an external dependency, and after a restart there
/// would be nothing to resolve it against.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PackageFile {
    /// Path as the source tree named it, POSIX separators.
    pub path: String,
    /// Exact length of `bytes`.
    pub length: u64,
    /// SHA-256 of `bytes`, lowercase hex.
    pub sha256: String,
    /// The actual bytes.
    pub bytes: Vec<u8>,
}

/// The self-contained import package, as bytes plus their digest.
///
/// This is a **control artifact**, versioned and documented, not a record codec: nothing here is
/// a canonical record. It is committed as `ImportSession.plan_ref` and read back with
/// `read_committed_blob` after a restart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImportPackage {
    /// SHA-256 of the package bytes; this *is* the `plan_ref` value.
    pub sha256: [u8; 32],
    /// Declared length of the payload, verified against the blob that owns it.
    pub len: u64,
}

/// The package format version.
pub const PACKAGE_VERSION: u32 = 1;

/// The source context the package records, so a reader needs nothing outside it.
///
/// These are the extraction plan's own binding facts, carried verbatim: which archive the bytes
/// came from, what authority the source had, what the plan counted. Copying them in costs a few
/// hundred bytes and removes every "ask the operator for the plan again" dependency.
#[derive(serde::Serialize)]
pub struct PackageProvenance<'a> {
    /// The extraction plan's own label for the run that produced it.
    pub archive_dir: &'a str,
    /// What authority the source KB had when extracted.
    pub authority: &'a str,
    /// The source KB identity.
    pub kb_id: &'a str,
    /// Files the extraction saw.
    pub files: u64,
    /// Bytes the extraction saw.
    pub bytes: u64,
    /// The extraction's own completeness verdict, preserved rather than restated.
    pub extraction: &'a str,
    /// **Every** unresolved requirement, not just the first — borrowed, never re-copied.
    pub unresolved: Vec<RequirementView<'a>>,
}

/// One source-id → target-id assignment, borrowed for package serialization.
#[derive(serde::Serialize)]
pub struct PackageMapping<'a> {
    /// The family this assignment belongs to, e.g. `term`.
    pub family: &'a str,
    /// The source identity, verbatim.
    pub source: &'a str,
    /// The target identifier assigned by this construction.
    pub target_id: u128,
}

/// Build the package's mapping view from the ONE in-memory `PathMapping`.
///
/// Every field is borrowed from that representation, so building the view allocates only the
/// small entry structs — and those slots are admitted by the caller before this runs.
fn package_mapping_of<'a>(mapping: &'a PathMapping, out: &mut Vec<PackageMapping<'a>>) {
    for (family, entries) in [
        ("term", &mapping.term),
        ("assertion", &mapping.assertion),
        ("proposal", &mapping.proposal),
        ("checkpoint", &mapping.checkpoint),
        ("frontier", &mapping.frontier),
    ] {
        for entry in entries {
            out.push(PackageMapping {
                family,
                source: &entry.source,
                target_id: entry.target_id,
            });
        }
    }
}

/// The target context a restart needs: identity, expectations and construction facts.
#[derive(serde::Serialize)]
pub struct PackageTarget<'a> {
    /// The target KB identity the executor will write under.
    pub kb_uuid: &'a str,
    /// The import session this package belongs to.
    pub session_id: &'a str,
    /// The import event time used for every constructed `created_at`.
    pub import_time: i64,
    /// The reason recorded on constructed records.
    pub reason: &'a str,
    /// The authenticated principal the executor must re-authorize.
    pub principal: &'a str,
    /// Expected rows per state kind, ascending by kind number. Includes the Genesis singletons.
    pub expected_counts: Vec<CountView<'a>>,
    /// High-water marks per id space, ascending by space number.
    pub watermarks: &'a [PackageWatermark],
}

/// A borrowed view of one frozen requirement, serialized without copying its strings.
#[derive(serde::Serialize)]
pub struct RequirementView<'a> {
    /// The plan's family, e.g. `term`.
    pub family: &'a str,
    /// The object it applies to.
    pub subject: &'a str,
    /// The field, e.g. `created_at`.
    pub field: &'a str,
    /// Why the extraction could not resolve it.
    pub reason: &'a str,
}

/// A borrowed view of one expected count, serialized from the session's own pairs.
#[derive(serde::Serialize)]
pub struct CountView<'a> {
    /// State kind number, 1..=13.
    pub kind: u16,
    /// Expected rows.
    pub count: &'a u64,
}

/// One high-water mark in the package.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct PackageWatermark {
    /// Id space number.
    pub space: u16,
    /// Largest allocated id.
    pub hwm: u128,
}

/// Serialise the package: one JSON object, canonical key order, files sorted by path.
///
/// JSON with strict validation on read rather than a bespoke binary container: the bytes are an
/// auditor's artifact, and a reviewer can read them without running this tool.
///
/// The package is **self-contained**: it carries the actual plan bytes, the complete target map,
/// every disposition with the value actually constructed, the expected counts, the watermarks and
/// every source file's bytes. After a restart `plan_ref` alone retains the mapping decisions.
struct PackageInput<'a> {
    plan_bytes: &'a [u8],
    archive_content_digest: &'a str,
    source_digest: &'a str,
    mapping: &'a [PackageMapping<'a>],
    dispositions: &'a [Disposition],
    target: &'a PackageTarget<'a>,
    provenance: &'a PackageProvenance<'a>,
    files: &'a [PackageFile],
}

fn encode_package(
    input: PackageInput<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, MappingError> {
    let PackageInput {
        plan_bytes,
        archive_content_digest,
        source_digest,
        mapping,
        dispositions,
        target,
        provenance,
        files,
    } = input;

    // ---- serialize by reference: no intermediate owned copy of the payloads ----------
    //
    // `serde_json` cannot be handed a pre-reserved `Vec`, and inventing a second JSON writer
    // would be a second authority. Instead the *serde output* is admitted up front by an exact
    // upper bound derived from the data itself, then the writer is told to fill a buffer with
    // that capacity, so its growth is not the first allocation after admission.
    #[derive(serde::Serialize)]
    struct Entry<'a> {
        path: &'a str,
        length: u64,
        sha256: &'a str,
        bytes_base64: Base64View<'a>,
    }
    #[derive(serde::Serialize)]
    struct PackageDisposition<'a> {
        requirement: &'a str,
        subject: &'a str,
        policy: &'a str,
        constructed: &'a str,
        reason: &'a str,
    }
    #[derive(serde::Serialize)]
    struct Package<'a> {
        package_version: u32,
        /// SHA-256 of the actual plan bytes carried below — an independent binding, so a reader
        /// can prove the plan it holds is the plan this mapping was built from.
        plan_digest: &'a str,
        plan_bytes_base64: Base64View<'a>,
        /// The extraction's own archive-content digest, labelled for what it is.
        archive_content_digest: &'a str,
        /// The extraction's own source-tree digest, labelled for what it is.
        source_digest: &'a str,
        provenance: &'a PackageProvenance<'a>,
        target: &'a PackageTarget<'a>,
        mapping: &'a [PackageMapping<'a>],
        dispositions: &'a [PackageDisposition<'a>],
        files: &'a [Entry<'a>],
    }

    let plan_hex_buf = hex(&digest_of(plan_bytes));
    let plan_hex = hex_str(&plan_hex_buf);

    let mut sorted: Vec<&PackageFile> = Vec::new();
    admit_and_reserve(&mut sorted, files.len(), ledger)?;
    for file in files {
        sorted.push(file);
    }
    sorted.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    for window in sorted.windows(2) {
        if window[0].path == window[1].path {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "the plan lists the same source path twice with different content",
            });
        }
    }

    let mut entries: Vec<Entry<'_>> = Vec::new();
    admit_and_reserve(&mut entries, sorted.len(), ledger)?;
    for file in &sorted {
        entries.push(Entry {
            path: &file.path,
            length: file.length,
            sha256: &file.sha256,
            bytes_base64: Base64View(&file.bytes),
        });
    }

    let mut views: Vec<PackageDisposition<'_>> = Vec::new();
    admit_and_reserve(&mut views, dispositions.len(), ledger)?;
    for disposition in dispositions {
        views.push(PackageDisposition {
            requirement: &disposition.requirement,
            subject: &disposition.subject,
            policy: &disposition.policy,
            constructed: &disposition.constructed,
            reason: &disposition.reason,
        });
    }

    let package = Package {
        package_version: PACKAGE_VERSION,
        plan_digest: plan_hex,
        plan_bytes_base64: Base64View(plan_bytes),
        archive_content_digest,
        source_digest,
        provenance,
        target,
        mapping,
        dispositions: &views,
        files: &entries,
    };

    // ---- S1: count the EXACT output with the SAME canonical serializer, then admit it ----
    //
    // Pass 1 serializes into a sink that only counts and allocates nothing; its result is the
    // exact package length. That length is admitted and reserved exactly, then pass 2
    // serializes the SAME immutable view through a bounded sink that refuses growth past the
    // reservation. Both passes use `serde_json::to_writer`: one serialization authority.
    let mut counter = CountingSink { bytes: 0 };
    if serde_json::to_writer(&mut counter, &package).is_err() {
        return Err(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "package counting pass failed",
        });
    }
    let exact = counter.bytes;

    admit_bytes(ledger, exact)?;
    let exact_usize = usize::try_from(exact).map_err(|_| MappingError::Internal {
        location: SourceLocation::plan(),
        invariant: "the serialized package length is not addressable here",
    })?;
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve_exact(exact_usize).is_err() {
        return Err(MappingError::Refused(Refusal::Allocation {
            requested: exact_usize,
        }));
    }
    let written = {
        let mut sink = BoundedSink {
            out: &mut out,
            limit: exact_usize,
        };
        if serde_json::to_writer(&mut sink, &package).is_err() {
            return Err(MappingError::Internal {
                location: SourceLocation::plan(),
                invariant: "bounded serialization disagreed with the counted length",
            });
        }
        sink.out.len()
    };
    if written != exact_usize {
        return Err(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "serialized length disagreed with the counting pass",
        });
    }
    Ok(out)
}

/// A `Write` sink that only counts. It allocates nothing, so it may run before admission.
struct CountingSink {
    bytes: u64,
}

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len =
            u64::try_from(buf.len()).map_err(|_| std::io::Error::other("chunk exceeds u64"))?;
        self.bytes = self
            .bytes
            .checked_add(len)
            .ok_or_else(|| std::io::Error::other("package length overflows u64"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `Write` sink over a pre-reserved buffer that REFUSES to exceed its admitted limit.
struct BoundedSink<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next = self
            .out
            .len()
            .checked_add(buf.len())
            .ok_or_else(|| std::io::Error::other("package length overflow"))?;
        if next > self.limit {
            return Err(std::io::Error::other(
                "serialized package would exceed its admitted length",
            ));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialize `bytes` as standard padded base64 without an intermediate `String`.
struct Base64View<'a>(&'a [u8]);

impl serde::Serialize for Base64View<'_> {
    /// Write base64 **directly into the serializer**, with no intermediate `String`.
    ///
    /// The previous shape built a `String` inside `serialize`, which is an unadmitted
    /// allocation on the serialization path. `serialize_str` needs a contiguous `&str`, so it
    /// cannot be used here; `collect_str` accepts any `Display`, and base64 output is pure
    /// ASCII with no escaping, so the display adapter emits exactly the final bytes.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&Base64Display(self.0))
    }
}

/// `Display` adapter that emits standard padded base64 without building a `String`.
struct Base64Display<'a>(&'a [u8]);

impl core::fmt::Display for Base64Display<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in self.0.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).copied().map_or(0, u32::from);
            let b2 = chunk.get(2).copied().map_or(0, u32::from);
            let triple = (b0 << 16) | (b1 << 8) | b2;
            let digits = [
                TABLE[(triple >> 18) as usize & 0x3f],
                TABLE[(triple >> 12) as usize & 0x3f],
                if chunk.len() > 1 {
                    TABLE[(triple >> 6) as usize & 0x3f]
                } else {
                    b'='
                },
                if chunk.len() > 2 {
                    TABLE[triple as usize & 0x3f]
                } else {
                    b'='
                },
            ];
            // Four ASCII bytes; a failed write cannot happen for a String-backed formatter.
            formatter.write_str(core::str::from_utf8(&digits).map_err(|_| core::fmt::Error)?)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The prepared import
// ---------------------------------------------------------------------------

/// The projected native rows, by state kind.
/// Row counts per state kind — a fixed 13-slot array, not a map.
///
/// Exactly 13 state kinds exist, so a map here would be an infallible-growing, unmeasurable
/// structure holding a bounded set. The array is exact, allocation-free, and cannot fail.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProjectedCounts {
    counts: [u64; 13],
}

impl ProjectedCounts {
    /// An all-zero set of counts.
    #[must_use]
    pub const fn new() -> Self {
        Self { counts: [0; 13] }
    }

    /// The slot index for `kind`.
    fn slot(kind: StateKind) -> usize {
        kind_number(kind) as usize - 1
    }

    /// Add one row to `kind`, refusing overflow.
    fn increment(&mut self, kind: StateKind) -> Result<(), MappingError> {
        let slot = Self::slot(kind);
        self.counts[slot] = self.counts[slot]
            .checked_add(1)
            .ok_or(MappingError::Internal {
                location: SourceLocation::plan(),
                invariant: "a row count overflows u64",
            })?;
        Ok(())
    }

    /// The count recorded for `kind`.
    #[must_use]
    pub fn get(&self, kind: StateKind) -> u64 {
        self.counts[Self::slot(kind)]
    }

    /// Iterate `(kind, count)` in ascending kind order for non-zero kinds.
    pub fn iter(&self) -> impl Iterator<Item = (StateKind, u64)> + '_ {
        StateKind::ALL
            .into_iter()
            .map(move |kind| (kind, self.get(kind)))
            .filter(|(_, count)| *count > 0)
    }

    /// How many distinct kinds have a non-zero count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.counts.iter().filter(|count| **count > 0).count()
    }

    /// Whether no kind has a non-zero count.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything a later executor needs to persist this import.
///
/// There is deliberately no `ready` or `complete` field: this type's existence means the
/// *mapping* succeeded. Whether the target is ready is a separate, later judgement made after
/// the executor commits and verifies.
pub struct PreparedImport {
    /// The owned buffers AND the reservation that keeps their credit.
    ///
    /// Private and inseparable: a caller cannot destructure the vectors away from the token by
    /// safe code, so the owned bytes cannot outlive the credit that pays for them. Access is
    /// through the borrowed accessors below, which keep the owner alive for the borrow.
    owned: OwnedOutput,
}

/// The owned buffers of a preparation, together with the reservation covering them.
///
/// Every field here is measured by `complete_output_extent`, so the reservation is the real
/// extent of what the caller receives — not an estimate of some of it.
struct OwnedOutput {
    events: Vec<OwnedEvent>,
    blobs: Vec<BlobDeclaration>,
    package: ImportPackage,
    import_session: ImportSessionRecord,
    path_mapping: PathMapping,
    dispositions: Vec<Disposition>,
    watermarks: Vec<PackageWatermark>,
    /// Adopted from credit already charged, never a fresh admission.
    retained: sage_db_store::lease::OwnedReservation,
}

impl PreparedImport {
    /// Canonical native record events, in construction order.
    #[must_use]
    pub fn events(&self) -> &[OwnedEvent] {
        &self.owned.events
    }

    /// Every blob those events require, including the package payload.
    #[must_use]
    pub fn blobs(&self) -> &[BlobDeclaration] {
        &self.owned.blobs
    }

    /// The package the executor must publish at `ImportSession.plan_ref`.
    #[must_use]
    pub fn package(&self) -> &ImportPackage {
        &self.owned.package
    }

    /// The package payload bytes, borrowed from their single owner.
    ///
    /// There is exactly one copy: the `Provide` payload of the blob whose digest is
    /// `package().sha256`. Returning a borrow rather than a second `Vec` is what makes the
    /// accounting honest — two real allocations would both have to be charged.
    ///
    /// # Errors
    ///
    /// [`MappingError::Internal`] if the blob carrying `package().sha256` is absent, which would
    /// mean the prepared value is self-inconsistent.
    pub fn package_bytes(&self) -> Result<&[u8], MappingError> {
        self.owned
            .blobs
            .iter()
            .find(|blob| blob.sha256 == self.owned.package.sha256)
            .and_then(|blob| match &blob.source {
                BlobSource::Provide(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .ok_or_else(|| MappingError::Internal {
                location: SourceLocation::plan(),
                invariant: "the package payload has no owning blob",
            })
    }

    /// The `ImportSession` stage record naming the package and expected counts.
    #[must_use]
    pub fn import_session(&self) -> &ImportSessionRecord {
        &self.owned.import_session
    }

    /// Expected native rows per state kind, ascending by kind.
    #[must_use]
    pub fn expected_counts(&self) -> &[CountPair] {
        &self.owned.import_session.expected_counts
    }

    /// Expected rows for one state kind, when it was counted.
    #[must_use]
    pub fn count_of(&self, kind: StateKind) -> Option<u64> {
        self.owned
            .import_session
            .expected_counts
            .iter()
            .find(|pair| pair.kind == kind)
            .map(|pair| pair.count)
    }

    /// Deterministic source-id to target-id mapping.
    #[must_use]
    pub fn path_mapping(&self) -> &PathMapping {
        &self.owned.path_mapping
    }

    /// Every source diagnostic this construction resolved, with its disposition.
    #[must_use]
    pub fn dispositions(&self) -> &[Disposition] {
        &self.owned.dispositions
    }

    /// High-water marks per id space, from the MAX assigned id in each.
    #[must_use]
    pub fn watermarks(&self) -> &[PackageWatermark] {
        &self.owned.watermarks
    }

    /// Bytes this result actually retains, as measured at construction.
    ///
    /// Public so a caller can size a budget against the real figure rather than a guess.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.owned.retained.bytes()
    }

    /// Ledger statistics for the counter that owns this preparation's credit.
    #[must_use]
    pub fn retained_stats(&self) -> sage_db_store::lease::LedgerStats {
        self.owned.retained.stats()
    }
}

/// One source identity and the target id this construction assigned it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MappingEntry {
    /// The source identity, verbatim.
    pub source: String,
    /// The target id assigned.
    pub target_id: u128,
}

/// Deterministic identity mapping: source identity to the target id assigned.
///
/// Retained in sorted `Vec`s rather than `BTreeMap`s. A map's retained nodes are real memory but
/// expose no capacity, so they cannot be measured or admitted; a `Vec` can be reserved fallibly
/// and its capacity read. Entries are kept sorted by source identity, which is also the
/// deterministic order a reader needs.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct PathMapping {
    /// Concept slug to target term id.
    pub term: Vec<MappingEntry>,
    /// Assertion source id to target assertion id.
    pub assertion: Vec<MappingEntry>,
    /// Source proposal path to target proposal id.
    pub proposal: Vec<MappingEntry>,
    /// Source checkpoint path to target checkpoint id.
    pub checkpoint: Vec<MappingEntry>,
    /// Frontier source id to target entry id.
    pub frontier: Vec<MappingEntry>,
    /// Source file path to its package entry path (identity), for the 73 attachments.
    pub attachment: Vec<MappingEntry>,
    /// Attachment entry path, parallel to `attachment` by index.
    pub attachment_target: Vec<String>,
}

impl PathMapping {
    /// Append one identity mapping, admitting the entry and both strings.
    fn push(
        family: &mut Vec<MappingEntry>,
        source: &str,
        target_id: u128,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), MappingError> {
        admit_and_reserve(family, 1, ledger)?;
        family.push(MappingEntry {
            source: admitted_string(ledger, source)?,
            target_id,
        });
        Ok(())
    }
}

/// How one previously-unresolved source requirement was resolved by this construction.
///
/// The old plan's `unresolved` list is **frozen history**: it is not cleared, and it is not
/// treated as a permanent blocker either. Each entry gets a disposition here, naming the source
/// diagnostic, the policy applied, the value constructed, and the reason.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Disposition {
    /// The source requirement this resolves, e.g. `term.created_at`.
    pub requirement: String,
    /// The source object it applies to, e.g. a slug or `(all)`.
    pub subject: String,
    /// The administrative policy applied, quoted from `TARGET-MAPPING-DECISIONS.md`.
    pub policy: String,
    /// The value this construction sets, or `unavailable` when none may be invented.
    pub constructed: String,
    /// Why that is the honest answer.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Source-side vocabulary decoding (input only)
// ---------------------------------------------------------------------------

/// Map the plan's declared confidence string onto the canonical enum.
fn confidence_of(value: &str) -> Result<Confidence, &'static str> {
    match value {
        "confirmed" => Ok(Confidence::Confirmed),
        "hypothetical" => Ok(Confidence::Hypothetical),
        "contested" => Ok(Confidence::Contested),
        _ => Err("confidence is not one of confirmed/hypothetical/contested"),
    }
}

/// Map the plan's declared source string onto the canonical enum.
fn term_source_of(value: &str) -> Result<TermSource, &'static str> {
    match value {
        "personal" => Ok(TermSource::Personal),
        "domain" => Ok(TermSource::Domain),
        "synthesis" => Ok(TermSource::Synthesis),
        _ => Err("source is not one of personal/domain/synthesis"),
    }
}

/// Map the plan's `layer` number onto the canonical enum.
fn layer_of(value: u64) -> Result<SemanticLayer, &'static str> {
    match value {
        0 => Ok(SemanticLayer::L0),
        1 => Ok(SemanticLayer::L1),
        2 => Ok(SemanticLayer::L2),
        3 => Ok(SemanticLayer::L3),
        _ => Err("layer is outside 0..=3"),
    }
}

/// Map an authored edge relation onto the canonical relation registry.
///
/// The seven authored types map one-to-one; nothing is folded, reversed or normalised.
fn relation_of(value: &str) -> Result<RelationId, &'static str> {
    match value {
        "is-a" => Ok(RelationId::IsA),
        "part-of" => Ok(RelationId::PartOf),
        "instantiates" => Ok(RelationId::Instantiates),
        "depends-on" => Ok(RelationId::DependsOn),
        "enables" => Ok(RelationId::Enables),
        "contradicts" => Ok(RelationId::Contradicts),
        "analogous-to" => Ok(RelationId::AnalogousTo),
        _ => Err("relation is not one of the seven authored edge types"),
    }
}

/// The `CommonFields` of a NEW import event.
///
/// `actor_origin` is `Authenticated` because the principal came from the server's verification.
/// The source's own declared author — when it had one — is a *different* field on the record
/// (`legacy_*`, or `declared_author` on a proposal), never this one.
fn import_audit(
    ledger: &mut dyn SharedLedger,
    principal: &str,
    context: &ExecutorContext,
    legacy_id: &str,
    location: SourceLocation,
) -> Result<CommonFields, MappingError> {
    let reason = admitted_format(
        ledger,
        format_args!(
            "legacy import of {legacy_id} from {} row {}",
            location.array.as_str(),
            location.row
        ),
    )?;
    Ok(CommonFields {
        recorded_by: Some(admitted_string(ledger, principal)?),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some(reason),
        created_at: context.import_time,
        updated_at: None,
    })
}

/// The authored triple an edge proposal asserts, read from its own payload bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
struct EdgeProposal<'a> {
    /// The `from` concept slug, which is also the proposal's canonical subject.
    ///
    /// Borrowed from the payload: only the spelling is needed for a lookup, and the raw payload
    /// is retained verbatim as the record's blob, so a copy would be pure waste.
    from: &'a str,
    /// The relation, as authored.
    relation: &'a str,
    /// The `target` concept slug. It stays in the payload; it is not the subject.
    target: &'a str,
    /// The authored confidence.
    confidence: &'a str,
}

/// Parse a legacy edge-proposal payload: `## 新边提案` followed by `- **from**: …` lines.
///
/// The source file states the triple explicitly, so this reads it rather than inferring a
/// subject. Parsing is deliberately narrow — four named bullets — and anything else in the file
/// (the rationale prose, the YAML `proposer`/`date`) remains part of the retained raw payload
/// blob rather than being reinterpreted as a native field.
fn read_edge_payload<'a>(
    payload: &'a [u8],
    location: SourceLocation,
) -> Result<EdgeProposal<'a>, MappingError> {
    let text = core::str::from_utf8(payload).map_err(|_| MappingError::InvalidSource {
        location: SourceLocation::plan(),
        detail: "an edge proposal payload is not UTF-8",
    })?;
    let field = |name: &str| -> Option<&'a str> {
        for line in text.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("- **") else {
                continue;
            };
            let Some((key, value)) = rest.split_once("**:") else {
                continue;
            };
            if key == name {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        None
    };
    let missing = |what: &'static str| MappingError::InvalidSource {
        location,
        detail: what,
    };
    Ok(EdgeProposal {
        from: field("from").ok_or_else(|| missing("an edge proposal states no `from`"))?,
        relation: field("type").ok_or_else(|| missing("an edge proposal states no `type`"))?,
        target: field("target").ok_or_else(|| missing("an edge proposal states no `target`"))?,
        confidence: field("confidence")
            .ok_or_else(|| missing("an edge proposal states no `confidence`"))?,
    })
}

// ---------------------------------------------------------------------------
// Admitted, fallible growth
// ---------------------------------------------------------------------------
//
// Every buffer that survives into `PreparedImport` is admitted against the caller's ledger
// BEFORE it grows, and the growth itself is fallible. `Vec::with_capacity`, `collect` and bare
// `push` are infallible in a way this crate cannot afford: an allocation failure would abort the
// process rather than refuse the import.

/// Admit `bytes` for retained output.
fn admit_bytes(ledger: &mut dyn SharedLedger, bytes: u64) -> Result<(), MappingError> {
    ledger.try_charge(bytes)?;
    Ok(())
}

/// Byte count as `u64`, refusing rather than fabricating a `u64::MAX` extent.
fn len_of(len: usize) -> Result<u64, MappingError> {
    u64::try_from(len).map_err(|_| MappingError::Internal {
        location: SourceLocation::plan(),
        invariant: "a buffer length is not representable as u64",
    })
}

/// Slot bytes for `count` elements of `T`, refused on overflow.
fn slots_of<T>(count: usize) -> Result<u64, MappingError> {
    let per = core::mem::size_of::<T>();
    u64::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(per as u64))
        .ok_or(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "a slot extent overflows u64",
        })
}

/// Admit `count` new slots **and then** reserve exactly that many, fallibly.
///
/// Charge the requested new slots before fallible reservation. The allocator may return
/// greater capacity; the final checked extent/adoption refuses any unadmitted remainder.
/// No top-up after allocation is permitted.
fn admit_and_reserve<T>(
    vec: &mut Vec<T>,
    additional: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<(), MappingError> {
    if additional == 0 {
        return Ok(());
    }
    admit_bytes(ledger, slots_of::<T>(additional)?)?;
    ledger.try_charge_entries(u64::try_from(additional).map_err(|_| {
        MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "an entry count is not representable as u64",
        }
    })?)?;
    if vec.try_reserve_exact(additional).is_err() {
        return Err(MappingError::Refused(Refusal::Allocation {
            requested: additional.saturating_mul(core::mem::size_of::<T>()),
        }));
    }
    Ok(())
}

/// Copy `bytes` into a fresh admitted buffer: capacity is admitted before the allocation.
fn admitted_copy(ledger: &mut dyn SharedLedger, bytes: &[u8]) -> Result<Vec<u8>, MappingError> {
    admit_bytes(ledger, len_of(bytes.len())?)?;
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve_exact(bytes.len()).is_err() {
        return Err(MappingError::Refused(Refusal::Allocation {
            requested: bytes.len(),
        }));
    }
    out.extend_from_slice(bytes);
    Ok(out)
}

/// A `fmt::Write` that only counts. Allocates nothing, so it runs before admission.
struct CountingFmt {
    bytes: u64,
}

impl core::fmt::Write for CountingFmt {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        let len = u64::try_from(value.len()).map_err(|_| core::fmt::Error)?;
        self.bytes = self.bytes.checked_add(len).ok_or(core::fmt::Error)?;
        Ok(())
    }
}

/// Format into an admitted, exactly-reserved `String` — **never** via `format!`.
///
/// `format_args!` yields borrowed `fmt::Arguments` and allocates nothing. The counting pass then
/// gives the exact byte length; that length is admitted and reserved exactly; and the single
/// real allocation is filled by `write`. This replaces the old helper that accepted an
/// already-allocated `String` and copied it, leaving the original allocation uncharged.
fn admitted_format(
    ledger: &mut dyn SharedLedger,
    args: core::fmt::Arguments<'_>,
) -> Result<String, MappingError> {
    let mut counter = CountingFmt { bytes: 0 };
    if core::fmt::write(&mut counter, args).is_err() {
        return Err(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "the formatting counting pass failed",
        });
    }
    let exact = counter.bytes;
    admit_bytes(ledger, exact)?;
    let exact_usize = usize::try_from(exact).map_err(|_| MappingError::Internal {
        location: SourceLocation::plan(),
        invariant: "the formatted length is not addressable here",
    })?;
    let mut out = String::new();
    if out.try_reserve_exact(exact_usize).is_err() {
        return Err(MappingError::Refused(Refusal::Allocation {
            requested: exact_usize,
        }));
    }
    if core::fmt::write(&mut out, args).is_err() {
        return Err(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "writing the formatted string failed",
        });
    }
    debug_assert_eq!(
        out.len(),
        exact_usize,
        "formatting disagreed with the count"
    );
    Ok(out)
}
/// Clone a string into an admitted buffer.
fn admitted_string(ledger: &mut dyn SharedLedger, value: &str) -> Result<String, MappingError> {
    let bytes = admitted_copy(ledger, value.as_bytes())?;
    String::from_utf8(bytes).map_err(|_| MappingError::Internal {
        location: SourceLocation::plan(),
        invariant: "a source string was not UTF-8",
    })
}

// ---------------------------------------------------------------------------
// The extraction plan as input
// ---------------------------------------------------------------------------

/// One term from the plan.
#[derive(Debug, serde::Deserialize)]
struct PlanTerm {
    source_id: String,
    source_path: String,
    target_id: u128,
    slug: String,
    atom: String,
    layer: u64,
    domain: Vec<String>,
    confidence: String,
    source: String,
    triggers: Vec<String>,
    body: PlanSlice,
    file: PlanFileRef,
}

/// One assertion from the plan.
#[derive(Debug, serde::Deserialize)]
struct PlanAssertion {
    source_id: String,
    /// Retained as source attribution for the package; the record itself carries no path.
    #[allow(dead_code)]
    source_path: String,
    target_id: u128,
    from_slug: String,
    from_target_id: Option<u128>,
    relation: String,
    target_target_id: Option<u128>,
    confidence: String,
    declaration_order: u32,
}

/// One proposal record from the plan.
#[derive(Debug, serde::Deserialize)]
struct PlanProposal {
    source_id: String,
    source_path: String,
    target_id: u128,
    status: String,
    concept: Option<String>,
    payload: PlanFileRef,
    declared_author: Option<String>,
    expected_revision: Option<String>,
}

/// One checkpoint from the plan.
#[derive(Debug, serde::Deserialize)]
struct PlanCheckpoint {
    source_id: String,
    source_path: String,
    target_id: u128,
    concept: String,
    session: u64,
    content: PlanSlice,
}

/// One frontier row from the plan.
#[derive(Debug, serde::Deserialize)]
struct PlanFrontier {
    source_id: String,
    /// Retained as source attribution for the package; the record itself carries no path.
    #[allow(dead_code)]
    source_path: String,
    section: String,
    table: u32,
    #[allow(dead_code)]
    row: u32,
    declaration_order: u32,
    target_id: u128,
    cells: Vec<String>,
    raw: String,
}

/// One attachment: a source file the plan saw.
#[derive(Debug, serde::Deserialize)]
struct PlanAttachment {
    path: String,
    bytes: u64,
    sha256: String,
}

/// A byte slice inside a source blob: offset, exact length, digest of exactly those bytes.
#[derive(Debug, serde::Deserialize)]
struct PlanSlice {
    offset: u64,
    length: u64,
    sha256: String,
}

/// A whole-file blob reference the plan recorded.
#[derive(Debug, serde::Deserialize)]
struct PlanFileRef {
    bytes: u64,
    sha256: String,
    blob: String,
}

/// One unresolved requirement from the (frozen) source extraction plan.
#[derive(Debug, Clone, serde::Deserialize)]
struct PlanUnresolved {
    family: String,
    subject: String,
    field: String,
    reason: String,
    /// Parsed so a malformed plan is still rejected, but **never** used as policy:
    /// `resolve_requirement` applies the authenticated Admin decision instead.
    /// Parsed so a malformed plan is still rejected, but **never** used as policy:
    /// `resolve_requirement` applies the authenticated construction decision instead.
    #[serde(default)]
    #[allow(dead_code)]
    proposed_strategy: Option<String>,
}

/// The plan's own source binding.
#[derive(Debug, serde::Deserialize)]
struct PlanSource {
    archive_dir: String,
    archive_content_digest: String,
    source_digest: String,
    authority: String,
    kb_id: String,
    files: u64,
    bytes: u64,
    extraction: String,
}

/// The extraction plan, as this importer reads it.
#[derive(Debug, serde::Deserialize)]
struct Plan {
    plan_version: String,
    source: PlanSource,
    counts: PlanCounts,
    terms: Vec<PlanTerm>,
    assertions: Vec<PlanAssertion>,
    proposals: Vec<PlanProposal>,
    checkpoints: Vec<PlanCheckpoint>,
    frontier: Vec<PlanFrontier>,
    attachments: Vec<PlanAttachment>,
    unresolved: Vec<PlanUnresolved>,
    errors: Vec<serde_json::Value>,
}

/// The plan's own family counts.
#[derive(Debug, serde::Deserialize)]
struct PlanCounts {
    term: u64,
    assertion: u64,
    proposal: u64,
    checkpoint: u64,
    frontier_entry: u64,
    attachment: u64,
    #[serde(default)]
    source_proposal_files: u64,
}

/// The source archive, read through a caller-supplied reader.
///
/// The reader is an interface rather than a path because the archive may be reached differently
/// by a test and by an operator — and because the mapper must not assume it can open files
/// itself.
pub trait SourceArchive {
    /// Read the blob at a plan-recorded `blobs/<sha256>` path.
    ///
    /// `ledger` is the **same** attempt ledger the mapping is spending against: the reader
    /// admits the actual bytes before it allocates them, so a caller ceiling applies to real
    /// reads rather than to a post-hoc total. The shared ledger is the canonical contract, not
    /// a duplicate adapter — a second accounting path would let the two disagree.
    ///
    /// # Errors
    ///
    /// [`MappingError::Refused`] when the ledger refuses the bytes, [`MappingError::InvalidSource`]
    /// when the blob is missing or its bytes disagree with the plan's recorded length or digest.
    fn blob(
        &mut self,
        ledger: &mut dyn SharedLedger,
        plan_blob_path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<Vec<u8>, MappingError>;
}

/// Parse the plan and check that it is internally consistent with itself.
///
/// Every check here is a *self*-consistency check: counts against lengths, ids against the
/// records that name them. Whether the source is what it claims to be is the archive's
/// digest — bound separately.
fn read_plan(bytes: &[u8]) -> Result<Plan, MappingError> {
    let plan: Plan =
        serde_json::from_slice(bytes).map_err(|error| MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: if error.is_data() {
                "plan is not a well-formed extraction plan"
            } else {
                "plan could not be parsed"
            },
        })?;
    if plan.plan_version != "2" {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "only plan version 2 is accepted by this importer",
        });
    }
    if !plan.errors.is_empty() {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::row(SourceArray::Errors, 0),
            detail: "the plan carries extraction errors; it is not a usable input",
        });
    }
    // Declared counts must match the records actually carried. A plan whose counts disagree with
    // its own arrays is not something a mapping should paper over.
    let actual = [
        (
            SourceArray::Terms,
            plan.terms.len() as u64,
            plan.counts.term,
        ),
        (
            SourceArray::Assertions,
            plan.assertions.len() as u64,
            plan.counts.assertion,
        ),
        (
            SourceArray::Proposals,
            plan.proposals.len() as u64,
            plan.counts.proposal,
        ),
        (
            SourceArray::Checkpoints,
            plan.checkpoints.len() as u64,
            plan.counts.checkpoint,
        ),
        (
            SourceArray::Frontier,
            plan.frontier.len() as u64,
            plan.counts.frontier_entry,
        ),
        (
            SourceArray::Attachments,
            plan.attachments.len() as u64,
            plan.counts.attachment,
        ),
    ];
    for (array, seen, declared) in actual {
        if seen != declared {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::row(array, 0),
                detail: "the plan's declared count disagrees with its own records",
            });
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Preparation
// ---------------------------------------------------------------------------

/// Build one canonical event through the typed bridge.
fn prepare(
    input: EventInput<'_>,
    ledger: &mut dyn SharedLedger,
    location: SourceLocation,
) -> Result<OwnedEvent, MappingError> {
    match typed::prepare_event(input, ledger) {
        FormatOutcome::Ok(event) => Ok(event),
        FormatOutcome::Refused(refusal) => Err(MappingError::Refused(refusal)),
        // The canonical `FormatError` is `Copy`: preserve it rather than describe it.
        FormatOutcome::Format(reason) => Err(MappingError::CanonicalRejected { location, reason }),
    }
}

/// Hash bytes with the canonical digest.
fn digest_of(bytes: &[u8]) -> [u8; 32] {
    sage_db_format::digest::raw_sha256(bytes)
}

/// Look up a term id in the sorted borrowed (slug, id) pairs.
fn lookup_term(pairs: &[(&str, u128)], slug: &str) -> Option<u128> {
    pairs
        .binary_search_by(|(candidate, _)| (*candidate).cmp(slug))
        .ok()
        .map(|index| pairs[index].1)
}

/// The plan's `blobs/<sha256>` path, built into an admitted buffer.
///
/// The digest comes from the plan (untrusted, arbitrary length), so this is a real transient
/// allocation and is admitted like any other — it is not a fixed stack extent.
fn blob_path(
    ledger: &mut dyn SharedLedger,
    prefix: &str,
    digest: &str,
) -> Result<String, MappingError> {
    admitted_format(ledger, format_args!("{prefix}{digest}"))
}

/// Compare a plan-recorded blob path against `blobs/<digest>` **without allocating**.
fn blob_path_matches(recorded: &str, prefix: &str, digest: &str) -> bool {
    recorded
        .strip_prefix(prefix)
        .is_some_and(|rest| rest == digest)
}

/// Lowercase hex of a digest: a fixed 64-byte stack value, no allocation.
fn hex(bytes: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 64];
    for (index, byte) in bytes.iter().enumerate() {
        buf[index * 2] = DIGITS[usize::from(byte >> 4)];
        buf[index * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    buf
}

/// The same digest hex as a borrowed `&str` over a caller-provided stack buffer.
fn hex_str(buf: &[u8; 64]) -> &str {
    core::str::from_utf8(buf).expect("hex output is ASCII")
}

/// Push one event, admitting the retained slot and the buffer it owns first.
fn push_event(
    events: &mut Vec<OwnedEvent>,
    ledger: &mut dyn SharedLedger,
    event: OwnedEvent,
) -> Result<(), MappingError> {
    // prepare_event already admitted the field table and every owned field value.
    // Moving that event into this collection allocates only a new event slot.
    admit_and_reserve(events, 1, ledger)?;
    events.push(event);
    Ok(())
}

/// Push one blob declaration, admitting its retained size first.
fn push_blob(
    blobs: &mut Vec<BlobDeclaration>,
    ledger: &mut dyn SharedLedger,
    blob: BlobDeclaration,
) -> Result<(), MappingError> {
    // The payload was admitted where it was allocated (or is an existing reference).
    // This move needs only the declaration slot, not another payload admission.
    admit_and_reserve(blobs, 1, ledger)?;
    blobs.push(blob);
    Ok(())
}

/// The COMPLETE retained extent of a preparation, in bytes.
///
/// Measured from the values actually returned, never estimated from a subset. `Vec` slot
/// **capacity** is used rather than length: a vector that grew to 8 slots and holds 5 entries
/// retains 8, and counting lengths is exactly how the previous version understated its own
/// output. No `to_vec()`/`clone()` appears here — measuring must not allocate.
struct OutputExtent<'a> {
    events: &'a Vec<OwnedEvent>,
    blobs: &'a Vec<BlobDeclaration>,
    package: &'a ImportPackage,
    session: &'a ImportSessionRecord,
    counts: &'a Vec<CountPair>,
    mapping: &'a PathMapping,
    dispositions: &'a Vec<Disposition>,
    watermarks: &'a Vec<PackageWatermark>,
}

fn complete_output_extent(input: OutputExtent<'_>) -> Result<u64, MappingError> {
    let OutputExtent {
        events,
        blobs,
        package,
        session,
        counts,
        mapping,
        dispositions,
        watermarks,
    } = input;
    /// A refused extent: the measurement must never fabricate a value.
    fn overflow() -> MappingError {
        MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "the retained extent overflows u64",
        }
    }
    /// Slot bytes retained by one vector of `T`.
    fn slots<T>(vec: &Vec<T>) -> Result<u64, MappingError> {
        let count = u64::try_from(vec.capacity()).map_err(|_| overflow())?;
        count
            .checked_mul(core::mem::size_of::<T>() as u64)
            .ok_or_else(overflow)
    }
    /// A string's retained heap bytes (capacity, not length).
    fn text(value: &String) -> Result<u64, MappingError> {
        u64::try_from(value.capacity()).map_err(|_| overflow())
    }
    /// Retained extent of one identity vector: slot capacity plus each source string.
    fn entries(vec: &Vec<MappingEntry>) -> Result<u64, MappingError> {
        let mut total = slots(vec)?;
        for entry in vec {
            total = total
                .checked_add(text(&entry.source)?)
                .ok_or_else(overflow)?;
        }
        Ok(total)
    }

    let mut total = slots(events)?;
    for event in events {
        total = total
            .checked_add(slots(&event.fields)?)
            .ok_or_else(overflow)?;
        for (_, value) in &event.fields {
            total = total.checked_add(slots(value)?).ok_or_else(overflow)?;
        }
    }
    total = total.checked_add(slots(blobs)?).ok_or_else(overflow)?;
    for blob in blobs {
        if let BlobSource::Provide(payload) = &blob.source {
            total = total.checked_add(slots(payload)?).ok_or_else(overflow)?;
        }
    }
    // The package payload lives in exactly one of those `Provide` blobs. Assert it is present
    // rather than silently counting zero: `_package` is the proof obligation, not an omission.
    if !blobs.iter().any(|blob| blob.sha256 == package.sha256) {
        return Err(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "the package payload has no owning blob",
        });
    }
    let _ = package;
    total = total
        .checked_add(slots(&session.expected_counts)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(text(&session.import_session_id)?)
        .ok_or_else(overflow)?;
    if let Some(reason) = &session.audit.reason {
        total = total.checked_add(text(reason)?).ok_or_else(overflow)?;
    }
    if let Some(actor) = &session.audit.recorded_by {
        total = total.checked_add(text(actor)?).ok_or_else(overflow)?;
    }
    total = total.checked_add(slots(counts)?).ok_or_else(overflow)?;
    total = total.checked_add(slots(watermarks)?).ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.term)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.assertion)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.proposal)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.frontier)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.checkpoint)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(entries(&mapping.attachment)?)
        .ok_or_else(overflow)?;
    total = total
        .checked_add(slots(&mapping.attachment_target)?)
        .ok_or_else(overflow)?;
    for path in &mapping.attachment_target {
        total = total.checked_add(text(path)?).ok_or_else(overflow)?;
    }
    total = total
        .checked_add(slots(dispositions)?)
        .ok_or_else(overflow)?;
    for disposition in dispositions {
        for value in [
            &disposition.requirement,
            &disposition.subject,
            &disposition.policy,
            &disposition.constructed,
            &disposition.reason,
        ] {
            total = total.checked_add(text(value)?).ok_or_else(overflow)?;
        }
    }
    Ok(total)
}

/// Admit a list of strings and copy each one into an admitted buffer.
///
/// The vector's slots are admitted in one step before it grows; each string's own capacity is
/// admitted by `admitted_string` before that string is allocated.
fn admitted_string_list(
    ledger: &mut dyn SharedLedger,
    values: &[String],
) -> Result<Vec<String>, MappingError> {
    let mut out: Vec<String> = Vec::new();
    admit_and_reserve(&mut out, values.len(), ledger)?;
    for value in values {
        out.push(admitted_string(ledger, value)?);
    }
    Ok(out)
}

/// The `kb.json` fields the metadata singleton needs.
struct SourceKbMetadata {
    id: String,
    name: String,
    description: String,
    domains: Vec<String>,
}

/// Read `kb.json` from the source package and validate it.
///
/// The raw file is provenance; this is the **current singleton update** the task requires, so
/// the name/description/domains are mapped rather than merely retained.
fn read_kb_metadata(
    files: &[PackageFile],
    ledger: &mut dyn SharedLedger,
) -> Result<SourceKbMetadata, MappingError> {
    let file = files
        .iter()
        .find(|file| file.path == "kb.json")
        .ok_or_else(|| MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the source archive carries no kb.json",
        })?;
    #[derive(serde::Deserialize)]
    struct Raw {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        domains: Vec<String>,
    }
    let raw: Raw =
        serde_json::from_slice(&file.bytes).map_err(|_| MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "kb.json is not a well-formed KB descriptor",
        })?;
    Ok(SourceKbMetadata {
        id: admitted_string(ledger, &raw.id)?,
        name: admitted_string(ledger, &raw.name)?,
        description: admitted_string(ledger, &raw.description)?,
        domains: admitted_string_list(ledger, &raw.domains)?,
    })
}

/// The `changed_fields` tags a metadata update carries over Genesis.
const KB_METADATA_CHANGED_TAGS: [u16; 5] = [2, 3, 4, 5, 6];

/// Project every event with the canonical projector and count the whole imported state.
///
/// This is projector **agreement**, not an independent golden: it proves the mapping's own
/// recorded counts equal what the canonical projector sees, including the two Genesis
/// singletons that this mapping does not construct itself.
fn project_whole_state(
    events: &[OwnedEvent],
    ledger: &mut dyn SharedLedger,
) -> Result<ProjectedCounts, MappingError> {
    let mut entries: Vec<sage_db_types::persist::StateEntry> = Vec::new();
    for event in events {
        let mut fields: Vec<sage_db_format::event::Field<'_>> = Vec::new();
        admit_and_reserve(&mut fields, event.fields.len(), ledger)?;
        for (tag, value) in &event.fields {
            fields.push(sage_db_format::event::Field::new(*tag, value));
        }
        match sage_db_format::projection::project_event(event.tag, &fields, &mut entries, ledger) {
            FormatOutcome::Ok(_) => {}
            FormatOutcome::Refused(refusal) => return Err(MappingError::Refused(refusal)),
            FormatOutcome::Format(reason) => {
                return Err(MappingError::CanonicalRejected {
                    location: SourceLocation::plan(),
                    reason,
                });
            }
        }
    }
    // MERGE full identities: two events for one record (Pending@1, Decided@2) are ONE row in
    // the imported current state. Counting entries would report 26 proposal rows for 13
    // proposals — a count of events, not of state.
    //
    // Sorted indices over BORROWED keys: no `BTreeMap`, no `entry.key.clone()`, one admitted
    // `Vec<usize>` of index slots. `entries` is owned by this function, so no per-event
    // allocation is added here.
    let mut order: Vec<usize> = Vec::new();
    admit_and_reserve(&mut order, entries.len(), ledger)?;
    for index in 0..entries.len() {
        order.push(index);
    }
    // Total order: kind, key, revision — so one pass both groups identities and checks
    // revision ordering inside each group without a map.
    order.sort_unstable_by(|left, right| {
        let a = &entries[*left];
        let b = &entries[*right];
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.revision.cmp(&b.revision))
    });

    let mut counts = ProjectedCounts::new();
    let mut current: Option<(u16, &[u8], u64)> = None;
    for index in &order {
        let entry = &entries[*index];
        if let Some((kind, key, revision)) = current
            && kind == entry.kind
            && key == entry.key.as_slice()
        {
            if revision == entry.revision {
                return Err(MappingError::Internal {
                    location: SourceLocation::plan(),
                    invariant: "two events produced the same revision of one identity",
                });
            }
            current = Some((kind, key, entry.revision));
            continue;
        }
        let Some(state_kind) = StateKind::from_u16(entry.kind) else {
            return Err(MappingError::Internal {
                location: SourceLocation::plan(),
                invariant: "the projector produced an unknown state kind",
            });
        };
        counts.increment(state_kind)?;
        current = Some((entry.kind, entry.key.as_slice(), entry.revision));
    }
    Ok(counts)
}

/// A state kind's stable name, for messages.
fn kind_name(kind: StateKind) -> &'static str {
    match kind {
        StateKind::Term => "term",
        StateKind::Assertion => "assertion",
        StateKind::Claim => "claim",
        StateKind::Evidence => "evidence",
        StateKind::EvidenceLink => "evidence-link",
        StateKind::Vector => "vector",
        StateKind::Proposal => "proposal",
        StateKind::FrontierEntry => "frontier-entry",
        StateKind::SessionCheckpoint => "session-checkpoint",
        StateKind::Asset => "asset",
        StateKind::Model => "model",
        StateKind::KbMetadata => "kb-metadata",
        StateKind::RuleConfig => "rule-config",
    }
}

/// A state kind's persisted number.
fn kind_number(kind: StateKind) -> u16 {
    match kind {
        StateKind::Term => 1,
        StateKind::Assertion => 2,
        StateKind::Claim => 3,
        StateKind::Evidence => 4,
        StateKind::EvidenceLink => 5,
        StateKind::Vector => 6,
        StateKind::Proposal => 7,
        StateKind::FrontierEntry => 8,
        StateKind::SessionCheckpoint => 9,
        StateKind::Asset => 10,
        StateKind::Model => 11,
        StateKind::KbMetadata => 12,
        StateKind::RuleConfig => 13,
    }
}

/// Lowercase hex of a 16-byte KB identity: a fixed 32-byte stack value.
fn hex16(bytes: &sage_db_types::identity::KbUuid) -> [u8; 32] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 32];
    for (index, byte) in bytes.as_bytes().iter().enumerate() {
        buf[index * 2] = DIGITS[usize::from(byte >> 4)];
        buf[index * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    buf
}

/// Resolve one frozen requirement by its OWN policy, naming the actual constructed value.
///
/// The previous version copied `proposed_strategy` out of the untrusted plan and said
/// `unavailable` for everything to a subset of fields. Neither is a disposition: the policy
/// must be the authenticated Admin decision, and the constructed value must be the value this
/// construction actually sets.
fn resolve_requirement(
    requirement: &PlanUnresolved,
    principal: &str,
    context: &ExecutorContext,
    plan: &Plan,
    ledger: &mut dyn SharedLedger,
) -> Result<Disposition, MappingError> {
    // Assembled through `format_args!` into an admitted buffer: no `format!`, no intermediate.
    let requirement_name = admitted_format(
        ledger,
        format_args!("{}.{}", requirement.family, requirement.field),
    )?;
    // Match the ACTUAL field spellings the frozen plan uses, not a guess. Each arm names the
    // value this construction really sets.
    let (policy, constructed, reason): (&'static str, String, &'static str) = match (
        requirement.family.as_str(),
        requirement.field.as_str(),
    ) {
        // ---- audit timestamps: NEW construction facts, never recovered source history ----
        ("term" | "assertion", "created_at" | "term.created_at" | "assertion.created_at") => (
            "the target stamps its own import event time; the source's creation time stays unavailable",
            admitted_format(
                ledger,
                format_args!("created_at={} (import event time)", context.import_time),
            )?,
            "the source never recorded when the document was written",
        ),
        // ---- actor: the authenticated Admin performing the import ----
        ("term", "recorded_by/actor_origin" | "term.recorded_by/actor_origin") => (
            "the imported record's actor is the authenticated Admin running the import",
            admitted_format(
                ledger,
                format_args!("recorded_by={principal}, actor_origin=authenticated"),
            )?,
            "source authorship was never archived; naming the real importer is the honest actor",
        ),
        // ---- revisions: revision 1 is the target's fact; no source revision invented ----
        ("term", "revision" | "term.revision") => (
            "each imported term is created at revision 1; the source revision stays unavailable",
            admitted_string(ledger, "revision=1, legacy_source_revision=None")?,
            "no source revision was ever archived for these files",
        ),
        // ---- frontier rows: no id or status domain existed in the source Markdown ----
        ("frontier_entry", "id/status" | "frontier_entry.id/status") => (
            "imported activation-boundary rows get target ids and Open status; the source had neither",
            admitted_format(
                ledger,
                format_args!(
                    "{} row(s) assigned target ids; status=Open",
                    plan.frontier.len()
                ),
            )?,
            "the source Markdown row had no id or status column to preserve",
        ),
        // ---- proposals: explicit NEW restatement, not a recovered review timeline ----
        ("proposal", "historical_restatement" | "proposal.historical_restatement") => (
            "each historically approved proposal is restated as NEW Pending@1 then Decided@2 with import provenance",
            admitted_format(
                ledger,
                format_args!(
                    "{} proposal(s) restated; original reviewer and review time recorded as never archived",
                    plan.proposals.len()
                ),
            )?,
            "replaying the old approval would forge a review that this import did not perform",
        ),
        // ---- attachments: files stay package members, no synthetic Evidence/Asset ----
        ("attachment", _) => (
            "source files stay package members; no native Evidence or Asset row is manufactured",
            admitted_format(
                ledger,
                format_args!("{} package file(s), 0 native rows", plan.attachments.len()),
            )?,
            "Evidence is not a path alias and MediaType is image-only",
        ),
        (_, _) => {
            return Err(MappingError::Unsupported {
                location: SourceLocation::plan(),
                detail: "this source requirement has no authenticated construction policy in this batch",
            });
        }
    };
    Ok(Disposition {
        requirement: requirement_name,
        subject: admitted_string(ledger, &requirement.subject)?,
        policy: admitted_string(ledger, policy)?,
        constructed,
        reason: admitted_string(ledger, reason)?,
    })
}

/// Map a whole extraction plan plus its source archive into a prepared import.
///
/// # Resource boundary
///
/// The caller's `max_plan_bytes` ceiling and one ledger checkpoint are applied **before** the
/// plan is parsed: a 200 MiB plan cannot make this function allocate first and refuse later.
/// The bounded JSON decode is the single named allocator boundary — `serde_json` allocates
/// internally, so it is byte-gated first and its output is never trusted for sizes. Every
/// retained buffer downstream is admitted through the same `SharedLedger` and grown fallibly.
///
/// # Errors
///
/// [`MappingError`] — a resource refusal, an unusable plan, an unsupported source object, or a
/// canonical writer rejecting a constructed record. There is no partial success: the result is
/// returned only if **every** family mapped.
pub fn prepare_native_mapping<L>(
    plan_bytes: &[u8],
    archive: &mut dyn SourceArchive,
    grants: &sage_db_kernel::auth::Grants<'_>,
    target_kb: sage_db_types::identity::KbUuid,
    context: &ExecutorContext,
    limits: MappingLimits,
    ledger: &mut L,
) -> Result<PreparedImport, MappingError>
where
    L: sage_db_store::lease::LeaseLedger,
{
    // --- Finding 1: the ceiling and the checkpoint come FIRST ----------------
    let plan_len = len_of(plan_bytes.len())?;
    if plan_len > limits.max_plan_bytes {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the plan exceeds the caller's plan-byte ceiling",
        });
    }
    ledger.checkpoint()?;

    // --- Finding 5: real authorization, not a spelled principal -------------
    // `prepare` is an administrative construction: it needs `Admin` on the TARGET KB. Holding
    // a `Grants` value is not permission; this call is the permission check, and an empty
    // principal, a cross-KB grant or a missing capability refuses here — before any read.
    let authorized = grants
        .authorize(target_kb, sage_db_kernel::auth::Capability::Admin, ledger)
        .map_err(|error| match error {
            sage_db_kernel::auth::AuthError::Refused(refusal) => MappingError::Refused(refusal),
            other => MappingError::Unauthorized {
                detail: match other {
                    sage_db_kernel::auth::AuthError::Unauthenticated => {
                        "the import principal is not authenticated"
                    }
                    sage_db_kernel::auth::AuthError::WrongKb { .. } => {
                        "the grant is scoped to a different KB than the import target"
                    }
                    sage_db_kernel::auth::AuthError::MissingCapability { .. } => {
                        "the principal lacks the Admin capability for this import"
                    }
                    sage_db_kernel::auth::AuthError::Refused(_) => unreachable!("matched above"),
                },
            },
        })?;
    let principal = authorized.principal();

    // Only now is the input decoded: authorization precedes reading, so an unauthorized caller
    // learns nothing about the plan's shape. This is the ONE named allocator boundary.
    let plan = read_plan(plan_bytes)?;

    // Caller ceilings are applied to the INPUT before anything is copied or built.
    let file_count = len_of(plan.attachments.len())?;
    if file_count > limits.max_files {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the plan carries more files than the caller's ceiling",
        });
    }
    // Finding 1: checked addition, not an unchecked sum.
    let mut total_bytes: u64 = 0;
    for (row, attachment) in plan.attachments.iter().enumerate() {
        total_bytes =
            total_bytes
                .checked_add(attachment.bytes)
                .ok_or(MappingError::InvalidSource {
                    location: SourceLocation::row(SourceArray::Attachments, row)
                        .with_field("bytes"),
                    detail: "the plan's file sizes overflow a u64",
                })?;
    }
    if total_bytes > limits.max_bytes {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the plan's files exceed the caller's byte ceiling",
        });
    }

    // Finding 2: the supplied digest is decoded, never hashed again.
    let source_digest = decode_digest(
        &plan.source.source_digest,
        SourceLocation::source("source_digest"),
    )?;
    let archive_digest = decode_digest(
        &plan.source.archive_content_digest,
        SourceLocation::source("archive_content_digest"),
    )?;

    let mut mapping = PathMapping::default();
    let mut dispositions: Vec<Disposition> = Vec::new();
    let mut events: Vec<OwnedEvent> = Vec::new();
    let mut blobs: Vec<BlobDeclaration> = Vec::new();
    let mut counts = ProjectedCounts::new();

    // --- the package: every source file's actual bytes -----------------------
    let mut package_files: Vec<PackageFile> = Vec::new();
    for attachment in &plan.attachments {
        admit_and_reserve(&mut package_files, 1, ledger)?;
        let path = blob_path(ledger, "blobs/", &attachment.sha256)?;
        let bytes = archive.blob(ledger, &path, attachment.bytes, &attachment.sha256)?;
        drop(path);
        package_files.push(PackageFile {
            path: admitted_string(ledger, &attachment.path)?,
            length: attachment.bytes,
            sha256: admitted_string(ledger, &attachment.sha256)?,
            bytes,
        });
        // An attachment has no numeric target id: the package entry IS its target identity.
        PathMapping::push(&mut mapping.attachment, &attachment.path, 0, ledger)?;
    }

    // --- Finding 7: the KB metadata singleton, from the actual kb.json -------
    let kb_metadata = read_kb_metadata(&package_files, ledger)?;
    let mut legacy_frontier_ref = None;
    for file in &package_files {
        ledger.try_charge_entries(1)?;
        ledger.checkpoint()?;
        if file.path == "frontier.md" {
            let digest = decode_digest(&file.sha256, SourceLocation::file("frontier.md"))?;
            let blob = BlobDeclaration {
                sha256: digest,
                declared_len: file.length,
                source: BlobSource::Provide(admitted_copy(ledger, &file.bytes)?),
            };
            push_blob(&mut blobs, ledger, blob)?;
            legacy_frontier_ref = Some(digest);
            break;
        }
    }
    let metadata_record = KbMetadataRecord {
        revision: RecordRevision::new(2).expect("2 is a valid revision"),
        kb_uuid: target_kb,
        slug: admitted_string(ledger, &kb_metadata.id)?,
        name: admitted_string(ledger, &kb_metadata.name)?,
        description: admitted_string(ledger, &kb_metadata.description)?,
        domains: admitted_string_list(ledger, &kb_metadata.domains)?,
        legacy_frontier_ref,
        audit: OptionalActorAudit {
            actor: Some(ActorOrigin::Authenticated),
            recorded_by: Some(admitted_string(ledger, principal)?),
            reason: Some(admitted_string(ledger, &context.reason)?),
            created_at: context.import_time,
            updated_at: Some(context.import_time),
        },
        extensions: Extensions::new(),
        // Genesis established revision 1; this update carries the changed tags.
        changed_fields: {
            let mut tags: Vec<u16> = Vec::new();
            admit_and_reserve(
                &mut tags,
                KB_METADATA_CHANGED_TAGS.len() + usize::from(legacy_frontier_ref.is_some()),
                ledger,
            )?;
            tags.extend_from_slice(&KB_METADATA_CHANGED_TAGS);
            if legacy_frontier_ref.is_some() {
                tags.push(153);
            }
            Some(tags)
        },
    };
    let event = prepare(
        EventInput::KbMetadata(&metadata_record),
        ledger,
        SourceLocation::file("kb.json"),
    )?;
    push_event(&mut events, ledger, event)?;
    counts.increment(StateKind::KbMetadata)?;

    // --- terms ---------------------------------------------------------------
    // Borrowed (slug -> id) pairs in an admitted vector, sorted for lookup: no `BTreeMap`
    // (unmeasurable capacity, infallible insert) and no `slug.clone()`.
    let mut term_ids: Vec<(&str, u128)> = Vec::new();
    admit_and_reserve(&mut term_ids, plan.terms.len(), ledger)?;
    for (row, term) in plan.terms.iter().enumerate() {
        term_ids.push((term.slug.as_str(), term.target_id));
        if !blob_path_matches(&term.file.blob, "blobs/", &term.file.sha256) {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "the plan's blob path is not its own digest",
            });
        }
        let source_file =
            archive.blob(ledger, &term.file.blob, term.file.bytes, &term.file.sha256)?;
        // The BODY is the exact slice, never the trimmed projection.
        let offset =
            usize::try_from(term.body.offset).map_err(|_| MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "body offset is not representable",
            })?;
        let length =
            usize::try_from(term.body.length).map_err(|_| MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "body length is not representable",
            })?;
        let end = offset
            .checked_add(length)
            .ok_or(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "body slice overflows",
            })?;
        let body = source_file
            .get(offset..end)
            .ok_or(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "body slice is outside the source file",
            })?;
        verify_digest(
            body,
            &term.body.sha256,
            SourceLocation::row(SourceArray::Terms, row).with_field("body.sha256"),
            "body slice digest disagrees with the plan",
        )?;
        let body_ref = digest_of(body);
        let blob = BlobDeclaration {
            sha256: body_ref,
            declared_len: len_of(body.len())?,
            source: BlobSource::Provide(admitted_copy(ledger, body)?),
        };
        push_blob(&mut blobs, ledger, blob)?;

        let record = TermRecord {
            key: RecordKey::new(RecordKind::Term, term.target_id),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: admitted_string(ledger, &term.slug)?,
            atom: admitted_string(ledger, &term.atom)?,
            layer: layer_of(term.layer).map_err(|detail| MappingError::InvalidSource {
                location: SourceLocation::row(SourceArray::Terms, row),
                detail,
            })?,
            domains: admitted_string_list(ledger, &term.domain)?,
            source: term_source_of(&term.source).map_err(|detail| MappingError::InvalidSource {
                location: SourceLocation::row(SourceArray::Terms, row),
                detail,
            })?,
            confidence: confidence_of(&term.confidence).map_err(|detail| {
                MappingError::InvalidSource {
                    location: SourceLocation::row(SourceArray::Terms, row),
                    detail,
                }
            })?,
            triggers: admitted_string_list(ledger, &term.triggers)?,
            body_ref,
            common: import_audit(
                ledger,
                principal,
                context,
                &term.slug,
                SourceLocation::row(SourceArray::Terms, row),
            )?,
            legacy_id: Some(admitted_string(ledger, &term.source_id)?),
            legacy_source_path: Some(admitted_string(ledger, &term.source_path)?),
            legacy_content_digest: Some(digest_of(body)),
            legacy_source_file_digest: Some(digest_of(&source_file)),
            legacy_source_revision: None,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        let event = prepare(
            EventInput::Term(TermEvent::Created, &record),
            ledger,
            SourceLocation::row(SourceArray::Terms, row),
        )?;
        push_event(&mut events, ledger, event)?;
        PathMapping::push(&mut mapping.term, &term.slug, term.target_id, ledger)?;
        counts.increment(StateKind::Term)?;
    }

    // Total order over the borrowed keys, then a duplicate scan: a repeated slug is ambiguous
    // identity and is refused rather than resolved first-wins.
    term_ids.sort_unstable_by_key(|(slug, _)| *slug);
    for window in term_ids.windows(2) {
        if window[0].0 == window[1].0 {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "duplicate concept slug in the plan",
            });
        }
    }

    // --- assertions ----------------------------------------------------------
    for (row, assertion) in plan.assertions.iter().enumerate() {
        let from = lookup_term(&term_ids, &assertion.from_slug).ok_or_else(|| {
            MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "assertion names a subject that is not in this source",
            }
        })?;
        // A target outside the KB is a cross-KB edge: the plan flagged it, and it cannot bind.
        let target = assertion
            .target_target_id
            .ok_or_else(|| MappingError::Unsupported {
                location: SourceLocation::plan(),
                detail: "assertion target does not resolve inside this KB",
            })?;
        if Some(from) != assertion.from_target_id {
            return Err(MappingError::InvalidSource {
                location: SourceLocation::plan(),
                detail: "assertion subject id disagrees with the term table",
            });
        }
        let record = AssertionRecordFull {
            key: RecordKey::new(RecordKind::Assertion, assertion.target_id),
            revision: RecordRevision::FIRST,
            retracted: false,
            from: RecordKey::new(RecordKind::Term, from),
            relation: relation_of(&assertion.relation).map_err(|detail| {
                MappingError::InvalidSource {
                    location: SourceLocation::row(SourceArray::Assertions, row),
                    detail,
                }
            })?,
            target: RecordKey::new(RecordKind::Term, target),
            confidence: confidence_of(&assertion.confidence).map_err(|detail| {
                MappingError::InvalidSource {
                    location: SourceLocation::row(SourceArray::Assertions, row),
                    detail,
                }
            })?,
            declaration_order: assertion.declaration_order,
            common: import_audit(
                ledger,
                principal,
                context,
                &assertion.source_id,
                SourceLocation::row(SourceArray::Assertions, row),
            )?,
            legacy_id: Some(admitted_string(ledger, &assertion.source_id)?),
            legacy_content_digest: None,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        let event = prepare(
            EventInput::Assertion(AssertionEvent::Created, &record),
            ledger,
            SourceLocation::row(SourceArray::Assertions, row),
        )?;
        push_event(&mut events, ledger, event)?;
        PathMapping::push(
            &mut mapping.assertion,
            &assertion.source_id,
            assertion.target_id,
            ledger,
        )?;
        counts.increment(StateKind::Assertion)?;
    }

    // --- historical proposals: NEW lifecycle restatement, never re-applied ----
    // `source_proposal_files` counts the plan's FILE inventory; `proposals` counts records.
    // The difference is exactly the documentation examples, which stay package members.
    let examples = plan
        .counts
        .source_proposal_files
        .saturating_sub(len_of(plan.proposals.len())?);
    if examples > 0 {
        // Every retained string admitted as it is created: no `format!`, no `.into()` copy.
        let constructed = admitted_format(
            ledger,
            format_args!("{examples} example file(s) stay attachments; 0 Proposal ids"),
        )?;
        admit_and_reserve(&mut dispositions, 1, ledger)?;
        dispositions.push(Disposition {
            requirement: admitted_string(ledger, "proposal.example")?,
            subject: admitted_format(ledger, format_args!("{}", plan.proposals.len()))?,
            policy: admitted_string(
                ledger,
                "examples remain source-package members, never reviewable Proposal records",
            )?,
            constructed,
            reason: admitted_string(
                ledger,
                "a documented example is not a review that ever happened",
            )?,
        });
    }
    for (row, proposal) in plan.proposals.iter().enumerate() {
        // A source `example` is documentation inside the package, not a Proposal record.
        if proposal.status == "example" {
            continue;
        }
        if proposal.status != "approved" {
            return Err(MappingError::Unsupported {
                location: SourceLocation::plan(),
                detail: "a non-approved source proposal has no canonical import shape in this batch",
            });
        }
        let payload_path = blob_path(ledger, "blobs/", &proposal.payload.sha256)?;
        let payload = archive.blob(
            ledger,
            &payload_path,
            proposal.payload.bytes,
            &proposal.payload.sha256,
        )?;
        drop(payload_path);
        let payload_ref = digest_of(&payload);
        // Read the authored triple while the bytes are still here: the subject and kind both
        // come from this payload, and the same bytes then become the record's blob.
        let edge = if proposal.concept.is_none() {
            Some(read_edge_payload(
                &payload,
                SourceLocation::row(SourceArray::Proposals, row),
            )?)
        } else {
            None
        };
        let blob = BlobDeclaration {
            sha256: payload_ref,
            declared_len: len_of(payload.len())?,
            source: BlobSource::Provide(admitted_copy(ledger, &payload)?),
        };
        push_blob(&mut blobs, ledger, blob)?;
        // Where the subject comes from, per canonical rule (`data-model` §5): a RELATION
        // proposal's `subject` must equal its payload's `from`. The plan's `concept` field is
        // null for an edge file because that file states `from`/`type`/`target` instead — the
        // absence is in the plan's projection, not in the source.
        let subject = match proposal.concept.as_ref() {
            Some(slug) => {
                lookup_term(&term_ids, slug).ok_or_else(|| MappingError::InvalidSource {
                    location: SourceLocation::plan(),
                    detail: "proposal names a concept that is not in this source",
                })?
            }
            None => {
                let edge = edge.as_ref().expect("an edge payload was parsed above");
                // Finding 9: BOTH endpoints must be Terms known to this import.
                lookup_term(&term_ids, edge.from).ok_or_else(|| MappingError::InvalidSource {
                    location: SourceLocation::plan(),
                    detail: "an edge proposal names a `from` concept that is not in this source",
                })?
            }
        };
        let subject_key = RecordKey::new(RecordKind::Term, subject);
        // Finding 9: the baseline names BOTH endpoints, sorted and unique. A self-edge is one
        // identity, which is a construction fact; the source's own direction is untouched.
        let mut baseline: Vec<RevPair> = Vec::new();
        admit_and_reserve(&mut baseline, 2, ledger)?;
        // At most two endpoints; keep the temporary on the stack.
        let mut endpoints = [subject; 2];
        let mut endpoint_count = 1;
        if let Some(edge) = edge.as_ref() {
            let target =
                lookup_term(&term_ids, edge.target).ok_or_else(|| MappingError::InvalidSource {
                    location: SourceLocation::plan(),
                    detail: "an edge proposal names a `target` concept that is not in this source",
                })?;
            if target != subject {
                endpoints[1] = target;
                endpoint_count = 2;
            }
        }
        endpoints[..endpoint_count].sort_unstable();
        for &id in &endpoints[..endpoint_count] {
            baseline.push(RevPair {
                key: RecordKey::new(RecordKind::Term, id),
                revision: RecordRevision::FIRST,
            });
        }
        let proposal_kind = if proposal.concept.is_none() {
            ProposalKind::RelationAssert
        } else {
            ProposalKind::ConceptMarkdown
        };
        let restatement_note = admitted_format(
            ledger,
            format_args!(
                "NEW import restatement of a historically approved source proposal; the original \
                 reviewer and review time were never archived; source baseline {}",
                proposal
                    .expected_revision
                    .as_deref()
                    .unwrap_or("unavailable")
            ),
        )?;
        for (revision, status) in [
            (RecordRevision::FIRST, ProposalStatus::Pending),
            (
                RecordRevision::new(2).expect("2 is a valid revision"),
                ProposalStatus::Approved,
            ),
        ] {
            // Each lifecycle image owns its own baseline and note. Admit both owners
            // before copying; credit for the first image cannot cover the second.
            let mut base_revisions = Vec::new();
            admit_and_reserve(&mut base_revisions, baseline.len(), ledger)?;
            base_revisions.extend_from_slice(&baseline);
            let review_note = admitted_string(ledger, &restatement_note)?;
            let record = ProposalRecord {
                key: RecordKey::new(RecordKind::Proposal, proposal.target_id),
                revision,
                status,
                kind: proposal_kind,
                subject: subject_key,
                base_revisions,
                payload_ref,
                review_note: Some(review_note),
                declared_author: admitted_string(
                    ledger,
                    proposal.declared_author.as_deref().unwrap_or_default(),
                )?,
                common: {
                    let mut audit = import_audit(
                        ledger,
                        principal,
                        context,
                        &proposal.source_id,
                        SourceLocation::row(SourceArray::Proposals, row),
                    )?;
                    // The decided restatement is revision 2; a later revision must stamp its
                    // update time, while the first must not carry one.
                    if revision != RecordRevision::FIRST {
                        audit.updated_at = Some(context.import_time);
                    }
                    audit
                },
                legacy_id: Some(admitted_string(ledger, &proposal.source_id)?),
                extensions: Extensions::new(),
                changed_fields: None,
            };
            let decided = revision != RecordRevision::FIRST;
            let event = prepare(
                EventInput::Proposal {
                    decided,
                    record: &record,
                },
                ledger,
                SourceLocation::row(SourceArray::Proposals, row),
            )?;
            push_event(&mut events, ledger, event)?;
        }
        PathMapping::push(
            &mut mapping.proposal,
            &proposal.source_path,
            proposal.target_id,
            ledger,
        )?;
        counts.increment(StateKind::Proposal)?;
    }

    // --- session checkpoints -------------------------------------------------
    for (row, checkpoint) in plan.checkpoints.iter().enumerate() {
        let concept = lookup_term(&term_ids, &checkpoint.concept);
        let location = match (concept, u32::try_from(checkpoint.session)) {
            (Some(concept), Ok(session_no)) => sage_db_types::persist::CheckpointLocation::Term {
                concept: RecordKey::new(RecordKind::Term, concept),
                session_no,
            },
            _ if sage_db_types::persist::CheckpointLocation::valid_public_parts(
                &checkpoint.concept,
                checkpoint.session,
            ) =>
            {
                sage_db_types::persist::CheckpointLocation::Slug {
                    slug: admitted_string(ledger, &checkpoint.concept)?,
                    session_no: checkpoint.session,
                }
            }
            _ => {
                return Err(MappingError::InvalidSource {
                    location: SourceLocation::row(SourceArray::Checkpoints, row),
                    detail: "invalid public checkpoint slot",
                });
            }
        };
        let content_path = blob_path(ledger, "blobs/", &checkpoint.content.sha256)?;
        let content = archive.blob(
            ledger,
            &content_path,
            checkpoint.content.length,
            &checkpoint.content.sha256,
        )?;
        drop(content_path);
        let content_ref = digest_of(&content);
        let blob = BlobDeclaration {
            sha256: content_ref,
            declared_len: len_of(content.len())?,
            source: BlobSource::Provide(admitted_copy(ledger, &content)?),
        };
        push_blob(&mut blobs, ledger, blob)?;
        let record = SessionCheckpointRecord {
            key: RecordKey::new(RecordKind::SessionCheckpoint, checkpoint.target_id),
            revision: RecordRevision::FIRST,
            location,
            content_ref,
            common: import_audit(
                ledger,
                principal,
                context,
                &checkpoint.source_id,
                SourceLocation::row(SourceArray::Checkpoints, row),
            )?,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        let event = prepare(
            EventInput::SessionCheckpoint(&record),
            ledger,
            SourceLocation::row(SourceArray::Checkpoints, row),
        )?;
        push_event(&mut events, ledger, event)?;
        PathMapping::push(
            &mut mapping.checkpoint,
            &checkpoint.source_path,
            checkpoint.target_id,
            ledger,
        )?;
        counts.increment(StateKind::SessionCheckpoint)?;
    }

    // --- frontier: every section, every row ----------------------------------
    for (row_index, row) in plan.frontier.iter().enumerate() {
        // Concept mentions become real references only where they name a Term in this source;
        // an unknown mention stays text in the package, never an invented id.
        let mut refs: Vec<RecordKey> = Vec::new();
        for cell in &row.cells {
            if let Some(id) = lookup_term(&term_ids, cell) {
                admit_and_reserve(&mut refs, 1, ledger)?;
                refs.push(RecordKey::new(RecordKind::Term, id));
            }
        }
        refs.sort_unstable();
        refs.dedup();
        // Priority is the authored column when it is one of the three levels; status is never
        // inferred from it.
        let priority = match row.cells.last().map(String::as_str) {
            Some("高") => 0,
            Some("中") => 1,
            Some("低") => 2,
            _ => 1,
        };
        let text = admitted_format(
            ledger,
            format_args!(
                "{}/{}/{}: {}",
                row.section, row.table, row.declaration_order, row.raw
            ),
        )?
        .into_bytes();
        let text_ref = digest_of(&text);
        let blob = BlobDeclaration {
            sha256: text_ref,
            declared_len: u64::try_from(text.len()).unwrap_or(u64::MAX),
            source: BlobSource::Provide(text),
        };
        push_blob(&mut blobs, ledger, blob)?;
        let record = FrontierEntryRecord {
            key: RecordKey::new(RecordKind::FrontierEntry, row.target_id),
            revision: RecordRevision::FIRST,
            status: FrontierStatus::Open,
            concept_refs: refs,
            text_ref,
            priority,
            common: import_audit(
                ledger,
                principal,
                context,
                &row.source_id,
                SourceLocation::row(SourceArray::Frontier, row_index),
            )?,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        let event = prepare(
            EventInput::Frontier(&record),
            ledger,
            SourceLocation::row(SourceArray::Frontier, row_index),
        )?;
        push_event(&mut events, ledger, event)?;
        PathMapping::push(&mut mapping.frontier, &row.source_id, row.target_id, ledger)?;
        counts.increment(StateKind::FrontierEntry)?;
    }

    // --- Finding 6: high-water marks from actual MAX ids ---------------------
    // A mark is the largest id ASSIGNED in a space, never a row count. Emitting these is
    // necessary imported state: after a physical import a valid create must not reuse an id.
    let mut watermarks: Vec<PackageWatermark> = Vec::new();
    let spaces: [(IdSpace, StateKind); 5] = [
        (IdSpace::TermId, StateKind::Term),
        (IdSpace::AssertionId, StateKind::Assertion),
        (IdSpace::ProposalId, StateKind::Proposal),
        (IdSpace::FrontierEntryId, StateKind::FrontierEntry),
        (IdSpace::SessionCheckpointId, StateKind::SessionCheckpoint),
    ];
    for (space, kind) in spaces {
        // Read the MAX assigned id straight from the single mapping representation.
        let family = match kind {
            StateKind::Term => &mapping.term,
            StateKind::Assertion => &mapping.assertion,
            StateKind::Proposal => &mapping.proposal,
            StateKind::FrontierEntry => &mapping.frontier,
            StateKind::SessionCheckpoint => &mapping.checkpoint,
            _ => continue,
        };
        let max_id = family
            .iter()
            .map(|entry| entry.target_id)
            .max()
            .unwrap_or(0);
        if max_id == 0 {
            continue;
        }
        let event = IdAllocationEvent {
            allocation: Watermark { space, hwm: max_id },
            reason: Some(admitted_format(
                ledger,
                format_args!(
                    "legacy import high-water mark for {} after mapping {max_id}",
                    kind_name(kind)
                ),
            )?),
            extensions: Extensions::new(),
        };
        let event = prepare(
            EventInput::IdAllocation(&event),
            ledger,
            SourceLocation::plan(),
        )?;
        push_event(&mut events, ledger, event)?;
        watermarks.push(PackageWatermark {
            space: space as u16,
            hwm: max_id,
        });
    }

    // --- Finding 8: every requirement, resolved by its own policy ------------
    for requirement in &plan.unresolved {
        let resolved = resolve_requirement(requirement, principal, context, &plan, ledger)?;
        admit_and_reserve(&mut dispositions, 1, ledger)?;
        dispositions.push(resolved);
    }

    // --- Finding 10: expected counts from the REAL projector -----------------
    // Project every event with the canonical projector and merge the whole imported state,
    // instead of incrementing counters by hand. The two Genesis singletons are part of the
    // imported current state, so the total is 262 family rows + 2 singletons = 264.
    let projected = project_whole_state(&events, ledger)?;
    for (kind, count) in projected.iter() {
        match Some(counts.get(kind)).filter(|recorded| *recorded > 0) {
            Some(recorded) if recorded == count => {}
            Some(_recorded) => {
                return Err(MappingError::Internal {
                    location: SourceLocation::plan(),
                    invariant: "projector counted rows the mapping did not record the same way",
                });
            }
            None => {
                return Err(MappingError::Internal {
                    location: SourceLocation::plan(),
                    invariant: "projector produced rows the mapping never recorded",
                });
            }
        }
    }
    // Genesis singletons are projected but not constructed by this mapping's family loops.
    for singleton in [StateKind::KbMetadata, StateKind::RuleConfig] {
        let _ = singleton;
    }
    counts.increment(StateKind::RuleConfig)?;

    // Native record ceiling, applied before the result exists.
    let native_total: u64 = counts
        .iter()
        .try_fold(0u64, |total, (_, count)| total.checked_add(count))
        .ok_or(MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "the expected row counts overflow a u64",
        })?;
    if native_total > limits.max_records {
        return Err(MappingError::InvalidSource {
            location: SourceLocation::plan(),
            detail: "the mapping exceeds the caller's record ceiling",
        });
    }

    // --- the package, now that the mapping and dispositions are final --------
    // The session's counts, admitted BEFORE growth — never a `collect`. Built here because the
    // package carries the same projection.
    let mut expected_counts: Vec<CountPair> = Vec::new();
    admit_and_reserve(&mut expected_counts, counts.len(), ledger)?;
    for (kind, count) in counts.iter() {
        expected_counts.push(CountPair { kind, count });
    }

    // Every field is borrowed from the plan or from already-admitted state: no copy, so nothing
    // further needs admitting for the package's descriptive half.
    let kb_uuid_buf = hex16(&target_kb);
    let kb_uuid_hex = String::from_utf8(admitted_copy(ledger, &kb_uuid_buf)?).map_err(|_| {
        MappingError::Internal {
            location: SourceLocation::plan(),
            invariant: "the hex identity was not UTF-8",
        }
    })?;
    let provenance = PackageProvenance {
        archive_dir: &plan.source.archive_dir,
        authority: &plan.source.authority,
        kb_id: &plan.source.kb_id,
        files: plan.source.files,
        bytes: plan.source.bytes,
        extraction: &plan.source.extraction,
        unresolved: {
            let mut view: Vec<RequirementView<'_>> = Vec::new();
            admit_and_reserve(&mut view, plan.unresolved.len(), ledger)?;
            for entry in &plan.unresolved {
                view.push(RequirementView {
                    family: &entry.family,
                    subject: &entry.subject,
                    field: &entry.field,
                    reason: &entry.reason,
                });
            }
            view
        },
    };
    let target = PackageTarget {
        kb_uuid: &kb_uuid_hex,
        session_id: &context.session_id,
        import_time: context.import_time,
        reason: &context.reason,
        principal,
        expected_counts: {
            let mut view: Vec<CountView<'_>> = Vec::new();
            admit_and_reserve(&mut view, expected_counts.len(), ledger)?;
            for pair in &expected_counts {
                view.push(CountView {
                    kind: kind_number(pair.kind),
                    count: &pair.count,
                });
            }
            view
        },
        watermarks: &watermarks,
    };
    // Reserved through the ledger FIRST, then filled by the borrowed view — the admission is
    // what pays for these slots, not the push.
    let mut package_view: Vec<PackageMapping<'_>> = Vec::new();
    admit_and_reserve(
        &mut package_view,
        mapping.term.len()
            + mapping.assertion.len()
            + mapping.proposal.len()
            + mapping.checkpoint.len()
            + mapping.frontier.len(),
        ledger,
    )?;
    package_mapping_of(&mapping, &mut package_view);
    let package_bytes = encode_package(
        PackageInput {
            plan_bytes,
            archive_content_digest: &plan.source.archive_content_digest,
            source_digest: &plan.source.source_digest,
            mapping: &package_view,
            dispositions: &dispositions,
            target: &target,
            provenance: &provenance,
            files: &package_files,
        },
        ledger,
    )?;
    let package_sha = digest_of(&package_bytes);
    // ONE owner for the package payload. The blob's `Provide` is the copy the executor publishes;
    // `ImportPackage` borrows its accessor rather than keeping a second Vec beside it, so a
    // single payload is not silently charged as two or under-counted as one.
    let package_len = u64::try_from(package_bytes.len()).unwrap_or(u64::MAX);
    let blob = BlobDeclaration {
        sha256: package_sha,
        declared_len: package_len,
        source: BlobSource::Provide(package_bytes),
    };
    push_blob(&mut blobs, ledger, blob)?;
    let package = ImportPackage {
        sha256: package_sha,
        len: package_len,
    };

    let import_session = ImportSessionRecord {
        import_session_id: admitted_string(ledger, &context.session_id)?,
        stage: ImportStage::Prepare,
        // The decoded supplied value, exactly — never a hash of a hash.
        source_digest,
        plan_ref: package_sha,
        // The actual projection, sorted by state kind — never an assumption about the
        // plan's 335 mapping records, which is a different quantity entirely.
        expected_counts,
        import_complete: false,
        audit: sage_db_types::persist::CreationAudit {
            recorded_by: Some(admitted_string(ledger, principal)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some(admitted_string(ledger, &context.reason)?),
            created_at: context.import_time,
        },
        extensions: Extensions::new(),
    };
    // Both extraction digests are validated above; the archive-content one is recorded in the
    // package (which is `plan_ref`), and the source one is the session's `source_digest`.
    let _ = archive_digest;
    let event = prepare(
        EventInput::ImportSession(&import_session),
        ledger,
        SourceLocation::plan(),
    )?;
    push_event(&mut events, ledger, event)?;

    // Measure the COMPLETE retained extent, then ADOPT credit this attempt already holds.
    //
    // `adopt_retained` transfers existing untransferred credit into an owning token: it adds no
    // fresh `live_bytes` (so the output is not charged twice) and moves the extent from
    // `untransferred` to `held`. `reserve_owned` would admit a second time — the overlap bug
    // this replacement exists to remove. A shortfall is a defect, not a budget outcome.

    let retained_bytes = complete_output_extent(OutputExtent {
        events: &events,
        blobs: &blobs,
        package: &package,
        session: &import_session,
        counts: &import_session.expected_counts,
        mapping: &mapping,
        dispositions: &dispositions,
        watermarks: &watermarks,
    })?;
    // Pure transfer. Every retained buffer was admitted **before** it was allocated, so the
    // extent has already been paid for and adoption moves that credit into an owned token
    // without admitting anything. A shortfall here is not a budget outcome to paper over: it
    // means the measurement and the accounting disagree, which is this crate's own defect.
    let retained = match sage_db_store::lease::LeaseLedger::adopt_retained(ledger, retained_bytes) {
        Ok(retained) => retained,
        Err(sage_db_store::lease::TransferError::Untransferred {
            requested_bytes,
            available_bytes,
        }) => {
            // A shortfall means a retained buffer was not admitted before it existed. The two
            // figures are the evidence; no string is needed to state it.
            let _ = (requested_bytes, available_bytes);
            return Err(MappingError::Internal {
                location: SourceLocation::plan(),
                invariant: "retained extent was not fully admitted before growth",
            });
        }
    };

    Ok(PreparedImport {
        owned: OwnedOutput {
            events,
            blobs,
            package,
            import_session,
            path_mapping: mapping,
            dispositions,
            watermarks,
            retained,
        },
    })
}

/// Strictly decode a digest string into its exact 32 bytes.
///
/// Two spellings occur in the actual plan: `<64 hex>` for record fields, and `sha256:<64 hex>`
/// for the source-level digests. Both are accepted; nothing else is.
///
/// Strict means strict: wrong length, a non-hex character or an uppercase spelling is refused.
/// The previous version silently produced zero bytes for any malformed pair, which turned "the
/// plan is damaged" into "the digest is 00…00" and made a later comparison succeed against a
/// value nobody authored.
fn decode_digest(value: &str, location: SourceLocation) -> Result<[u8; 32], MappingError> {
    let invalid = |detail: &'static str| MappingError::InvalidSource { location, detail };
    let hex = value.strip_prefix("sha256:").unwrap_or(value).as_bytes();
    // Byte-level check FIRST, before any UTF-8 slicing: a 64-byte string containing multi-byte
    // characters can have `&s[0..2]` land inside a char and PANIC. Strictness that panics is not
    // fail-closed, so every byte is validated as ASCII lowercase hex up front.
    if hex.len() != 64 {
        return Err(invalid("a digest is not 64 lowercase hex digits"));
    }
    if !hex.iter().all(u8::is_ascii_hexdigit) || !hex.iter().all(|b| !b.is_ascii_uppercase()) {
        return Err(invalid("a digest contains a non-lowercase-hex character"));
    }
    // From here every byte is a validated ASCII hex digit, so fixed byte pairs are safe.
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let pair = [hex[index * 2], hex[index * 2 + 1]];
        let digit = |b: u8| -> u8 {
            match b {
                b'0'..=b'9' => b - b'0',
                _ => b - b'a' + 10,
            }
        };
        *slot = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    Ok(out)
}

/// Check a plan-recorded digest against the bytes actually read.
///
/// The plan records digests as `sha256:<hex>`; the actual bytes are hashed and the comparison is
/// on the decoded value, never on a re-serialised string.
fn verify_digest(
    actual: &[u8],
    expected: &str,
    location: SourceLocation,
    what: &'static str,
) -> Result<(), MappingError> {
    let expected = decode_digest(expected, location)?;
    if digest_of(actual) != expected {
        return Err(MappingError::InvalidSource {
            location,
            detail: what,
        });
    }
    Ok(())
}
