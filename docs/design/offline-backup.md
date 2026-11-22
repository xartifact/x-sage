# Offline versioned backup and fresh-root restore

Status: **physical storage primitive**. Not a phase, readiness, activation or device-durability
statement, and it authenticates nothing.

Code: `sage-db/crates/store/src/backup.rs`; evidence
`sage-db/crates/store-tests/tests/p5_backup.rs`.

## 1. What it is

`create` exports one committed version of a KB — plus every blob that version's **history**
references — into a fresh directory. `restore` rebuilds such a directory into a fresh root and
returns a genuine `Ready` store.

Like `verify`, both are primitives. They mint no grants and check no identity; a future admin
adapter authenticates `Admin` on the actual KB before calling in, and routing is outside this
component.

## 2. Authority is never chosen by listing a directory

The source is bound through the locked `Store`, so the export holds a borrow a safe caller cannot
mutate across. `CURRENT` remains the only commit root: `open::observe_root` reads it once and follows
the recorded predecessor chain, so an orphan or a higher-numbered file can never become authority.

For an older `--version`, the selection is the real `ChainRow` in that verified chain, and the
artifact's `CURRENT` is **re-encoded canonically** from it — copying the source's head `CURRENT`
would make the artifact claim a version whose manifest it does not carry.

## 3. Blobs come from history

`backup_blob_union` delegates to the canonical replay engine. It replays from genesis through the
selected version and includes every blob referenced along that chain, including superseded content.
Blobs added in later versions and unreferenced files stay out of the artifact.

Before completion, the destination's copied chain is independently replayed and its blob references
are compared to the inventory.

## 4. Artifact format (offline, not a KB event)

**Inventory** — 72-byte header, then fixed 65-byte rows:

| part | bytes |
|---|---|
| magic `SGBKUP\0\x01` | 8 |
| kb_uuid | 16 |
| selected version (LE) | 8 |
| selected manifest file SHA-256 | 32 |
| row count (LE) | 8 |
| rows `tag:u8 \| version:u64 \| id16 \| file_sha32 \| file_len:u64` | 65 each |

Tag `0` contains CURRENT with version and id set to zero. Its SHA-256 and length describe the
re-encoded pointer for the selected version. Tag `1` contains a manifest with its version and id.
Tag `2` contains a transaction with its id and version zero. Tag `3` contains a blob with version
and id set to zero and a SHA-256 that names the content. Rows are ordered by tag, then version and id
for manifests and transactions, or hash for blobs. `EntryName` renders each file name, and duplicate
typed names are refused.

**Completion** — 48 bytes: magic `SGBDONE1`, the inventory's raw SHA-256, and its length. It is
written **last**, after the destination has been verified and synced. A directory without it is not
a usable backup, however many files it holds.

A zero-length **blob is legal** (an empty body has a real SHA); only manifests and transactions must
be non-empty.

## 5. Sequence, and what failure means

Export holds the locked source basis through version selection and replay. It checks the configured
ceilings before creating the artifact root, streams the selected files, and writes the inventory.
The artifact root stays locked while the copied chain is replayed and the files and directory
entries are synced. Only then is `backup.complete` written and synced along with the root directory.

An I/O failure can leave a partial directory. A caller receiving an unknown effect should inspect
the artifact through restore validation. Existing backups are never overwritten.

Restore checks for the artifact's existing lock entry and holds an exclusive lock on the artifact
through validation and copy. It checks the marker, inventory, every listed file's length and hash,
and the canonical CURRENT and manifest chain before creating the destination. Destination creation
is exclusive even for an existing empty directory. Copy rechecks each file's hash, and canonical
replay and sync establish the returned store's `Ready` state under the destination lock.

## 6. Errors keep their class

| situation | variant |
|---|---|
| no durable basis | `NoDurableBasis` |
| not an ancestor / zero / future | `NotAnAncestor` |
| a **configured** ceiling | `LimitExceeded` — never a source refusal |
| ledger budget or stop | `Refused(Refusal)` |
| counted extent not representable | `ExtentOverflow` |
| allocator refusal | `Allocation { requested }` |
| filesystem refusal | `Filesystem { step, error }` — the **whole** `FsError`, so `Effect`, budget and control survive |
| no completion marker | `MissingComplete` — only for a real NotFound |
| completion does not describe the inventory | `CompleteMismatch` |
| corrupt inventory | `InventoryCorrupt(defect)` |
| file length/hash disagreement | `FileMismatch { tag }` |
| target already exists | `DestinationExists` |
| wrong KB | `KbMismatch` |
| destination replay refused | `Replay` |
| confirmed **source** read/replay failure | `SourceRefused` — the only class that fences the source |
| destination preparation out of budget | `InitializerBudget` — labelled, **not** given a fabricated `0/0` payload |

## 7. Cost

One caller `LeaseLedger` spans planning, validation, copy, hashing and sync checks. Data files
stream through a 1024-byte stack chunk with `StreamingSha256::with_prepaid`. The marker, CURRENT
and inventory are bounded metadata buffers. Inventory storage is admitted before it is reserved.
Row ordering uses an insertion sort with one entry charge and a control check per comparison.
Its worst case takes quadratic comparisons.

## 8. Evidence

The model tests compare the restored basis, current records, historical term revision, request
outcomes, allocation high-water mark and referenced blob bytes. They also check older selection,
exclusion of newer and orphaned blobs, preservation of an existing destination, an unfinished
import, and byte, entry and stop refusals. One test corrupts CURRENT and a historical blob without
changing either length. The inventory checks include duplicate typed names and malformed metadata
rebound to a new completion hash.

The fault matrix exercises first and last calls to create a file, write, sync a file, sync an entry
and sync a directory, plus the root-parent sync. It includes supported unknown-after-effect cases
and an interrupted short write of the marker. Each sampled world with a complete marker must
restore, and each incomplete world must refuse before creating a root.

The ignored Linux operator test makes version 2 and version 3 backups, restores each to a fresh
directory and reopens each from disk on the qualified mount. The model and operator tests leave
device power-loss and production activation outside their scope.
