# Relation-assert commands — submit and review a new authored edge

Status: **bounded P2 continuation**. Not inference, not a phase acceptance, and not a
cutover/readiness statement.

Code: `sage-db/crates/kernel/src/relations.rs`; tests
`sage-db/crates/store-tests/tests/p2_relations.rs`.

## 1. What an approval means

A submitter proposes that `from --relation--> target` should exist. On approval the store gains a
**new, independent** [`AssertionRecord`] — never a revision, never a replacement. Two people
asserting the same triple are two supports with two identities, because that is what they are.

This batch does **not** revise, retract or replace assertions, and does not touch any existing
edge.

## 2. Operations and exact payload bytes

Both are one canonical P1 `RequestFrame` operation, little-endian, no padding.

**op10 / v1 `SubmitRelationAssert`**

| field | width |
|---|---|
| `from` | `kind:u16 | id:u128` = 18 |
| `target` | 18 |
| `relation` | `u16` |
| `confidence` | `u16` |
| `declaration_order` | `u32` |
| `declared_author` | `u32` length + UTF-8 bytes |
| `reason` | `u32` length + UTF-8 bytes |

**op11 / v1 `ReviewRelationAssert`**

| field | width |
|---|---|
| `proposal` | 18 |
| `decision` | `u16` (0 approve, 1 reject) |
| `note` | `u32` length + UTF-8 bytes |

`RecordKind::Proposal` is discriminant **7** and `Term` is **1**; both are asserted as literal bytes
in the tests, not merely round-tripped. `RelationId` decodes through `from_u8` and `Confidence`
through `from_u16`, so an unregistered discriminant is refused rather than cast or defaulted.

## 3. Submit

Order is deliberate:

1. `Propose` authorized against the store's **actual** KB;
2. request metadata and payload codec checked;
3. the canonical frame and its request-domain digest;
4. **outcome-first**: an exact retry returns the original outcome; a different digest for a
   recorded request is an idempotency mismatch;
5. write fence, then the persisted import gate;
6. both endpoints read: present, active, and at the revision the caller pinned;
7. `ProposalId` allocated from the committed watermark, refusing exhaustion or collision;
8. one transaction: Pending proposal (`kind = RelationAssert`, `subject = from`,
   `base_revisions` = the caller's two endpoints), `IdAllocation`, outcome, payload blob.

The stored payload **is** the request frame, so the reviewer reads exactly what the author wrote.
`declared_author` is stored as attribution and is never inferred from the authenticated principal.

The read set must be exactly the two endpoint `RevPair`s, ascending by key and unique — not a
whole-KB pin. Nothing is normalized, sorted or deduplicated from the caller's input.

## 4. Review

`Review` is required and `Propose` does not imply it.

**Approval** derives the candidate **only** from the stored payload: one leased, hash-verified read
of the frame, decoded by the canonical P1 decoder, with the operation kind/version,
`RelationAssert` kind, subject/`from`, declared author and the two stored baselines all required to
agree.

**Rejection does not decode the frame.** Per `kernel-operations`, a rejection only closes the
proposal (`Rejected`) and is a real commit; it needs no candidate, so it deliberately skips the
payload-intent cross-checks. That is intentional: a reviewer must be able to close a proposal whose
payload is subtly inconsistent, which requiring the full integrity gate would prevent. The payload
blob is still read and hash-verified; only the *intent* decode and cross-check are approval-only.
So "every review decodes the frame" would be false, and the approval path's integrity checks are not
a statement about rejections.

| situation | outcome |
|---|---|
| reject decision (no frame decode) | committed `Rejected`, no assertion |
| either endpoint's baseline moved | committed `Conflicted`, no assertion |
| reject decision | committed `Rejected`, no assertion |
| baselines current, candidate violates a rule | **typed refusal, no write**, proposal stays `Pending` |
| baselines current, candidate valid | atomic `Assertion` + `Approved` proposal + `IdAllocation` + outcome |
| assertion id exhausted or occupied | typed refusal, never wrap/reuse/search |

A validation refusal is deliberately **not** `Conflicted`: no baseline moved. Reporting it as a
conflict would tell the submitter to re-pin a baseline that is still current.

## 5. Validation is the canonical validator

The entire current active graph — collected from the store's own identified-record scan — plus the
proposed assertion is validated by `sage_db_infer::validate_candidate` under the verified committed
rule profile. No relation table, layer rule or V-code is copied into this module.

Because the rules come from `infer`, `formal-v1` and `legacy-v1` behave differently for the same
input: a definition target must sit strictly below its source under formal, so `is-a` L2→L2 is
refused while L2→L1 is accepted. Both behaviours are the validator's, not this writer's.

The candidate graph's `SnapshotId` uses the real canonical `graph_digest`, next version and current
rules — never an invented zero digest.

The candidate's term and assertion arrays are ordered by an **attended insertion sort**, which
charges one entry and one control checkpoint **per comparison**. It is therefore
`O(n²)` comparisons rather than the `O(n log n)` a std sort would do: for the active
term/assertion counts of a KB that is fine, but the cost is quadratic in KB size and a large KB
would feel it in `max_work_entries`. This is deliberate — a std comparator cannot charge or be
interrupted — and it is **not** a performance claim.

Validation takes its own P0 `Budget` and the **caller's control** explicitly. `LeaseLedger` is not
`QueryControl`, so the control is a separate argument: there is no hidden default and no
per-container allowance. The two currencies are distinct and this module does not claim otherwise.

## 6. Resources

One `LeaseLedger` spans authorization, the payload, record and blob reads, the candidate arrays, the
digest and the commit. Every retained table is admitted before it grows, and the error path is
heap-free: `RelationAssertError`/`RelationAssertInputError` carry only scalars and borrowed slices,
never a `Box` or a `String`.

Error classes stay distinct: authorization, input, payload, idempotency, fence, import gate,
incomplete authority, validation (`QueryError`), and real store/ledger refusals.

## 7. Evidence

Nineteen tests on a real model Store through the public commands: pending submission with no
assertion until approval; approval after-image with declared vs authenticated identity separate; a
duplicate triple as an independent support; exact retry before review and a digest mismatch; reject;
stale endpoint → committed `Conflicted` with no graph mutation; subject/payload disagreement refused;
omitted and misordered endpoint baselines refused; a rule-violating candidate refused with the
proposal left `Pending`; two individually valid proposals whose successive approval would form a
cycle, where the second is refused by the fresh candidate; capability separation; a real byte
refusal and a real stop; the unfinished-import fence; and an `Unknown` review followed by a real
reopen that must never show a half approval.

### Coverage, stated exactly

Exercised: literal payload bytes; pending-without-assertion; approval after-image with separated
identities; duplicate triple as independent support; exact retry in-session **and after a real
reopen**; reject; stale endpoint → `Conflicted`; forged subject/payload mismatch; omitted/misordered
baselines; rule violation (V3); reachable cycle (V6); **Legacy same-layer accepted vs formal
refused**; **V4 distinct-target cardinality, plus the same-target duplicate that is NOT V4**;
**V5 uncovered L3**, reachable because the seeded graph may contain an active L3 term with no
`instantiates` edge; **physical `AssertionIdCollision`, `AssertionIdExhausted` and
`ProposalIdCollision`, each with unchanged head**; `ProposalIdExhausted`; **two-reviewer CAS on one
proposal with an unrelated pending left untouched and still approvable**; capability separation;
real byte refusal and stop; import gate; and `Unknown` across every sampled crash world.

**Not exercised, and why:** V1 (unknown endpoint) and V2 (self relation) are impossible to reach
through this command because `BaseGraph` already rejects dangling endpoints and self references; a
test that cannot fail is not evidence, so none is written. The candidate snapshot *version* label is
behaviour-neutral (`validate_candidate` never reads it) and is therefore correct by inspection, not
by a failing test.

Mutations verified to fail the suite: **skipping candidate validation** (the
rule-violation and cycle tests fail) and **weakening the subject equality** (the forged-proposal
test approves an edge the proposal never named).

## 8. Limits

* Platform durability is not claimed, and `Unknown` is exercised as the ambiguous attempt it is.
* No phase, P4, cutover, activation or readiness claim.
* Test-local allocations are not allocator or RSS evidence.
* Assertions are only created here; revision and retraction lifecycles remain out of scope.

## Declared attribution boundary

`declared_author` is literal provenance, including an empty string. Its canonical
field ceiling is `MAX_DECLARED_AUTHOR` (800 UTF-8 bytes), distinct from the
smaller authenticated identity ceiling. Submission and stored-intent decoding
use the same bound; NUL is refused. Approval preserves attribution and records
the authenticated reviewer separately. Root corrected the earlier nonempty /
200-byte admission in a new candidate, with literal zero/800 length fields,
801-byte rejection on both codec paths and real submit/approve boundary cases.
