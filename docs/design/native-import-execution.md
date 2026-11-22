# Native import execution — operator guide

Status: **staged executor for administrative import**. Not a cutover tool, not a readiness
statement and not device-durability evidence.

Code: `sage-db/crates/migrate/src/execute.rs` (executor), `src/intent.rs` (op9/v1 payload).
Forced dependency: `sage-db/crates/migrate/src/mapping.rs` (the prepared plan this executes).

## 1. What an import is

Five stages, written by an `admin`-authorized executor onto one target KB:

| stage | writes |
|---|---|
| `Prepare` | the `ImportSession` record (`import_complete = false`) plus every blob the package needs. No record row. |
| `Validate` | **no mapped record at all**; the stage commits its own outcome so an exact retry is answered from storage rather than replayed |
| `Import` | the mapped record events and the blobs they reference |
| `Verify` | nothing; the evidence is the outcome, and verification runs separately |
| `Activate` | nothing but the session row at `import_complete = true` |

Between `Prepare` and `Activate` the target is **closed**: `Store::import_readiness` reports
`Incomplete`, and the ordinary kernel commands and snapshot both refuse. The gate is a persisted
row, so it survives a process restart without this tool being consulted.

## 2. Running it

```text
bootstrap_with_profile(fs, root, CreateDir::IfMissing, kb,
                       (RuleProfile::LegacyV1, &rules_bytes), &Limits::V1, ledger)
```

A migration target must be created with `legacy-v1`, using the profile-aware constructor. The
default `bootstrap` creates a `formal-v1` root and will not accept legacy rule bytes — the verifier
refuses the contradiction rather than guessing.

Then, per stage, with the plan bytes the operator holds and an externally frozen expectation:

```text
ExpectedIdentity { plan_sha256, source_digest, kb_uuid, import_session_id }
StepContext      { principal, tx_id, created_at, limits, expect_version }
write_stage(store, grants, &expected, plan_bytes, &prepared, &step, &context, ledger)
```

`plan_sha256` must be computed **by the operator, outside** this tool, over the exact plan file it
is about to submit. The executor compares the supplied bytes against it before decoding anything;
deriving the expectation from the plan itself would prove nothing.

`tx_id` names the transaction file, so **every stage needs its own**. Reusing one makes the second
publication collide.

## 3. Verification and completion

```text
let verified = verify_and_attest(&mut store, &prepared, &expected, ledger)?;
```

`verify_written` compares, on the handle it is given:

* **counts** for all thirteen state kinds against the plan, with an unplanned kind compared to
  **zero** — an extra row of a family the plan never mentions fails;
* **the recorded watermarks, in production**: for every space the plan declares, the committed
  basis is read on this handle and must be at or above the planned mark. A missing or low mark is
  a typed defect, and the marks are bound into the `Verified` token. A mark advances only through
  an `IdAllocation` event (it is a maximum over recorded allocations), so a create that consumes
  an id without recording the allocation leaves the mark where it was;
* **the full typed after-image of every planned record, of every family** — including the
  natural-key kinds — decoded from the mapping's own event bytes through the canonical dispatcher
  and compared with `PartialEq` against the store's record. Identity and revision alone are never
  the check: a wrong atom or content at an unchanged revision fails with `ValueMismatch`. The
  natural keys are **not** all content hashes: `Asset` is keyed by its blob digest, `Model` by its
  configured name, and `KbMetadata`/`RuleConfig` are singletons keyed by kind — which is exactly
  why the value comparison, not the key, is what makes them meaningful;
* **every revision a plan carries**, grouped by identity: the newest planned image against the
  CURRENT row, every earlier one through `Store::history` on a durable basis. A record planned at
  two revisions verifies clean, and an earlier revision that is absent is `MissingRecord`;
* **every planned blob**: its length, its bytes where the mapping supplies them, and a re-hash equal
  to its own digest. The self-contained package is included;
* **the session**, still open at this identity on the same basis.

`Verified` is not `Clone`, `Copy` or `Default` and binds the durable basis, session, plan digest,
source digest and package reference. `mark_complete_after_verification` consumes it and refuses a
token whose bound state has moved.

**Fresh-process discipline.** A token is a value on one handle, so a single library call cannot
prove the verification ran in a freshly reopened *process*. The driver owns that: drop the `Store`,
reopen it, call `verify_and_attest` on the reopened handle, then complete. A restart that preserves
an identical basis is legitimate and the token check is designed to allow it; anything that
committed in between changes the basis and is refused.

## 4. Resume

Every step's request id is derived from the actual session, stage and batch. Before any mutation the
executor looks the id up:

* **found, same canonical digest** → `AlreadyCommitted` with the original version, nothing written;
* **found, different digest** → `SessionConflict`, refusing to replay a request this step never made;
* **absent** → a novel step, which alone may write.

That lookup runs *before* the fresh-target admission, so an acknowledged `Prepare` can always be
retried — the same ordering ordinary kernel commands use.

## 5. Idempotency and `Unknown`

A commit that returns `Unknown` is an ambiguous attempt, returned as an outcome rather than an
error. Do **not** assume it committed or that it did not: resolve it (reopen the store) before
retrying, exactly as any other kernel command.

## 6. Limits, stated honestly

* **No device-durability claim.** The modeled crash sample shows what a process restart can
  observe; it is not power-loss evidence, and nothing here is Linux-specific evidence.
* **No cutover or readiness claim.** This executor writes a target; it does not route production
  reads to it.
* **Historical coverage is per identity, not per family.** Every planned revision is compared —
  newest against current, earlier ones through `Store::history`. What is *not* done is replaying a
  record's history beyond the revisions the plan itself declares.
* **Defects keep the canonical taxonomy.** `VerifyDefect` carries `Refusal`, `FormatError`,
  `RecordReadError`, `OpenRefusal`, `RecordScanError`, `HistoryIncomplete` and the
  incomplete-reason types unchanged, so a budget refusal is never reported as corruption, or the
  reverse. Extent arithmetic overflow and a failed credit transfer have their own variants rather
  than being reported as allocation failures. It is deliberately not `Copy`: the variants own real
  error values.
* **Ownership.** Every retained verification table is admitted through the caller's ledger before
  it is reserved; the current images are **borrowed** from the store's own retained owners, so the
  scan clones nothing. The token's extent is charge-then-**transfer** (`adopt_retained`), never a
  second reservation, so admitted bytes are counted once.
* **`Validate` performs no record work.** It writes no mapped record and no blob beyond its own
  outcome, which is what the stage table above says. A future plan/source re-check would be added
  here, and this line would change with it.
* **Foreign-operator preparation is not supported.** A plan prepared under another operator's
  session is refused by the frozen identity check rather than being described as delegated.

## Root R7 verification-index correction

The first scan counts state kinds only. Its unused membership table and repeated planned-image
scan are removed. The existing planned table is already ordered by full identity and revision;
an adjacent comparison selects the latest image. Every selection and natural-key membership
comparison checkpoints and pays an entry on the same caller ledger. Current values, earlier
history, blobs and recorded watermarks retain their full checks. Validate carries no mapped
records; Import alone does. This changes no persistent/request bytes.
