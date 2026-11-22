# Store/process qualification on the real OS backend

> Status: **qualification harness (R2)**, not a product feature. It consumes the checked
> `OsFileSystem::for_root` constructor (platform R2 policy); it adds no second platform policy,
> no shadow override, no forced unlock, no new production format, and no second durability claim.
> Upstream: [storage.md](storage.md) §1–§3, [migration.md](migration.md) §3, [system.md](system.md).

## 1. What gap this closes

The platform work proved the **backend**: `for_root` checks compile-time target (`linux/x86_64`),
`/proc/sys/kernel/osrelease`, `/proc/self/mountinfo` (both per-mount and superblock options), and
the pinned descriptor's mount identity, refusing any host/mount outside the validated bound. This
harness exercises the **real `Store`** on top of that backend across real OS processes, real
`bootstrap` / `commit` / `open_existing`, real `SIGKILL(9)`, and fresh-PID recovery over the same
files on disk.

| Layer | Uses |
|---|---|
| Backend | `OsFileSystem::for_root(root)` — the sole path to `DurabilityBasis::Documented` |
| Store | public `init::bootstrap`, `Store::commit`, `open::open_existing`, `Store::outcome`, `Store::retained_entries`, `Store::read_committed_blob` |
| Fixtures | read-only `tests/support/p16_fixture.rs`, via `#[path]` |
| Oracle | existing `oracle::attempts::AttemptHistory`, driven by observed attempts and actual persisted G0 outcomes |

`sage-db-store` does not depend on `sage-db-store-tests`; the dependency stays strictly one-way.

## 2. Host qualification, bootstrap shapes, and why ignored cases fail (never skip)

The real-OS cases in `p2_os_store_restart.rs` are marked `#[ignore]` so non-qualifying hosts still
compile and run the host-independent pure/unit controls. When invoked on the target:

```text
cargo test -p sage-db-store-tests --test p2_os_store_restart -- --ignored --nocapture --test-threads=1
```

Before any Store test body runs, `require_qualified_host` executes a `Guarded` `host-probe` child,
persists its raw output before parsing, and records + asserts in code (`host-qualification.kv`):

- compile-time and runtime target `linux/x86_64`;
- `/proc/sys/kernel/osrelease` non-empty and accepted by `OsFileSystem::for_root`;
- read-only `stat` (`MetadataExt::dev()`) showing `/var/tmp` shares `st_dev` with
  `/opt/xartifact/x-sage/data/kb` (zero writes to production data, service, or config);
- `OsFileSystem::for_root` returns `DurabilityBasis::Documented` (`POLICY=for_root/Documented`).

On any non-qualifying host, `require_qualified_host` **fails** rather than skipping.

### Missing-root and existing-empty-root bootstrap (R2)

In platform M1, pre-creating an empty root directory caused `bootstrap(..., CreateDir::IfMissing)`
to fail with `Container(Filesystem { step: Root, kind: Unsupported })`. Root reclassified that
behaviour as a checked-policy compatibility defect in M1 (`create_root` returned `Unsupported`
instead of `AlreadyExists`, stranding `init.rs`'s documented open-instead fallback). Platform R2
fixed `os.rs` to pin the direct parent in both cases and return `AlreadyExists` for an existing
directory.

The R2 qualification suite therefore tests **both** real bootstrap shapes:

1. **Missing-root bootstrap**: parent exists, KB root does not exist yet (`for_root` binds parent,
   `create_root` creates the root).
2. **Existing-empty-root bootstrap**: empty KB root directory already exists (`for_root` binds the
   root itself, `create_root` returns `AlreadyExists`, `init::bootstrap` opens the existing root).

## 3. Whole-state verification, oracle driving, and blob union coverage

### 3.1 Captured Genesis preservation baseline and raw row comparison (H1)

Immediately after `bootstrap` returns `head == Some(1)` — and **before** committing base-2 —
`seed` reads `store.retained_entries()`, asserts the captured Genesis count is **exactly 2**, and
writes the raw `Vec<StateEntry>` to an explicit CLI path (`--genesis-baseline <path>`) inside the
parent's artifact directory (strictly **outside** the KB root, written with `create_new` / `wx`).
The file uses test-only bounded checksummed framing (`P2OSGEN1` + bound root path + row entries +
SHA-256 footer); it is **not** a new production format.

On fresh-PID `reopen`:

- Missing or unreadable baseline fails immediately and is **never** recreated.
- The decoded baseline must match the exact bound KB root path and contain exactly 2 Genesis rows.
- `store.retained_entries()` is collected raw — **no** sorting, deduplication, or key-search on the
  actual side — and compared element-by-element against `fixture::expected_rows(&genesis, head)`.
- Full `StateEntry` equality covers `kind`, `key`, `revision`, and complete `body` bytes (including
  all audit fields and extensions), and verifies byte-identical preservation of both Genesis rows.
  Duplicates, missing rows, extra rows, wrong revisions, or out-of-order rows all fail.

### 3.2 `oracle::attempts` and full G0 outcomes for requests 2 & 3 (H2)

Every `reopen` inspects actual persisted G0 outcomes via `store.outcome(...)` for **both**
request 2 and request 3 (using actual fixture carrier headers/digests, never fabricated sentinel
versions) and drives existing `sage_db_store_tests::oracle::attempts::AttemptHistory`:

- Observed `AttemptHistory`:
  - `attempt-base-2`: `AttemptOutcome::Durable { version: 2 }` (always present; base-2 never lost).
  - `attempt-candidate-3`: omitted for base-2-only timelines (`--candidate3-outcome none`),
    `AttemptOutcome::Unknown` for the unacked kill timeline (`unknown`), or
    `AttemptOutcome::Durable { version: 3 }` for the post-ACK kill timeline (`durable`).
- Recovered `World`: derived from `head` **and** the actual persisted `OutcomeLookup::Found`
  records (`original_version`, `principal_id`, `request_id`, `payload_digest`) — never
  constant-present from `head` alone.
- Full G0 event assertions:
  - Request 2 must be `Found` with `original_version == 2` and `outcome.event() == fixture::expected_outcome(2)`.
  - Request 3 must be `Absent` **only** when `head == 2`, and `Found` with `original_version == 3`
    and `outcome.event() == fixture::expected_outcome(3)` when `head == 3`.
- Exact retry (`retry` mode): asserts `HandleOutcome::AlreadyCommitted { version: 2, recorded }`
  with `recorded` matching `fixture::expected_outcome(2)`, accepted by `AttemptHistory`, alongside
  zero named mutating/sync seam calls through `CountingFs` and byte-identical `CURRENT`.

### 3.3 Whole historical blob union

`reopen` verifies the **complete historical union** of committed blobs declared from Genesis rules
through `head` (`2..=head`), reading every blob via `Store::read_committed_blob` and checking exact
bytes and SHA-256:

- At `head == 2`: **4** unique blobs — Genesis/v2 rules, Term A1 body, Term B1 body, v2 result.
- At `head == 3`: **7** unique blobs — rules, **superseded A1 body**, **carried-forward B1 body**,
  v2 result, A2 body, C1 body, and v3 result.

## 4. Explicit arming and ordered root-sync proof (H3)

`CountingFs` is a pure delegating observer that always starts **unarmed**. In both crash-producer
modes (`unacked` and `post-ack`):

1. Setup opens `Ready(2)` while unarmed.
2. The driver asserts `!armed`, `fired.is_none()`, `replace_current == 0`, and both publish flags
   false before calling `reset_and_arm(...)` for candidate-3.
3. **`unacked` timeline**: armed with `Barrier::AfterReplaceCurrent`. Inside `replace_current`,
   only after `inner.replace_current` returns `Ok(())` on the bound root — and **before** returning
   to `Store` (so root `sync_dir` and outer `Committed(3)` ACK cannot execute) — the observer
   records `FiredOpRecord`, emits `BARRIER=after_replace_current`, `REPLACE_CURRENT_COMPLETED=true`,
   `ROOT_SYNC_AFTER_REPLACE=false`, `STATE=steady`, and blocks until the parent sends `SIGKILL(9)`.
   Fresh-PID recovery (`--candidate3-outcome unknown`) must recover either whole head 2 or whole
   head 3 (never head 1 or mixed state).
4. **`post-ack` timeline**: armed with `Barrier::None`. `CountingFs::sync_dir` sets
   `root_sync_after_replace = true` **only** when `inner.sync_dir(dir)` returns `Ok(())` for a
   directory with `dir.family().is_none() && dir.path() == bound_root` **after** a successful armed
   `replace_current` on that bound root. Cumulative `sync_dir` counts over family directories or
   pre-replace root syncs do not satisfy it. Before emitting `ACK3=3`, the driver requires outer
   `Committed(3)` matching `expected_outcome(3)`, `head == Some(3)`, a `Durable` view, and
   `validate_post_ack_proof` (`replace_current_completed && root_sync_after_replace`). Fresh-PID
   recovery (`--candidate3-outcome durable`) must recover whole head 3.

## 5. Child process discipline and pure/unit controls

- Every child is managed by RAII `Guarded`: kill+reap runs on every exit path, including assertion
  failure and panic unwinding.
- Two reader threads continuously drain **both** stdout and stderr to EOF; killing a child reaps it
  and joins both streams so the **complete** transcript up to death is saved (`create_new` / `wx`)
  before parsing — never a truncated pre-READY prefix.
- Driver stdout writes propagate both `writeln!` and `flush()` errors (exiting nonzero on error).
- Every multi-step timeline asserts distinct OS PIDs (`fresh_pid != producer_pid` / `seed_pid` /
  `holder_pid`) and checks child-reported `PID=` against `Child::id()`.
- **Load-bearing pure/unit controls** (compiled and executed on every host, making no OS durability
  claim):
  - `p2_os_store_driver` unit tests cover `PublishTracker` / `validate_post_ack_proof` (missing
    root sync with 12 non-root/failed syncs, inverted root-sync-before-replace order, unarmed,
    non-durable view), `GenesisBaseline` framing + root binding + `wx` non-overwrite,
    `validate_retained_rows` (wrong body/revision, missing/extra/duplicate/out-of-order rows),
    `validate_recovered_outcomes_and_oracle` (wrong `original_version`, mutated payload, lost ACK2,
    lost ACK3, head/outcome mismatch, exact retry), and historical blob union composition.
  - `p2_os_store_restart` pure tests cover silent-child absolute deadline expiry + kill/reap +
    raw failure output preservation, killed-status guard rejecting normal exit (0 and nonzero) and
    wrong signal (`SIGTERM` 15) while accepting `SIGKILL(9)`, drop/double-wait reaping, stdin pipe
    close exit, complete post-READY stdout + stderr capture on kill, missing baseline refusal
    without recreation, explicit host-probe refusal on non-qualifying hosts, and `wx` artifact
    non-overwrite.

## 6. Bounds and non-claims

- **Process death only**: `SIGKILL(9)` tests OS process crash against the assumed filesystem flush
  contract on one qualified host/kernel/Btrfs/device. It is **not** power-loss, remount, media,
  device-failure, or cross-filesystem durability evidence.
- **No production format or policy change**: the Genesis baseline file is a test-only transport
  written outside the KB; `OsFileSystem::for_root` remains the sole policy authority.
- **Local vs target evidence**: running default (non-ignored) tests on a non-Linux host exercises
  only the pure/unit controls and never constitutes a durable-Store pass; target qualification
  requires running the `--ignored` suite on the exact composed source on the qualified Linux/x86_64
  Btrfs target.
