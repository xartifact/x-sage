# P2 operation registry — Claim edits and session checkpoints (draft v1)

This is the first business-write slice, under [system.md](system.md) P2. It does
not implement every record lifecycle, graph mutation, a network protocol or a
real-KB activation. Format remains draft major 0. The P1 request envelope stays
unchanged; operation semantics belong here, per [storage-digests §4.7](storage-digests.md).

## Operations and exact payload bytes

Every command is one operation in the canonical P1 `RequestFrame`, version 1.
All integers below are little endian, with no padding. Strings use a `u32` byte
length followed by exactly those UTF-8 bytes. No text is trimmed or normalized.
`RecordKey = kind:u16 | id:u128`, with canonical record-kind/id bounds.

| op_kind | op_version | Meaning | Payload |
|---|---|---|---|
| 1 | 1 | SubmitClaimEdit | subject RecordKey (Term), claim RecordKey (Claim), text str, declared_author str |
| 2 | 1 | ReviewClaimEdit | proposal RecordKey (Proposal), decision u16 (0 approve, 1 reject), note str |
| 9 | 1 | NativeImportStep | package_ref [32], source_digest [32], stage u16 (0 Prepare..4 Activate), batch_index u32, batch_count u32, first_event u32, event_count u32, import_session_id str |
| 10 | 1 | SubmitRelationAssert | from RecordKey (Term), target RecordKey (Term), relation u16, confidence u16, declaration_order u32, declared_author str, reason str |
| 11 | 1 | ReviewRelationAssert | proposal RecordKey (Proposal), decision u16 (0 approve, 1 reject), note str |
| 12 | 1 | SubmitTermCreate | slug str, atom str, body str, layer u8, confidence u16, source u16, domains list of str, triggers list of str, declared_author str, reason str |
| 13 | 1 | ReviewTermCreate | proposal RecordKey (Proposal), decision u16 (0 approve, 1 reject), note str |
| 15 | 1 | ReplaceFrontierDocument | expected KB metadata revision u64 LE, raw SHA-256 of the exact replacement document (32 bytes) |


`text` and `note` are nonempty. Empty declared authors remain empty; the executor
must not invent authorship. The author and note must also fit the canonical
persisted-string limits (declared-author attribution and reason respectively) and cannot contain
NUL. The text becomes a blob, so the reason-string ceiling does not limit its
length. Each complete payload must fit P1 `MAX_PAYLOAD` **before allocation**;
new payload encoders and decoders use the caller's actual shared ledger.
Unknown operation/version pairs are unsupported semantics, not guessed events.

Submit's read set is exactly the current subject Term and target Claim revisions,
in canonical order. Claim.concept must equal the subject; the Term must be active
and the Claim accepted. Submission validates those caller revisions; it does not
silently replace a stale caller baseline with the latest revisions.

Review's read set is exactly the Proposal and the revision the reviewer saw.
Neither command uses a whole-KB expected version. The new P2 namespace is separate
from ProposalKind values and stored event tags.

For op 12, each list is a u32 count followed by strings in authored order.
The submission read set is empty. The author does not supply a Term key.
A durable submission advances both allocation marks and records one pending
proposal whose subject is the reserved Term. A novel approval checks that the
key and slug are absent and validates the complete graph under committed rules.
Slug or key conflict closes the proposal as conflicted. A graph violation
leaves it pending so a reviewer may still reject it. The reserved keys for
rejected and conflicted proposals are never reused.

## Submission

Authorization requires `propose` on this KB. The authenticated principal comes
from trusted `Grants`; `declared_author` is a separate literal source field.
Trusted executor metadata supplies request_id, transaction id and commit time.
Transaction id/time never enter the canonical request digest.

After request framing/hash and idempotency lookup, the serial writer checks the
actual current Term/Claim, then allocates `ProposalId` from the real current high
water mark plus one. Overflow or an existing identity is an error, never wrapping,
reuse or a scan for a convenient id. Allocation and Pending Proposal publication
are one Store transaction. The proposed edit does not change active Claim text.

The Proposal stores the complete canonical submit request frame as its payload
blob, exact caller baselines, original declared author and authenticated audit.
A restart must preserve that Pending record and its bytes.

## Review

Authorization requires `review`, with no implied capability inheritance. Exact
idempotency precedes pending/revision/baseline checks. A novel review reads the
stored Proposal and verifies the expected Proposal revision and Pending status.

Approve authenticates the actual committed payload bytes, decodes the P1 frame
and registered operation, and verifies its subject, author and baselines against
the stored Proposal. It derives dependencies from that payload; there is no
caller-supplied `required` subset on this writer API.

- Stale subject/Claim revisions: commit Proposal=Conflicted plus the outcome;
  do not change Claim content. This check precedes current lifecycle admission.
- Current revisions: validate active subject/accepted Claim, then commit the
  revised Claim and Proposal=Approved in **one** transaction.
- Reject: commit Proposal=Rejected with no Claim modification.

A decided Proposal is never newly applied again. Review increments Proposal
revision; an approved text edit increments Claim revision. Original creation
time, declared author, legacy metadata and advisory record data are preserved;
new audit names the authenticated reviewer and carries its time/reason. Only
Claim text and revision/audit change, so this slice changes no graph fields.
Other record/graph lifecycles remain unsupported by this command family.

Ordinary commands never scan other Pending baselines. A stale unrelated proposal
must not block a legal command. Historical Claim revisions and old text blobs
remain committed history; existing evidence targets are not rebound to new text.

## Outcomes and failure boundaries

The idempotency key remains `(KB, authenticated principal, request_id)`. The actual
P1 request-domain digest covers operation bytes/read set. Same key+digest returns
the original shared outcome/version with no mutating seam calls, even after the
proposal/baselines change. Different payload returns an idempotency mismatch.

The transaction result uses the exact [§4.2](storage-digests.md) result payload:
kind 0, commit version, sorted affected RevPairs and sorted allocation counters.
A Rejected/Conflicted **Proposal record** is still a committed transaction; it
never introduces a forbidden `rejected`/`conflict` wire result_kind. Invalid
uncommitted commands may return a typed error without an outcome.

All record events use Store's canonical typed preparation bridge; Kernel does
not serialize a second copy of persistent records. Rules and blob lengths come
from the same actual durable Store basis. Every referenced blob has a genuine
Provide/Existing declaration; no unknown-length zero placeholders.

The same LeaseLedger spans request framing, payload traversal, fallible record
copy/preparation, real blob reads and Store commit. Confirmed committed blob
loss/hash damage invalidates normal Store serving. Unsupported semantics,
configured ceilings, budget, stop, filesystem transport and internal defects
remain separate. No blanket restore-backup error for newly generated bytes.

Store owns persistence/Unknown/fencing. A command does not clear a fence or
replay itself after Unknown; the caller explicitly resolves before retrying.
Unacknowledged recovery must contain the old whole state or the new whole state,
never an approved Proposal with unchanged content or content without its decision.

## Remaining readiness requirements

This slice needs existing Terms/Claims. Full creation/retirement, other proposal
families, evidence/reference policies, graph validation, P3 query, P4 authenticated
service and P5 import/activation remain separate work. The OS platform durability
policy is still conservative; modeled recovery is not device power-loss evidence.


## Auxiliary session checkpoints (operations 3–4, version 1)

These are user reasoning-session records, distinct from storage checkpoints.
They use `write_aux`, never `propose`/`review` or implied admin permissions. Reading
requires `read` separately. Every operation still uses the ONE canonical P1 request
frame, real request-domain digest and durable outcome index.

| op_kind | Payload (little endian) | Exact read set |
|---|---|---|
| 3 CreateSessionCheckpoint | concept RecordKey; session_no u32; content str | current Term revision |
| 4 ReviseSessionCheckpoint | checkpoint RecordKey; concept RecordKey; session_no u32; content str | current Term and checkpoint revisions, sorted by RecordKey |

`str` is u32 byte count followed by exact UTF-8. `session_no` is at least1; content
may be empty or contain Unicode/NUL and is never trimmed, parsed or normalized.
The complete operation payload must fit canonical MAX_PAYLOAD before allocation.
Neither operation uses whole-KB expected_version. A stale caller read set refuses
before a new commit; these commands do not scan or modify unrelated proposals.

Create requires an existing active Term and allocates a new SessionCheckpointId
from the actual current high-water mark, checking overflow and actual absence.
Checkpoint128, allocation mark8 and outcome192 commit atomically. The record key
is its identity: concept/session ordinal are not a second uniqueness authority.
There is no highest-session scan or implicit last-writer-wins pair lookup.

Revise uses exact Term+checkpoint CAS and requires the concept/session ordinal to
match that checkpoint's immutable fields. Its full after-image preserves creation
time, reason, extensions and advisory changed-fields; revision, content_ref,
current recorded_by/actor_origin and updated_at change. Content and result blobs
are genuinely provided or reused under verified lengths. Old revisions and old
content remain reachable committed history.

Exact retries return the original outcome/version before current/stale checks;
a changed canonical payload under the same principal/request_id is an idempotency
mismatch. Unknown is the actual Store outcome and leaves normal serving gated
until resolution, never a fabricated success or automatic replay.

`read_session_checkpoint` checks `read`, looks up the real full record, shares its existing retained record owner and reads the actual
hash-verified content through a retained CommittedBlob owner. Found record+content
share the SAME durable basis; a proven absent key carries that basis too. Read
ceiling, caller refusal, transport and confirmed committed damage retain distinct
errors. This is an ID-based read, not the future generic query service.

## Budgeted identified-record traversal

`Store::scan_identified_records` lets graph/query assembly visit full current
numeric-key records under one immutable Store borrow and one durable basis.
Retired/retracted records are included for the caller to classify. Natural-key
metadata is explicitly outside this API; rules use commit_basis instead.

Each current-table step and retained-owner comparison is checkpointed/charged on
the caller ledger. It allocates no copied record table. A visitor refusal is
preserved; callers discard working output after any error. An interrupted scan
never reports a successful complete/absent result. Unknown/Unverified authority
cannot serve a stale fold as current data. This is a Store read primitive, not
full P3 graph/query acceptance.


Owned current reads use `Store::record_owned`, a refcounted alias of the SAME
record owner used for historical images. They use the same physical bounded
lookup as borrowed `record`, not a deep clone followed by a late charge. Holding
the image beyond the Store or attempt facade keeps its actual record/payload
reservations alive. SessionRead carries that retained image and a separately
retained content owner; neither field can shed its reservation into a bare record
or Vec. `reserved_bytes` reports claim bytes only, not allocator/RSS extents.

## Administrative import isolation

`Store::import_readiness` scans the actual latest committed ImportSession rows on one
durable basis using the caller ledger. An unfinished marker denies new ordinary
claim/session commands and session reads as `ImportInProgress`, including after reopen.
Exact committed command retries still answer outcome-first. Physical Store access remains
available to the separately authorized administrative importer and verifier; this gate
does not authenticate an Admin or prove that completion was validated. The future import
writer must validate and verify before setting the completion marker.

The typed preparation bridge additionally delegates Term, Assertion, Asset, Frontier,
KbMetadata and ImportSession to their existing canonical family codecs. Its Rules verifier
is a direct alias of the canonical parser/hash/support check; it consumes actual committed
blob bytes. Neither preparation nor the import marker is business or migration acceptance.


## Auxiliary Frontier operations (5/6, draft v1 continuation)

This continuation uses the unchanged P1 frame and canonical FrontierEntryChanged
event112. It changes auxiliary notes only, never graph facts or proposal review.

| op_kind | op_version | Payload, little endian without padding |
|---|---|---|
| 5 CreateFrontier | 1 | refs_count:u32; concept_refs[RecordKey18]; text:str; priority:u16; status:u16 |
| 6 ReviseFrontier | 1 | frontier:RecordKey18; then the same complete state |

References are ascending unique Term keys, at most64, including an empty list.
Priority is0 high/1 medium/2 low; status0 open/1 resolved. Text may be empty and is
opaque UTF-8 blob content, preserving Unicode/NUL/whitespace. Full operation payload
must fit MAX_PAYLOAD before allocation. Invalid order is refused, never repaired
by sorting or deduplication. Create's exact read set names the listed current active
Terms; revise names the Frontier plus only its NEW listed Terms, in canonical order.
Removed references require no current Term baseline.

Write requires write_aux; read requires read. Idempotency is outcome-first,
then fence/import gate, real durable basis, object CAS and active-reference checks.
Creation allocates FrontierEntryId = actual HWM +1, checks arithmetic and confirmed
key absence, then publishes record+allocation+outcome atomically. Revision preserves
creation audit/extensions/changed_fields, sets authenticated updater+updated_at,
and retains optional reason without inventing one. All revised fields are explicit.
Old text remains committed history. Unknown stays unknown.

## Existing-Term text proposals (7/8, draft v1 continuation)

| op_kind | op_version | Payload, little endian without padding |
|---|---|---|
| 7 SubmitConceptText | 1 | subject:RecordKey18(Term); field_mask:u8; atom:str iff bit0; body:str iff bit1; declared_author:str |
| 7 SubmitConceptText | 2 | All v1 fields followed by reason:str |
| 8 ReviewConceptText | 1 | proposal:RecordKey18(Proposal); decision:u16; note:str |

A missing reason keeps the v1 request identity. A present reason uses v2, preserves an
explicit empty string, and obeys the canonical text byte ceiling and NUL rule. The pending
proposal records the reason. After review, the original reason remains in the hash-bound
submission while the decision records the reviewer’s note separately.

The mask is exactly1,2 or3; it distinguishes an absent edit from an explicitly empty
body. Atom is nonempty/no-NUL and fits canonical MAX_ATOM_BYTES(800) and the product
bound of200 UTF-16 code units, matching existing TypeScript validators. The byte
guard precedes the allocation-free UTF-16 count;100 astral characters occupy200
units. The format ceiling remains800, and no normalization occurs. Body may be
empty/Unicode/NUL. Author remains separate literal attribution, including empty.
The whole payload fits MAX_PAYLOAD before allocation. Propose and Review are exact,
non-inheriting capabilities. Submit pins exactly the Term; review pins exactly the
Proposal and derives the Term dependency from the authenticated stored request frame.

Outcome lookup precedes every state/baseline/lifecycle check. Reject publishes only
ProposalRejected+outcome. A stale Term baseline publishes ProposalConflicted+outcome;
an unrelated pending proposal is never scanned as a blocker. A current active Term
publishes its full revised after-image, Approved Proposal and outcome atomically.
Only selected atom/body plus revision/new reviewer/reason/updated_at change; original
creation audit, slug, layer, source, domains, confidence, triggers, lifecycle, legacy
fields, extensions and changed_fields survive. A revision>=2 record is copied with
the canonical Revised event rather than forged as a Created image. Text-only changes
do not change the canonical active graph digest.

The unchanged body is declared Existing using actual verified bytes and length.
Review receives a separate finite max_existing_body_len resource policy from the
executor; it is not the request-frame ceiling and is excluded from the idempotency
digest. It is consulted after outcome lookup, so an exact committed retry still
returns its original outcome when a later read ceiling is smaller.

The current Term/body reader is a separate low-level read seam: existing RecordImage
and leased CommittedBlob with the same complete CURRENT-named manifest-hash basis.
It is not an editable document projection or relationship-set baseline. The physical
administrative all-current scanner includes all13 record families on one durable
basis; it excludes non-record outcomes/HWM/import/rules-history bookkeeping. Runtime
family coverage is bounded to the tested families, not inferred from that type count.


## New authored relation proposals (operations10/11, version1)

These create independent Assertion supports. They do not replace a relationship set,
revise/retract an assertion or bypass review. Submit needs Propose on the actual KB,
pins exactly the two active endpoint Terms in sorted key order, and allocates a
Pending Proposal from the real ProposalId mark with collision/exhaustion refusal.
The complete canonical request frame is the stored payload; principal/tx/time are
trusted metadata, while declared_author is literal provenance (empty legal, canonical
MAX_DECLARED_AUTHOR800 bytes, NUL refused). Reason/note remain nonempty bounded text.

Review needs Review separately and pins exactly the Proposal revision. Exact retry
precedes fence/import/lifecycle/current-baseline checks. Reject closes only the
proposal after its payload hash read; it need not decode a damaged intent. Approval
authenticates and decodes that stored frame, checks operation/kind/subject/author and
both baselines, and derives endpoints from it. A moved endpoint commits Conflicted
without an assertion. Current baselines lead to validation of the ENTIRE current
active graph plus candidate through canonical infer::validate_candidate under the
committed profile; a rule violation writes nothing and leaves Pending. A valid
assertion, Approved proposal, real id mark and outcome publish atomically.

Candidate graph_digest uses the canonical projection and its label is the checked
next version. P0 validation Budget and caller QueryControl are explicit arguments,
distinct from the single LeaseLedger covering reads/copies/request/commit. No
unified-currency claim. Candidate sorting charges/checkpoints every comparison;
the current insertion sort is O(n²), not a scalability claim. No assertion/actor
identity, allocator figure or input normalization is invented.

Canonical validator rule labels are V1–V6 (V6 has two cycle variants); older task
prose saying V1–V7 was inaccurate. This bounded family is not full P2 or a service.


### Legacy Frontier document replacement

`ReplaceFrontierDocument` requires `WriteAux` on the actual KB. It binds the
expected metadata revision and the raw SHA-256 of the supplied document in the
canonical request payload. The document is a blob, with the metadata reference
and request outcome committed in one transaction. An exact retry returns the
original durable outcome. The operation allocates no typed Frontier row or ID.
Reads require `Read` and return the metadata revision with the hash-verified
document on the same durable basis, if one has been established.

The legacy product adapter interprets Markdown and supplies the document already
assembled according to its legacy append and duplicate rules. Blob admission
uses the caller's ledger. Transport size limits are independent of the operation.


## Public checkpoint slot upsert

Operation 16 version 1 stores slug (length-prefixed UTF-8), session ordinal (u64 LE) and
the 32-byte raw content digest. The frame pins the observed whole-KB version and has no
record read set. WriteAux is bound to the actual KB. Exact request retries return the
existing outcome before the version check.

The current durable slot is selected by slug and ordinal, including a slug that has no
Term. Previously imported native slots remain readable and an upsert retains the existing
record identity. A new slot advances the checkpoint identity allocation. The checkpoint,
content blob, allocation when needed, and outcome are written in one transaction. An
unknown outcome stays unresolved until the original request is checked.

Bun selects the next default session from the latest numeric slot and submits the
observation's version with the opaque state. A definite version refusal lets Bun reread
and recompute. The private raw frame admits 256 KiB of checkpoint data without changing
the ordinary JSON request and response ceilings.


## Legacy proposal filenames and approvals

Private compatibility submissions bind the public filename to the authenticated request.
Operation 10 version 2 carries a keyed relation intent. Version 3 carries the actual source
key and the authored target slug for a relation proposal awaiting a target. Only the source
has a revision baseline. Review resolves the slug on the observed publication. Missing
targets leave the pending intent intact.
Operation 7 version 3 carries a text intent with its quoted atom or expected public document
digest. Version 4 carries a body extent and SHA-256 in the canonical request payload. The
committed proposal owns the frame and the verified body bytes under its single payload
reference. Approval checks the body identity before publishing the Term revision. Aliases choose the
latest committed matching proposal. File provenance already present on imports remains
readable through the separate import metadata.

Compatibility approvals use the observed publication and the effective filename.
Operation 11 version 2 applies the edge or approves an already authored triple without
changing its confidence. Operation 8 version 2 compares the quoted atom or document
digest and preserves the pending proposal on a definite conflict. The Bun batch submits
individual approvals in edge, atom, body order and reports the completed decisions.
