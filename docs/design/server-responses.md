# Recorded-response assembly

Status: P4 library slice. Assembly only — **not** transport, not serialisation.

## Precondition on the outcome path

`assemble_outcome` performs a real `read_committed_blob` on the root the store already holds, but
does **not** re-run the import gate. The caller has authenticated the request and taken the
lookup through the store's own import-and-durability gate; this module inherits that basis rather
than opening a second authority for it.

`crates/server/src/responses.rs` turns a kernel `HandleOutcome` (or a store `OutcomeLookup`)
into the generated protocol response DTOs. It reads the result blob the kernel recorded, parses
it with the canonical borrowed decoder, cross-checks it against the outcome event, and fills the
full affected/counter lists.

## What it is not

* **No transport.** No HTTP, RPC, framing, or serialisation. The DTOs are returned populated;
  encoding them on a wire is later work.
* **No second decoder.** `sage_db_store::typed::read_result_payload` parses the blob — the
  canonical borrowed reader. Nothing here hand-reads result bytes.
* **No second authority for identity.** The proposal id, version and counters come from the
  *recorded* outcome, never from the request. An exact retry after a lost ACK therefore recovers
  the original allocation even though the request is replayed.
* **No re-authorization.** `assemble_outcome` takes an already-performed `OutcomeLookup` rather
  than a principal/request pair, so it does not invent an anonymous cross-principal scope. Route
  authentication stays with the command side's verified `Grants`; there is one authority.

## The one shared builder

`recorded_bodies` is the only place a `RecordedCommit` is filled, for all three write methods
and for `GetRequestOutcome` found. One builder means one decision about what a commit recorded:

1. `store.read_committed_blob(event.result_ref, caller_ceiling, ledger)` — the real reader, so
   an over-ceiling result is the reader's own `Ceiling` refusal, not an invented one.
2. `typed::read_result_payload(bytes, caller_ceiling, ledger)` — canonical parse.
3. Cross-check `parsed.version() == event.commit_version`; a disagreement is a durable
   inconsistency and is refused rather than presented as success.
4. Copy affected pairs and counters into buffers **pre-reserved at their real element counts**,
   before any row is written.

## Facts it refuses to invent

| Situation | Behaviour |
|---|---|
| `HandleOutcome::Unknown` | Carries the request id only. No version, no result read, no latest fallback. |
| `AlreadyCommitted` | The ORIGINAL recorded bodies and status, recovered from the original revision. |
| Empty `affected`/`counters` on a present `RecordedCommit` | A complete, legitimately empty result — never a placeholder. |
| `recorded: None` | "Not loaded", never "empty commit". |
| Review status | The status the ORIGINAL review persisted, via `Store::history` at the decoded identity and revision — never the requested decision, never the proposal's current status. |
| Status unprovable within bounds | A typed error, never `APPROVED` by default. |
| Historical read **failed** | `HistoryError(OpenRefusal)` — a definite failure, never absence. |
| `Unknown` | Carries the authenticated request id, and nothing else. No version, no read. |
| Genesis / missing outcome event | `ResponseError::NoRecordedOutcome`, never a fabricated success. |

The basis and the version are **different facts**: the basis states where the bytes were read
(the current publication), the commit version states what the outcome recorded. An old outcome
assembled after later commits keeps its original version while its basis names the current
publication.

## Refusal taxonomy

`refusal_class` names **every** `HandleRefusal` variant — no wildcard, so adding a variant fails
to compile rather than silently becoming a generic answer. The same holds for `OpenRefusal`,
`FsErrorKind`, `FormatError`, `CommitRefusal` and `Refusal`.

Two rules govern it:

1. **Transient, capacity and permission failures are never corruption.** `ErrorCode` is the only
   machine-readable remedy signal on the wire, so `NoSpace` → `UNSPECIFIED` (the protocol has no
   "unavailable" member, and the phrase says exactly what happened), `WouldBlock`/`Interrupted` →
   `CONFLICT` (retryable), `PermissionDenied` → `PERMISSION_DENIED`, `Unsupported` →
   `UNSUPPORTED`, `Cancelled`/`DeadlineExceeded` → their own codes. `CORRUPT` is reserved for
   structural kinds: `HashMismatch`, `Symlink`, `NotRegular`, `NotADirectory`, `IsADirectory`,
   `CrossDevice`, plus `OpenRefusal::Corrupt` and `FormatError::Corrupt`.
2. **A budget detail is emitted only from a real `BudgetError`.** `FsError` carries its
   `BudgetError` and `ControlStop` intact, so both the code and the figures are reported when
   they exist. A bare `FsErrorKind::BudgetExceeded` with no payload claims neither the `BUDGET`
   code nor any figure.

`OutcomeIndexIncomplete` is **coverage**, not damage: it maps to `CONFLICT` with a phrase that
says the index does not cover committed history.

`HandleRefusal::History(OpenRefusal)` preserves the exact canonical class it carries — the same
guarantee R1 established on the read path, now on the write path too.

`ResponseError` distinguishes what was actually consulted:

| Variant | Means |
|---|---|
| `Refused(Refusal)` | the caller's ledger refused |
| `Allocation { requested }` | the allocator was consulted and failed at that extent |
| `ExtentOverflow { elements }` | arithmetic: the extent is not representable (**no allocator ran**) |
| `Transfer` | retained credit was not admitted (**neither budget nor allocation**) |
| `ShortCanonicalRead { table, index }` | the canonical reader declined an index it counted — a local invariant defect, never a silent truncation |

A short canonical read is an error, not a `break`: silently dropping rows would produce a
DTO carrying fewer rows than its declared count with no failure at all.

## Verified through the real assembler

The history-failure guarantee is tested twice, with different scopes:

* `a_history_read_failure_is_reported_as_an_error_not_an_absence` — **scope: the store only**.
  It proves `Store::history` returns `Error`, not `Absent`, for an unreadable transaction. It
  does **not** execute the response mapper, and is labelled so it cannot be mistaken for that.
* `the_assembler_maps_a_history_read_failure_to_history_error` — **scope: the real assembler**.
  A test-local forwarding filesystem delegates every capability and operation to the model
  unchanged, and arms the model's canonical `FaultSchedule` with `FailNoEffect` only when
  `read_exact_into` sees `EntryName::Transaction`. The result blob is `EntryName::Blob`, so it
  reads normally and the historical search is the stage that fails. The test then asserts
  `ResponseError::HistoryError(OpenRefusal::Filesystem { .. })`, and asserts the injection
  actually fired.

The second test is what makes the mapping claim real. Restoring the `Error → Absent` collapse
fails it — and leaves the store-only test passing, which is exactly why the two are kept
separate rather than merged.

## Ownership and accounting

Every buffer is **admitted before it is allocated**, and the resulting credit is adopted into a
`Response` that owns it:

1. measure the exact extent — table slot bytes plus each string's byte length, computed from the
   values (a decimal is at most 39 bytes, a digest 64, a uuid 32);
2. `try_charge(extent)` then `adopt_retained(extent)`, giving one `OwnedReservation`;
3. allocate fallibly at those sizes, then build the strings from stack-formatted decimal/hex/UUID
   buffers.

There is no `to_string`, no `to_owned`, no `with_capacity`, and no `Box::new` on this path.
Existing `String::new()` calls are followed immediately by an admitted `push` that reserves the
exact length. A failed allocation is `ResponseError::Allocation { requested }` carrying the real
extent that failed — never a unit-only variant — so it cannot be confused with a budget refusal.

Attempt capacity never adopted is returned when the attempt drops; admitted credit held by a
`Response` is released when **that** drops. Both facts are observed through the counter's own
figures in the test suite, not asserted from a statistic.

The DTO is reachable only by borrow, so a caller cannot move buffers out from under the credit.
A fixed three-slot credit array holds the tokens, because holding them must not itself allocate.

## Verification

`crates/store-tests/tests/p4_responses.rs` drives real kernel commands against a real `Store` on
the model filesystem, then assembles. It covers: a committed response with the full recorded
body (proposal identity, proposal counter, version, `result_ref`, basis); `Unknown` reading no
result and carrying no version; `NotCommitted` preserving `IDEMPOTENCY_MISMATCH` with no
invented budget; a review's approved and rejected statuses; `Found` with original version,
digests and basis; `Absent` distinct from `Incomplete`; a blob ceiling refusal; and a starved
ledger surfacing a real `Entries` budget refusal.

Discriminating counterexamples, each mutation-checked against the production code:

| Mutation applied | Test that failed |
|---|---|
| report `store.head()` instead of the recorded version | `a_later_commit_does_not_change_the_original_recorded_version` (head 5 into a v3 outcome) |
| default the review status to `Approved` | `a_rejection_records_rejected_as_the_original_status` (left 2 ≠ 1) |

These are the fragile assertions; the rest are ordinary reads of recorded facts.
