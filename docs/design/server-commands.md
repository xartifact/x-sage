# Server command application

The P4 command adapter is used by the [loopback JSON runtime](server-http-runtime.md).

`crates/server/src/commands.rs` is the one layer between generated protocol DTOs and the
existing kernel command set. It authenticates, validates, and dispatches. It does not decode
the wire, assemble responses, run a queue, or own a clock.

## The order every request takes

```
bearer ──▶ CredentialVerifier::verify_bearer(store.kb_uuid(), caller_ledger)
              │  opaque failure            │  Grants (principal + capabilities)
              ▼                            ▼
        Unauthenticated            envelope.expect_version present? ──▶ UnsupportedExpectedVersion
                                           │ absent
                                           ▼
                          validate_{submit,review,commit}_request(envelope, payload)
                          on the SAME caller ledger, one walk for the whole request
                                           │
                                           ▼
                          envelope.kb_uuid == store.kb_uuid? ──▶ KbMismatch
                                           │ equal
                                           ▼
                           canonical kernel command on the same store
```

Three properties of that order are load-bearing:

1. **Authentication precedes every store lookup.** An unauthenticated request cannot reach a
   record read; the test asserts the filesystem's mutating-operation count is unchanged.
2. **One ledger spans authentication and protocol validation.** Budget and cancellation are
   therefore charged against the caller's real allowance, not a per-stage fiction.
3. **`expect_version` is refused before any mutation, even when it matches the current
   version.** No command in this set carries a whole-KB expectation, so treating a pin as
   "absent" would silently change the request's identity.

## The eight commands

| Protocol selection | Kernel command | Required capability |
|---|---|---|
| `SubmitPayload.claim_edit` | `claims::submit_claim_edit` | `Propose` |
| `SubmitPayload.concept_text` | `concept::submit_concept_text` | `Propose` |
| `ReviewPayload.claim_edit` | `claims::review_claim_edit` | `Review` |
| `ReviewPayload.concept_text` | `concept::review_concept_text` | `Review` |
| `CommitPayload.create_session_checkpoint` | `session::create_session_checkpoint` | `WriteAux` |
| `CommitPayload.revise_session_checkpoint` | `session::revise_session_checkpoint` | `WriteAux` |
| `CommitPayload.create_frontier` | `frontier::create_frontier` | `WriteAux` |
| `CommitPayload.revise_frontier` | `frontier::revise_frontier` | `WriteAux` |

`Admin` is not an inherited capability: a token holding `Propose` does not thereby review.

## Rules the adapter exists to hold

### The principal is the credential's

`Grants` from `verify_bearer` is the only source of the audit principal. `declared_author` in a
payload is copied into the record as **attribution** and never promoted to identity. The
generated schema has no actor, timestamp, or transaction-id field, so a client cannot supply
one: `ExecutionMeta { tx_id, created_at }` comes from the server executor, and `request_id`
comes from the authenticated envelope.

### Review dispatches on the wire family first

`ReviewPayload` is an explicit `oneof` (`claim_edit` | `concept_text`), not an inference. The
family is read from the payload **before** any stored record is consulted, because the two
families hash different operation kinds and the kernel's outcome-first lookup depends on
knowing which operation this is. A request whose wire family disagrees with the stored
`ProposalKind` is the kernel's own typed refusal — never a new guess at the family.

### Exactly one read pair for a review

Both review commands take a single proposal revision and build their read pair internally.
Accepting a two-pair envelope and checking only the first would tell the caller their other
dependencies had been verified. The adapter refuses anything but exactly one pair naming a
`Proposal`.

### Revise forwards the complete new state

`revise_frontier` and `revise_session_checkpoint` receive the caller's whole ref list and the
named record. The kernel derives the expected read set from both; no ref from the old record is
carried forward implicitly.

### Conversions are checked, never repaired

Protocol keys become `RecordKey` only through the closed registry and the kind's own id bound.
Decimal revisions become `RecordRevision::new`, which rejects `0` because "no revision" is
absence, not zero. Priority and status narrow with `try_from`, refusing overflow rather than
truncating. The read set preserves the caller's order exactly: no sort, no dedup, no repair.

### Optionals keep their meaning

`ConceptText.atom` and `.body` are `Option`; the field mask is derived from *presence*, so
`None` ("not edited") and `Some("")` ("becomes empty") stay distinct all the way into the
kernel. Payload strings are borrowed from the request rather than copied.

### No allocation on a refusal path

`UnsupportedExpectedVersion` carries the pinned version as a `u64`, parsed from the
already-validated decimal. A refusal must not allocate: the caller still holds the exact text it
sent, so copying it into the error would buy nothing and cost an unadmitted heap block. There is
no `clone()` anywhere in the adapter.

### The outcome is the kernel's, unchanged

`HandleOutcome` is returned as-is: `Committed`, `AlreadyCommitted`, `NotCommitted`, or
`Unknown`. There is no local success enum, because a second description would be a second
authority for what a commit means. A future response assembler reads the canonical result blob
out of it. The adapter never recomputes a digest and never replays an outcome.

## Refusals

| Variant | Meaning |
|---|---|
| `Unauthenticated(VerifyError)` | One opaque class: unbound KB, wrong token, empty token, revoked binding |
| `Refused(Refusal)` | The caller's ledger refused budget, or a stop arrived, **during authentication** |
| `Invalid(ValidationError)` | The protocol syntax gate refused the envelope or payload |
| `KbMismatch { expected, declared }` | The envelope named a different KB than the target store |
| `UnsupportedExpectedVersion { expected: u64 }` | A whole-KB pin this command set cannot honour, carried as a validated scalar |
| `MalformedField { field }` | A key, revision, or narrowed integer did not convert |
| `UnsupportedShape { payload }` | The oneof selected nothing this adapter implements |
| `Claims(CommandError)` | Typed claim verdict |
| `Concept(ConceptTextError)` | Typed concept-text verdict |
| `Session(SessionError)` | Typed session verdict |
| `Frontier(FrontierError)` | Typed frontier verdict |

`Refused` is deliberately distinct from `Unauthenticated`: reporting a real budget refusal as
an authentication failure would send the caller to fix credentials when the answer is "no
capacity". A genuine `Unknown` is **not** an error and never appears in this enum.

## What this slice does not do

* **No HTTP, RPC, or queue.** No route table, no framing, no listener.
* **No response encoding.** `HandleOutcome` is returned; building `CommitCommandResponse`,
  `ReviewProposalResponse` and friends from it is a later, separate concern.
* **No decode-bound claim.** Nothing here proves an ingress decoder is bounded.
* **No deadline enforcement.** The envelope's `deadline_ms` is not turned into a timer. This
  library has no clock and no cancellation source; the outer transport must tighten the
  caller's deadline and supply the control hook through `sage_db_types::query::QueryControl`.
  `docs/design/server.md` states the same requirement.
* **No process-safe write queue.** Writes are serialised by the single `&mut Store` borrow.
  Two processes must not hold one KB; the store's lock is the real guard.

## Verification

`crates/store-tests/tests/p4_commands.rs` drives all eight commands through the real
`CredentialVerifier`, the real protocol validation, and a real `Store` on the model filesystem.
For all four commit commands it reads the record back through the kernel's own
`frontier::read_frontier` / `session::read_session_checkpoint`, asserting the committed refs in
caller order, the text/content bytes, priority, status, `session_no`, the ref naming those bytes,
the audit principal, the post-revise revision, and that record and blob share one `ViewBasis`.
Mutation-checked: silently zeroing a revise priority, or emptying a checkpoint's content, fails
the suite — the counter alone could not distinguish either.

It covers: every command's committed outcome; request identity is delegated to the canonical kernel path (these tests do not independently assert persisted request digests or result payloads); an unauthenticated
request that touches no filesystem operation; a wrong-KB token failing opaquely; a missing
capability surfacing as the kernel's `Auth` refusal rather than an auth bypass; `expect_version`
refused with zero mutations; a zero revision and a foreign record kind refused as caller
errors; an extra review read pair refused; a mismatched wire family refused by the kernel; an
exact retry returning `AlreadyCommitted` with the head unchanged; and a starved ledger surfacing
a real `BudgetError` in `Entries`.
