# sage-db implementation baseline

Baseline ID: `p1-parallel-01` (2026-11-17). Coordinator: Herdr `t5n`.
This is a current navigation/ownership index, not an additional format authority.
User-approved P1 scope and later coordinator releases supersede old evidence-only
task messages. x-cart was explicitly declined for this work.

## Effective requirements and evidence

- Normative design: [system](system.md), [storage](storage.md),
  [data model](data-model.md), [format](storage-format.md),
  [digests](storage-digests.md). Report contradictions; do not silently rewrite rules.
- Accepted immutable format contract: `/tmp/sage-db-p1.2yjLG4/format-contract/DELIVERY-10`.
  Its compiled stub is historical contract evidence, not the production codec.
- Accepted store contract: `store-contract-S2-corrected` plus `store-contract-S2-appendix`
  under `/tmp/sage-db-p1.2yjLG4`; identities and scope in
  `/tmp/sage-db-p1.2yjLG4/P1-1-ACCEPTANCE.md`.
- Accepted P1-3 foundation: `/tmp/t62-p1-3-corrected-v4/store` with documented
  comment/diagnostic-only live erratum. Scope/identity:
  `/tmp/sage-db-p1.2yjLG4/P1-3-ACCEPTANCE.md`.
- P1-3 clarification (appended, not a re-freeze): `fs` `91bad418` already accepts
  `publish_no_overwrite` with the canonical `ledger` argument for charged staged/existing
  verification, which the live signature confirms. Bind that production signature; no
  historical S2 adapter and no new permission. Three durability concepts stay distinct and
  are not a rename of one another: `DurabilityBasis::{Documented,NotClaimed}` is the
  foundation's **platform-evidence descriptor**; `product_eligible()` is a capability
  **eligibility predicate**; and `Durability::Durable` in the accepted S2 FS-API is the
  later **commit-result** type, which is not implemented yet. Both enums are constructible
  and neither is an unforgeable token — that does not make them the same type, and the
  public-enum warning concerns the contract result and future output invariant, not this
  descriptor.
- Current production types live in `sage-db/crates/types/src/{ledger,persist}.rs`;
  ControlStop is the existing query type. No copied evidence vendors in production.
  Actual implementations under `sage-db/crates/format/src` supersede stub bodies.
- The `/tmp` paths are session evidence, not runtime/test dependencies. Bundle any
  required fixture with source hashes; tests must not skip if a `/tmp` path is absent.

Current implementation includes the bounded P2 Claim-edit and session continuations,
source-archive tooling, a checked x99 platform backend and persisted import isolation;
P1 captures below remain immutable accepted origins, not byte-identical descriptions
of the later live workspace. P2 is not phase-accepted. Its active operation contract
is [kernel-operations](kernel-operations.md), with readiness in
[sage-db-readiness](sage-db-readiness.md).


## Current bounded continuation: relation proposals (2026-11-20)

Root adopted9 paths (6 existing +3 new;619 outside existing payloads unchanged)
from `final-bounded-relation-proposals-20261120T014228Z`: MANIFEST99ec71a9/628,
EVIDENCEf3e63cac/802, CAPTURE89b314aa. Exact local CI passed Rust1675/0 over114
summaries (12 explicitly ignored), Bun476/0, contract55/0 (overlaps Bun), strict
docs/toolsTS0. The same628 source verified before/after on x99 and passed Rust1675/0,
policy6/0, actual-file/process5/0 and clippy/fmt/strictdocs0. All73 OS artifact files
matched remote and local hashes. The first log transfer exited255; a separate transfer
recovered the unchanged12 logs with remote-before/after and local hash equality, without
rerunning tests. The failed transfer remains recorded.

Adopted op10/11 submit and review authored RelationAssert proposals, with payload-derived
endpoints, exact revision baselines, committed-profile candidate validation and atomic
assertion/proposal/outcome writes. Rejection closes only the proposal; approval validates
the intent. Declared attribution permits empty text and the canonical800-byte ceiling,
separately from authenticated reviewer identity. Kernel now depends on types/store/infer;
the exact dependency contract includes that permitted forward edge. Earlier adopted
executor, catalog, Requires and Navigation payloads remain unchanged.

The graph validator reports V1–V6. Normative V7 lifecycle obligations remain owned by
the kernel; the absence of Term creation/retirement commands does not waive them.
P2 remains incomplete. JSON ingress, backup/restore and the Rust runtime are separate
candidates, not adopted by this capture. A read-only source recheck at
2026-11-20T01:37:46Z found73 files/335915 bytes, zero preparation drift and no
knowledge.db. Real x99 Markdown41 remains authority; no production write or activation
occurred. Only the three navigation documents change after this immutable capture.

## Prior bounded continuation: authored navigation (2026-11-20)

Root adopted4 paths (1 existing +3 new;621 outside existing payloads unchanged)
from `final-bounded-authored-navigation-20261119T234756Z`: MANIFESTf1200d5e/625,
EVIDENCEbbf25419/959, CAPTURE1bee02ed. Exact local CI passed Rust1640/0 over113
summaries (12 explicitly ignored), Bun476/0, contract55/0 (overlaps Bun), strict
docs/toolsTS0. The same625 source verified before/after on x99 and passed Rust1640/0,
policy6/0, actual-file/process5/0 and clippy/fmt/strictdocs0. All73 OS artifact files
matched remote and local hashes.

Adopted navigation returns the shortest undirected path over active authored assertions,
with original assertion identity/revision/direction and deterministic canonical ties. It is
not a semantic Fact or an inferred relationship. One canonical meter accounts actual
work rows, slot bytes, comparisons and attended reconstruction; budget/stop/allocation/
extent/invariant errors remain distinct. Requires exports and its scoped behavior remain
unchanged through the merge. No JSON ingress or relation-assert candidate is adopted.

A separate622-source private x99 Btrfs operator rehearsal imported the real73-file capture,
reopened, verified all264 whole rows/90 blobs/5 marks, completed, then reopened again.
All287 term/relation queries agreed with the independent reference executor;356 fact rows
had replayable proofs. All41 Requires queries matched reference rows and dependency order
(259 ordered terms in aggregate). This is an operator probe, not old Bun byte parity,
RSS/allocator evidence, full private-overlay CI, power-loss or service availability.

A read-only source recheck at2026-11-19T23:45:52Z still found73 files/335915 bytes and zero
changes versus the preparation capture; the old service remains Markdown41. This is not
the final write-stop freeze. No production KB/service/config write, stage/commit/push/deploy,
phase acceptance or cutover occurred. Index and36 local KB files were unchanged. This
navigation edit changes only three documentation payloads after the immutable capture.

## Prior bounded continuation: native execution, catalog and Requires (2026-11-20)

Root adopted22 paths (11 existing +11 new;600 outside existing payloads unchanged)
from `final-bounded-native-executor-catalog-requires-20261119T230820Z`:
MANIFEST1007aaea/622, EVIDENCEa2ca0d5b/321, CAPTURE91933ee4. Exact local CI passed
Rust1623/0 over112 summaries (12 explicitly ignored), Bun476/0 and contract55/0
(overlaps Bun), strict docs/toolsTS0. The same622 source verified before/after on x99
and passed Rust1623/0, policy6/0, actual-file/process5/0, clippy/fmt/strictdocs0.
All73 files in the five OS test artifact directories matched remote and local hashes.

Adopted: guarded staged native execution with complete current/history value checks,
blob and watermark verification; Read-authorized borrowed catalog with import isolation
and exact slug resolution; one-meter query continuation and complete unfiltered Requires
with dependency ordering, full answer/proofs and total work statistics. These components
have independent scoped source reviews; no phase is declared complete.

A separate591-source private rehearsal used byte-identical RootR7 executor production:
real73-file input,264 whole rows,90 blobs,5 watermarks, two reopens, verification then
completion. Model and actual x99 Btrfs invocations each passed one operator test; the
completed Snapshot has41 terms/173 assertions under LegacyV1. The final receipts bind
actual bytes; earlier construction receipts are explicitly superseded outside frozen
reports. This is not a final production source write-freeze or activation.

Navigation and fallible JSON ingress remain separate held candidates. No Rust listener,
complete REST compatibility, backup/restore protocol, production KB/service/config write,
stage/commit/push/deploy or cutover occurred. Git index and36 local KB files were unchanged.
This navigation edit changes only three documentation payloads after the immutable capture.

## Prior bounded continuation: native preparation and response assembly (2026-11-20)

Root adopted16 paths (7 existing +9 new;595 outside existing inputs unchanged)
from `final-bounded-native-prepare-response-20261119T213824Z`: MANIFEST7916a989/611,
EVIDENCEa9f51307/177, CAPTURE8d4678b1. Local exact CI passed Rust1554/0 over108
summaries (12 explicitly ignored), Bun476/0 and contract55/0 (overlaps Bun), strict
docs/toolsTS0. The same611 source verified before/after on x99 and passed Rust1554/0,
policy6/0, real-file/process5/0 and clippy/fmt/strictdocs0.

Adopted: additive profile-aware Genesis/bootstrap (the old entry remains FormalV1);
canonical response assembly preserving recorded versions, original decision status,
Unknown request identity and real failure classes; self-contained native import
preparation with all73 source files in its required-reference package. Preparation
constructs262 family rows /264 whole-state rows and13 historical proposal restatements.
The real73-file dry-prepare probe passed separately from37 self-contained fixture tests
plus1 explicitly ignored operator probe. This is preparation, not committed import.

Native staged execution remains held for verification and resource findings. No
listener, REST compatibility, full query service, phase acceptance or cutover follows.
Real x99 Markdown41 is still authoritative. No production KB/service/config write,
stage/commit/push/deploy occurred; Git index and36 local KB files remain unchanged.
This navigation edit changes only three documentation payloads after capture.

## Prior bounded continuation: protocol, fact queries and commands (2026-11-20)

Root integrated31 paths (10 existing +21 new;571 outside existing inputs unchanged)
from immutable602-source capture
`/private/tmp/sage-db-p2-sessions-20261119/final-bounded-protocol-query-command-20261119T202537Z`:
MANIFEST `181ae950`/602, EVIDENCE `16c23fb9`/143, CAPTURE `9f4ab131`.
Exact local CI passed Rust1486/0 over103 summaries, Bun475/0 and separate contract54/0
(overlaps Bun), strict documentation0 and toolsTS0. Fresh x99 source verified602/602
before/after, Rust1486/0, policy6/0, real-file Store/process5/0, clippy/fmt/docs0.

Adopted: one generated protocol authority with shared write outcomes, explicit review
families, complete recorded-result presence semantics and canonical shared-ledger validation;
nonzero record revisions remain distinct from legal KB PRE_GENESIS version0. The UTF-16
business limit is advertised optionally, while the kernel owns enforcement. Rust/TS codegen
pins protoc36.0 and ts-proto2.6.1; CI provisions them and requires drift checking. The
canonical borrowed result reader validates both extents before row work. Fact queries own
complete executor answers/proofs, reject unknown targets rather than reporting absence, bind
all snapshot fields, and observe control at the bounded point lookup. The authenticated
application adapter dispatches eight existing kernel commands on one caller ledger, preserves
principal and complete new refs, and refuses unsupported whole-KB version expectations.

No response assembly, listener, HTTP/RPC service, native mapping/executor adoption, phase
acceptance or real-KB activation follows. Mapping and response candidates are held for their
own source findings. Real x99 Markdown41 remains authority; no production KB/service/config
write, stage/commit/push/deploy occurred. Source is exact at capture; this navigation update
changes only three documentation hashes afterwards. Git index and36 local KB files remain
unchanged. Only omp/pi delegation is permitted.

## Prior bounded continuation: text, auxiliary notes and physical scans (2026-11-20)

Root integrated15 paths (6 existing +9 new;566 outside existing inputs unchanged)
from immutable581-source capture
`/private/tmp/sage-db-p2-sessions-20261119/final-bounded-text-frontier-20261119T191342Z`:
MANIFEST `b6fe81bd`/581, EVIDENCE `ec203ed9`/141, CAPTURE `955e62b9`.
Exact-source local CI passed Rust1394/0 across95 ok summaries, Bun473/0 and separate
contract52/0 (overlaps Bun), strict documentation0 and actual-environment toolsTS0.
Fresh x99 copy verified581/581 before/after: Rust1394/0, platform6/0, real-file
Store/process5/0, clippy/fmt/docs0. Bounded source review is NEXT-STAGE-R4-SOURCE-REVIEW.

Adopted: existing-Term atom/body submit/review (operations7/8), including repeated
approvals, full after-image preservation, actual stored-frame validation and finite
unchanged-body read policy; atom command admission matches existing200 UTF-16-unit
validators while the canonical800-byte format ceiling stays unchanged. Frontier
operations5/6 use preflight bounds before allocation, exact CAS and retained body reads;
all sampled Unknown worlds compare whole records/text and real submitted-vs-persisted
request/outcome identity. Physical collision/exhaustion inputs are tested. The current
Term/body read now tests retired rows through restart. Administrative current scanning
serves every record family structurally, including natural singletons; runtime family
coverage is only tags1/2/10/11/12/13, and exact Genesis entry totals are4/10 for the two
scan selections. No non-record bookkeeping becomes a fake record.

This is bounded source adoption, not P2/P3/P4/P5 acceptance, real-KB activation,
major1, device power-loss or RSS/all-input allocation evidence. Real x99 Markdown41
remains authority. Protocol M1 is held for a duplicated resource contract; its R2 and
query/native-mapping work are separate candidates. Source was byte-exact at capture;
this navigation update changes three documentation hashes afterwards. Git index and
36 local KB files remain unchanged; no stage/commit/push/deploy or production write.
Only omp/pi delegation is allowed; agy remains prohibited.

## Prior bounded continuation: snapshot and credentials (2026-11-20)

Root integrated17 paths (8 existing +9 new;555 outside existing inputs unchanged)
from the identified572-file source. Immutable capture:
`/private/tmp/sage-db-p2-sessions-20261119/final-bounded-snapshot-auth-20261119T182743Z`:
MANIFEST `e930a9f0`/572, EVIDENCE `34bf5eb3`/158, CAPTURE `52148846`.
Local exact-source full CI: Rust1303/0 across91 ok summaries, Bun473/0, separate
contract52/0 (overlaps main Bun; never sum into a unique total), strict docs0 and
actual-environment tool TypeScript0. The same572 source verified before/after on x99:
workspace1303/0, policy6/0, real-file Store/process5/0, clippy/fmt/strict docs0.
Source-only reviews remain bounded: Snapshot RootR3/R4, input-planR5 and credentialsR2.

Adopted: a borrowed four-family SnapshotView adapter, pre-admitted slot/input arrays,
one shared metered graph builder, real Rules verification and canonical active-graph
digest recomputation, precise upstream failures and retained view credit. The input-plan
extractor has335 mapping records and138 outstanding native-target requirements; it is
not an importer. The credential library verifies hash-only, per-KB explicit grants under
request/startup ledgers with mature sha2/subtle, but has no HTTP/RPC/config loader or token
issuance. Root additionally fixed context-document limits and audited emitted mapping
counts; deleting the real frontier charge in a private test mutant fails the guard.

No P2/P3/P4/P5 phase acceptance, real-KB switch, major1, device power-loss, RSS/all-input
allocation or P-NFA inference follows. Frontier and all-current administrative scanning
are separate, unadopted continuations. Real x99 Markdown41 remains authority; local22 is
not complete input. This capture was byte-exact **at capture**; this navigation update
changes three documentation hashes afterwards. Git index and36 local KB files stayed
unchanged. No stage/commit/push/deploy or production KB/service/config write occurred.
Only omp/pi delegation is now allowed; agy is prohibited and its terminal was closed.

## Prior bounded continuation (2026-11-20)

Root integrated26 paths (12 existing +14 new;537 outside existing inputs unchanged)
from the identified563-file composition. Immutable capture:
`/private/tmp/sage-db-p2-sessions-20261119/final-p2-sessions-platform-20261119T173227Z`:
MANIFEST `cb7a1735`/563, EVIDENCE `012688c9`/372, CAPTURE `f2602273`;
coherent review `4178c289`, integration `b2c84613`. Local full CI passed Rust1262/0
across88 result summaries, Bun403/0 and the separate contract target51/0 (overlapping,
not unique totals); strict workspace docs passed. The same source on x99 passed the
workspace Rust tests,6 policy qualification cases and5 real-file Store/process cases,
clippy/fmt/strict docs, with563 source hashes verified before/after. This includes the
new load-bearing whole-row/outcome/oracle/root-sync checks, not the held M1 process
claims. Source reviews and corrections remain bounded to their named components.

The capture adopts owner-backed session reads, identified-record scans, canonical typed
preparation bridges, a caller-bounded neutral source archive, the root-checked
linux/x86_64 kernel7.2.11-arch1-* Btrfs backend, and persisted unfinished-import isolation.
No generic Linux capability override, device power-loss/RSS/all-input/P-NFA claim, full
P2/P3/P4/P5 acceptance or real-KB switch follows. Snapshot M1/R2 and the target-mapping
work remain unaccepted and unintegrated. Real x99 Markdown authority is unchanged;
local22 is a stale subset of the real41-concept/173-edge input. The immutable source was
verified byte-exact **at capture**; this navigation edit changes this document afterwards.
Git index and36 local KB files were unchanged by integration. No stage/commit/push/deploy
or production data write was performed.

## Phase status

| Phase | Current status |
|---|---|
| P1-1 contract | Accepted; not a durability proof |
| P1-2 types/format | **ACCEPTED** on final-v2-20261117T103218Z; not a durability proof |
| P1-3 filesystem foundations | Accepted on identified v4 composition |
| P1-4 commit | **RE-ACCEPTED** on corrected `final-p1-4-v2-20261117T224953Z`; MAIN integrated |
| P1-5 recovery | **ACCEPTED + MAIN integrated** on `final-p1-5-20261118T092938Z`; not a durability proof |
| P1-6 independent harness | **ACCEPTED + MAIN test-harness integrated** on `final-p1-6-20261118T152842Z`; format stays draft major0 |
| P2 business records | **IN PROGRESS**; Claim text-edit proposals, auxiliary session CAS/read/retry, retained record/blob reads and import isolation are adopted bounded components; not phase acceptance |
| P3 query | **IN PROGRESS**; four-family SnapshotView and metered base graph adopted; query verbs/search/cache remain incomplete |
| P4 service | **IN PROGRESS**; pure hash-only credential verifier adopted; no running Rust protocol server |
| P5 migration | **IN PROGRESS**; neutral archive and input-plan extraction adopted; native target mapping/import/verify/activation incomplete |

P1-2's required scope — every registered event's codec, public file framing/version/limit
gates, state/graph/bookkeeping digest assembly and independent byte evidence — is what the
accepted composition covers, bounded to its represented inputs and optional fields rather
than exhaustive value coverage. At that P1-2 capture there was no Linux/device evidence.
The later checked x99 Linux/Btrfs backend and process tests below do not establish device
power-loss durability; the generic OS allowlist remains empty. On 2026-11-19 the user authorized further
implementation and a real-KB cutover only if readiness is established. That later
instruction supersedes the earlier P1-only migration hold; it does not establish
readiness. The current decision and missing prerequisites are tracked in
[sage-db-readiness](sage-db-readiness.md). No commit, push or deployment has been
performed. Existing immutable P1 captures remain unchanged.

**P1-2 is accepted** per `/tmp/sage-db-p1.2yjLG4/P1-2-ACCEPTANCE.md` on capture
`final-v2-20261117T103218Z`: manifest `f81d6e72` (250 files, 250 hash entries, 116 fixture
paths, `fixtures/rules-verify` 28), CI log `949366ba` (`CI_EXIT=0`; 941 Rust tests passed,
0 failed; 50 CI-contract and 348 Bun tests), strict rustdoc `3e0e3028` (`RUSTDOC_EXIT=0`).
The maintainer independently confirmed all four named artifact hashes and that the live tree
matches the manifest 250/0/0, so the frozen composition and the working tree are the same
bytes. Final `rules_read` is `dd3a45a1` — a doc-only link fix over `8e3956e2`, with test
`4376f9be` and all 28 accepted blobs unchanged. The independent harness is exactly
`dbf903af` + `72ad0087` (156 tests). Historical note preserved: the first final capture had
valid green CI but omitted the accepted harness, carried one duplicate rules fixture, did not
manifest fixture payloads and failed strict rustdoc; it is historical evidence of an
incomplete composition, **not** a prior phase acceptance. Its temporarily missing top-level
metadata is a separate restoration incident, not a product finding, and the accepted capture
must not be modified. P1-4 is released under `P1-4-IMPLEMENTATION-RELEASE.md`; P1-5 and P1-6
remain held. No durability, Linux/device, deployment or format-major claim follows.

P1-4 evidence status (no phase acceptance implied; P1-5 held). Accepted bounded, each on
its own evidence:

- **Names** now **accepted/live bounded** at `77f718d4` with `names_manifest` `b53c8f90`
  (11/0 author) from source `eb78`, i.e. the reviewed candidate plus authorized hygiene:
  `to_file_name` marked as an unbounded convenience with no ledger, and the unneeded
  `write_into` emitter made private. One canonical emitter on the admitted path,
  `render_name_bounded` admits capacity, checks control, `try_reserve_exact`, then emits; the
  fixed per-variant maximum is the real reserved extent (manifest 58 B incl. 20-digit u64),
  and spare bytes for short versions are reserved memory rather than a misreported filename
  length or RSS claim. `copy_value_bounded` is likewise fallible. No lower caller
  string/list policy. The integration creates a distinct live hash and is not claimed
  byte-identical to the candidate. Grammar/rendered bytes remain the reviewed contract.
- **Projection** delivered `8f901c7a` (`8f`) accepted on independently derived D10 full
  payload bytes (337/592/892 B), not just digests. Integration `f4fe6c44` and dispatch
  `9246094d` are a **separate identity**, accepted on source delegation preserving both
  checkpoints, with test `1c8e9d2d` at 17/0 (t66, then integrator after the test overlay).
- **Shared surface**: canonical carrier `bdf67fa8` and prepared facade `2bcb7b89`, handoff
  v2 bounded accepted — root verified all 10 source/test hashes against live, plus the same
  discriminating entries probes (2 accepts / 2 refuses, 1/0, exit 0). The v1 `2369b0d1`
  failing case (exit 101) stays frozen as the counterexample. Carrier is data, not
  validated/unforgeable input; Store still owns header conversion, validation, publication
  and idempotence.
- **Helper v3** accepted/live on the 277 base per `HELPER-V3-REVIEW.md`, at prepared
  `6ff5c1db` and encode `ce7e5244`, with no lower caller string/list policy. This is a
  distinct identity from the earlier `2bcb7b89` facade.
- **Model v3** bounded accepted/live on exactly three files — `e6261a70` / `94fafdeb` /
  `a9b24891` — after a doc-only integration. Root `MODEL-V3-ACCEPTANCE` `33caddd5` accepted
  the behaviour, and `MODEL-V3-INTEGRATION` `562f7651` records the post-hygiene identity:
  non-comment lines match approved `572b6770` / `91adb88a` / `a9b24891`, `crash_states` is
  byte-identical. The author's `replace_current` run was **filtered: 5/0, not the full 179**
  inventory, and `p14_positive` is **not** integrated.
- **Fold** root-owned isolated correction `src/fold.rs` `56aca632` / `tests/fold_state.rs`
  `17eeb64a`, delivery `delivery-fold-root-56aca632` (manifest `7102709a`, 10 entries), is
  **bounded component-accepted** after t67 REPORT-25 `25248ba2`: 2 lib + 22 fold passing,
  clippy exit 0, and both clone/geometric mutants exit 101. It is **already live** at
  `store/src/fold.rs` `56aca632`, so the t62-sole-author / t61-exact-integration assignment
  reads as discharged for this file rather than pending. Its temporary root 2-file lease has
  ended.

Attempt oracle `54b7623b` is accepted bounded per `ATTEMPT-ORACLE-REVIEW.md` (eight-case
probe exit 0, bounded to 64 attempts / 256 worlds, judging versioned commit sequences not
record contents) and is **confirmed live**. Earlier candidates `05698731` and `48c93e29`
stay frozen as rejected.

**Delivery-09 (commit `2858` / init / prepared) is WITHDRAWN and removed from live**, on four
named defects: empty-event encoding that guarantees `ZeroEvents` after writes; a fabricated
post-persist ACK with two `String` clones; erased refusal classes; and no actual commit
test. `store/src` no longer carries a commit or init module.

Live Store at that time was **seven source modules** — `fold`, `fs`, `lease` (`59752f42`),
`lease_tests`, `lib` (`b216e233`), `names`, `os` — with no commit, init, prepared, handle or
pin module. That is a **historical T85-era reading of its own identity**, superseded by the
accepted P1-4 integration: the current live Store is the 12-module set recorded below. The
`lib` identity then was a distinct live integration with doc/scope hygiene, and the two leaf
lease files stay byte-identical to their acceptance.

**Lease** is a **bounded, bytes-only** component accepted at packet `19775004`
(`LEASE-V2C-ACCEPTANCE.md` `5e46db06`) after REVIEW-30 `71621dbd`: `lease.rs` `59752f42` /
`lease_tests.rs` `197ef2aa`, with t61's actual `lib` result of 15 = 13 lease-related plus 2
existing fold tests — not 15 new tests — and strict rustdoc clean with all warnings denied.
V1 is rejected and rolled back; v2/v2b stay as historical correction evidence. Types remains
dependency-free (`ledger` `b2943`, `lib` `97cf59f6`, `Cargo.toml` `aab8cc83`), and the Store
provider stays the approved Store-only `triomphe`. Bounds recorded so they are not over-read:
the **live byte ceiling is independent of the monotone cumulative diagnostic**; entry counts
remain cumulative attempt work and are **never a token lease**; untransferred bytes carry
attempt ownership and release with that owner, while a transferred token retains its extent
and counter past facade/Store drop; transfer shortage is typed separately from `Budget`; and
cumulative sums use checked addition into `Option<u128>`, where `None` means unrepresentable
rather than zero or quota exhaustion. No allocator, RSS, concurrency-runtime or observed
allocation-window claim follows.

**RecordSet consuming accessor** is accepted/live in Types `persist` `cd275522` (approval
`625f0339`, authoritative content-addressed packet
`/tmp/sage-db-p1.4-handoff/delivery-recordset-cd275522`, `FILES` `41bc9120`, root 1/1). The
MAIN types lib run is **18 = 17 existing + 1** selected buffer pointer/capacity/content
regression — not allocator instrumentation; `from_sorted`/order/work semantics are unchanged
and Types stays dependency-free. Provenance warning: the old `recordset-into-entries-v1`
cited path was **overwritten in place**, so that v1 must not be treated as frozen authority.
The new `PATHS-AND-CORRECTION` `6bb73772` records what recovered (`ab7` source plus old logs,
reconstructed manifest `535`) and what did not (old diff `207`, unrecovered).

**T82 is HELD** per `374054fc`: no MAIN runtime integration. Narrow credit — the original
real-Genesis plus ordinary-192 positive passes independently through actual Store and
ModelFileSystem. But **two G0 probes fail** (pre-Genesis `coverage_complete` true while
`head=None`; a client candidate before Ready returning `VersionConflict{1,2}` instead of
unready coverage), and a **finite `ActualLedger` retention probe fails**: on facade drop,
live reservations go `27007 → 0` while the Store still serves 2 current entries. That is
premature release while retained data lives — explicitly **not** RSS, and **not** v1's
never-released leak. No exact retained-byte total is inferred from `27007`, which includes
scratch/work. Source contradictions stand: unused fresh-lock handoff (`prepare_fresh` has no
call sites), public pre-Genesis init API, no actual pin in the handle, header heap clones,
remaining fork keys/outcome/import copies with a wrong slot `T`, and `ArcRecord` carrying no
lease. REPORT-32 `9b05767b` closes only the bounded F1 scan of pin `94f`, not a Store or
allocation claim. t62 has one **new coherent candidate implementation** released for the
full `374` batch; t61 MAIN otherwise holds.

**T83 is HELD** per `T83-ACTUAL-OWNER-AND-PUBLICATION-REVIEW.md` `1987d1ec` (287-payload seal
`5ae68766`, root-verified). Bounded credit for real source progress: public `init::bootstrap`
now calls `prepare_fresh`, consumes the private `FreshRoot`, retains the same lock across
families and uses crate-private `open_initializing`/`commit_genesis`; initial coverage is
false and the usable Store returns only after actual `Committed`; the old public
`open_fresh`/`commit_genesis` route is gone. A real pin is prepared before persistence and
swapped on success. That closes the prior unused-handoff/public-pre-Genesis gap at source
level — no fault or durability claim follows, and `open_initializing`'s redundant
CURRENT/families recheck is a documentation inconsistency, not the old lock gap.

Scope of the root proof, stated to prevent over-reading: ONE private public-bootstrap probe,
`EXIT101`, 0 passed / 1 failed / 2 filtered. The bootstrap genuinely succeeds first —
`head=1`, `coverage=true`, a real pin exists, and both retained entries are value-equal to
the pin's — but `ROOT_SAME_ENTRY_BUFFER=false` and `ROOT_SAME_BODY_BUFFER=false` (first body
151 B), so the reader view does not borrow the published immutable pin buffer. This is
**selected-buffer identity plus source ordering**, not allocator instrumentation and not
RSS. The author's `385` figure is a count, not acceptance.

Standing contradictions in that batch: `carrier.clone` remains in `tx_digest_of`;
`Transfer`/`Layout`/`ExtentOverflow` are still flattened to `Refusal::Allocation{0}`, which
is false for each; history-replay and deep-fork copies remain; `outerRecordOwner` carries no
nested-retention adoption for moved String/Vec capacities; and Genesis in fact creates **two**
`SharedRecord` owners, not none.

t62 holds one **new coherent candidate** for the next isolated batch. t63 has been released
on exact T83 for independent functional controls **before** acceptance: real three-commit,
current/old-pin, coverage and readback, with retry comparing recorded payload plus original
version and observing durable CURRENT/manifest/root-write stability, and persisted
`state_digest` read from the real manifest and independently recomputed. No fake Store and no
author-tree execution. t61 MAIN holds with no runtime integration.

**T84 is HELD** (`T84-DELTA-AND-RETAINED-EXTENT-REVIEW.md` `9de6d45d`; seal `083a29d6`,
287 payloads, root-verified 287/287). **Partial credit only.** One cargo invocation with
`--no-fail-fast` over the two existing discriminators: `PROBE_EXIT=101`, one passed / one
failed, each a single-test target.

- **Same-buffer control now PASSES** — `ROOT_SAME_ENTRY_BUFFER=true` and
  `ROOT_SAME_BODY_BUFFER=true` (first body 151 B). Source deletes `current_entries` and the
  post-`Committed` `to_vec`, so `retained_entries` borrows the actual pin. That closes the
  specific T83 buffer-identity defect on the T84 identity; it does **not** prove the rest of
  the post-fence window is allocation-free.
- **Nested retained-credit control STILL FAILS** — after facade drop the fold still serves
  its 600-byte atom and 16 triggers; the actual selected nested-capacity lower bound is
  **3915** against entire held/live **1131** with `untransferred=0` (T83 was 480). So some
  payload now transfers, but the credited claim is smaller than the selected real buffers.
  This is **not** RSS, allocator instrumentation, complete-extent arithmetic, or the whole
  scratch budget.

Real improvements also credited: current derivation starts from pin plus candidate rather
than walking history; `copy_entry_admitted` genuinely charges key/body length, `try_reserve`s
and copies; pin layout/extent/transfer now map to their own `HandleRefusal` variants with a
real request on actual `Allocate`; non-`AlreadyExists` `create_root` errors keep their actual
`FsError`. These bounded changes do **not** establish the claimed complete seven-item closure.
Still held: `fork_string_tables`' `clone_from` over current/outcomes/imports (and the
revision tuple keys), which the pre-reserved table does not make fallible; `record_owned_bytes`
omitting triggers/legacy strings/`CommonFields` with nine family arms returning `0`, where a
lower or zero estimate is not a conservative retained ceiling; surviving fold tables,
identity keys, outcomes, imports and handle history still without a releasing retained owner;
and double charging the fork slots. Snapshot owner `160` was measured only on the prior
success path and is **not** `RecordOwner` or Store closure.

**Header-owning-copy** is **accepted bounded and the three-file lease is RETURNED to t62**
(`8525da01`, after REPORT-33 `744ae6eb`): final packet `delivery-header-once-afa45f7d`
(`FILES` `912b1e47`) with handle `0aac40b3`, prepared `afa45f7d`, commit `0e5ef2d3`. The old
prepared `16a3fed2` is a doc-only derivative measured separately; original functional 3/0 plus
check/clippy-all-targets/docs/fmt all 0, and the final doc-only identity re-ran only affected
strictdoc/fmt at 0 with no test recount. This is a **candidate only, not MAIN**.
t63 observed v2 is still **partial** pending actual `read_transaction`/public `read_header`
and all ancestor-hash/header joins, so the persisted-digest item must **not** be marked
complete.

**T85 is HELD — partial** (`T85-RESOURCE-AND-RUNTIME-REVIEW.md` `fc425691`; seal `e820f5c2`,
287 payloads, root 287/287). One root private selected-retention target on unchanged
production: `PROBE_EXIT0`, 1 passed / 0 failed, actual nested capacity lower bound **3915**
against held/live **4411** with `untransferred=0` after facade drop while the decoded Term
(600-byte atom, 16 triggers) still lives. The former `480`/`1131` under-retention case is
**closed on T85**. This certifies that specific case only — **not** all fields, families,
tables, allocator extent or RSS. Also credited bounded: the Model-identity/revisions key path
now uses a real fallible string builder, the repeated outer-slot charge in
`fork_string_tables` is removed, and init keeps `ControlStop` with the correct
attempted-authority distinction. No whole-resource closure follows from the 389 tests.
Remaining held: `copy_outcome`/`copy_import` still begin with a wholesale `.clone()` that
already allocates every owned field, and precharge does not make `Clone` fallible; extent
walks still omit `Term.changed_fields`, the `OptionalActorAudit` `recorded_by`/`reason` and
`changed_fields`, and use `entries().len` where outer `Vec` capacity is meant; and the
surviving fold tables/keys/outcomes/imports still lack their own truthful owner.

**Types `Extensions::capacity`** is accepted and **live** at `persist` `40ee1e54` (approval
`ba932ea9`) from the exact-one-file packet `delivery-extensions-capacity-40ee1e54`
(`FILES` `4e6cd658`); MAIN Types lib is **19 = 18 + 1** with the root integration log copy
`8285165d`, and no dependency, ledger or manifest change. This closes the concrete
private-container gap and is the only Types file a composition may overlay here.

**t63 observed v4 test31/18** is **bounded accepted on T83 only** (approval `84c584`): real
readers, all hash/header joins, independently disk/input-derived persisted `state_digest`,
old pin `2`, and exact original/persisted retry payload+version with zero named mutating seam
calls. It is a legal positive consistency result and **must be rerun on the new concrete
candidate before phase**. REPORT-34 B is a **contract failure**: actual post-first-write
cancellation returns `NotCommitted{Filesystem PublishBlob …}` where §4.1 makes
persistence uncancellable. Only the `A` Unknown case passes (write fence, old-pin retention);
latest-absent is **unmet**, corrected by note 34A `f0dd4077`, with no P1-5 dependency.

**T85 retry allocation measurement** (`packet` `98ce`, 7 files verified): real bootstrap plus
`Committed` ordinary-192-only request, already-committed original version 2, **3 allocations /
0 reallocations / 26 requested bytes**, and a separate live-counter control allocating 4096
once, `PROGRAM_EXIT0` preserved. The `26` is input-specific on hand-authored valid fixture —
**not** a P-NFA, no I/O, RSS or Store constant. Digest `0x22` is a valid matching nonzero
value; the `v4[version]` convention is not protocol.

Next: **one** complete isolated batch from t62 covering resources plus explicit
phase/Unknown-view and revision checks. MAIN runtime holds; P1-4 open; P1-5/P1-6 held.

**T86 is HELD — partial** (`T86-INCOMPLETE-BATCH-AND-RUNTIME-REVIEW.md` `e5221783`; seal
`dec8c2db`, 287 payloads root 287/287). The complete `fc425691` assignment is **not**
discharged; mandatory paths are labelled NOT DONE in the source report itself.

Root private runtime probe (`PROBE-MANIFEST` `f031e008`, raw log `c81ba408`), ONE invocation,
`CARGO_EXIT=101`, **zero passed / two failed**, while B's unhalted positive genuinely commits
version 2:

- **A** — `published_pin()` is now `None` (credit this specific correction), but
  `retained_entries()` still returns the **two old current entries** (`left2/right0`): source
  `handle:325-333` reads `self.pin` directly, bypassing the authority gate at
  `published_pin:291`. The explicitly retained old pin, version 2 and reservation 561 stay
  stable, and the fenced follow-up makes no mutating calls.
- **B** — the same control flip after actual write 1 now proceeds to a **second write** and
  returns `NotCommitted{Derivation(Refused(Stopped(Cancelled)))}` with head 1, where
  Committed(version 2) is required. Same contract-failure class as REPORT-34 at a later call
  site, not closure: `PersistingPhase` covers only two blob calls, while
  `verify_and_sync_rules`, transaction and manifest publication and `render_name_bounded`
  receive the original ledger again.

`K12b` is **missing from source** — there is no `NotCommittedEffectUnknown` fence — recorded as
a source gap, not an observed orphan-fault claim.

Credited locally: fold `copy_outcome`/`copy_import` now build audit/import/extension fields
directly instead of cloning whole records; `copy_string` bills and reserves the same new
length with the earlier bulk-plus-field admission gone; `Term.changed_fields`,
KbMetadata/RuleConfig optional audit plus `changed_fields`, and real `Extensions` outer
capacity are in the retained walk; Genesis metadata/rules group **moves** replace copies; and
unused history afterimages/watermarks are removed without touching disk or fold revision
history. **No all-resource claim follows.** Still held: bookkeeping remains nested
`clone()`s with **no ledger parameter** at all (its outer Vecs are `try_reserve`d without
byte/entry admission); the fold-container owner is still absent (`StateFold`'s seven Vecs,
index keys and outcome/import buffers have no token); `copy_extensions` still bills
`value.capacity()` but reserves `value.len()`; and factories, `Field` collect, reply,
revision-continuity and exact-retry `stored.event.clone` remain.

New **one** complete t62 batch is authorized. No MAIN runtime; P1-4 open; P1-5/P1-6 held. The
T85 specific closure above stays a T85 statement about that one control.

**T87 is HELD — partial** (`T87-PARTIAL-OWNERSHIP-AND-REVISION-REVIEW.md` `03f1d0dd`; seal
`ac40f677`, 287 payloads root 287/287). Probe `PROBE-MANIFEST` `10a83759` / raw `run.log`
`de944e99`: ONE invocation, `CARGO_EXIT=101`, **3 passed / 1 failed**, with the three old
standing-v4 tests **filtered out** (not rerun, not passes). The manifest lists its input seal,
verification, command, log and three probe tests — it lists neither itself nor the review.

**Three bounded runtime controls now PASS.** (1) Attempted-Unknown after bootstrap plus v2:
both fresh current views are absent — `published_pin=None` **and** `retained_entries` empty —
while the explicit old pin keeps its values, version 2 and reservation 561, with a fenced
follow-up making no mutating calls. (2) A new existing-`FaultSchedule` Publish-Fail-Unknown:
actual `NotCommitted` carrying real `Filesystem::Unknown` and a fence, yet the **old durable
current v2 stays visible**, value-equal to the old pin, and the follow-up is Fenced with no
mutating calls. (3) The same control flip after actual write 1 now reaches `Committed(2)`,
unlike T86. None of this is platform durability, recovery or allocator evidence, and the
initial-stop branch did **not** take an independent zero-mutating-call measurement.

**One control still FAILS, and it is a genuine public-path revision defect.** After a legal
Term1@rev1 at v2 commits, a **new, never-seen Term2 at revision 2** returns `Committed(3)`
with head 3 and changed mutating counters, where NotCommitted/head 2/no mutation is required.
`insert_record` only checks `prev+1` when `current_slot` is `Ok`, so the missing-identity
branch accepts any nonzero revision; MODEL §3 rule 1 requires **1** for creation. The old
arbitrary first-revision-2 fixture cannot override the protocol — correcting it to a legal
prefix is authorized **only** in a new candidate, preserving the assertion that the untouched
record stays at revision 2.

Credited: `Field` descriptor fallibility and `Format` forwarding, the corrected current-phase
path, and the last-only RAM metadata simplification. **Ownership is partial and
error-laundered**: `adopt_container` is on the real path now, but `container_extent` omits the
Model String inside every revisions tuple key and the outcome/import nested payloads;
`adopt_retained` errors are still mapped to `Refusal::Allocation{requested:extent}` when
**no allocator failed**; a hidden `audit.clone` was **relocated** into the new bookkeeping
helper; and `predecessorName.clone`, factory allocations, saturated actual-record extent and
the retry clone all persist.

Explicitly authorized in the next candidate: a **public opaque `Store::SharedOutcome`** wrapper
with private provider/lease internals, immutable payload access and value equality (Clone as a
refcount bump), built fallibly before persistence — with **no** Types/format/lease change.
Exact retry is **not** implemented yet. One complete t62 correction; no MAIN runtime, no P1-4
acceptance, P1-5/P1-6 held.

**HISTORICAL — superseded acceptance, known-bad producer.** Re-acceptance did **not**
reinstate this capture: `final-p1-4-20261117T220443Z` / `19c8aff7` carries the defective
raw-file-SHA-as-`tx_digest` producer and remains historical evidence only. P1-4 was formerly
recorded as formally ACCEPTED per
`/tmp/sage-db-p1.2yjLG4/P1-4-ACCEPTANCE.md` (`dde6b3f1`) on immutable capture
`/tmp/sage-db-p1.2yjLG4/final-p1-4-20261117T220443Z`: source `MANIFEST.sha256` `19c8aff7`,
**516 payloads** verified 516/0/0 with zero included-input drift **at capture**, evidence
manifest `cd7092f4`. Gates of record are one real `scripts/ci.sh` run, `CI_EXIT=0`, carrying
Rust **1083 passed / 0 failed**, contract **50/0**, main TypeScript **348/0**, plus separate
strict workspace rustdoc `DOC_EXIT=0` (raw `b710047d` and `1f52b57d`). Per-target counts come
from GATE-SUMMARY, not projected totals. The 16 integration paths are 3 shared formatting/
export-order hygiene plus 13 Store/runtime/test paths — 6 existing preimages and 10 new —
with 500 preexisting MAIN inputs outside them unchanged; the integrator's `3cb` evidence is
preserved with its erroneous 8/8 prose corrected to 6/10 by note `94355ff0`.
Live Store is now **12 source modules**: `commit` `b8829641`, `fold` `407f0d7d`, `handle`
`ec4fa917`, `lib` `e46f0fe1`, `init` `7768d178`, `pin` `372f4074`, `prepared` `afa45f7d`,
with `lease` `59752f42` unchanged; shared/format results `33b` (format lib), `229` (format
tests), `e643` (types lib), and `ledger` `b294`/types `Cargo` `aab8`/`persist` `40ee`/
provider `cfc`/lock `07`/CI `441` unchanged. `handle`/`lib` are comments-only derivatives of
the reviewed runtime source; owner code remains `commit b882`/`fold 407`.
Scope limits stay explicit: the root entry-to-outer-return **P-NFA** passes debug and release
at 0 alloc / 0 realloc for **one fixture/capacity on a memory filesystem**, with both
in-window controls rejected — **not** OS/backend/platform/device, universal all-input, RSS or
fault-matrix evidence. Exact-retry 0-alloc and `RecordOwner` 496 remain bound to their own
identities, not relabelled as whole-Store figures. Detailed artifacts and limits are bound by
root owner review `8e14e0d3` and outside note `6a2027f5`.
P1-5 and P1-6 **implementation remain held**; no whole-P1, format-major-1, cross-platform
durability or release claim follows. The accepted P1-2 capture is unchanged, and T85–T87
readings above stay historical records of their own identities. One caveat of this index:
because this document itself changed, whole-MAIN drift must **not** be claimed zero *after*
this edit — the runtime inputs remain exact, the index does not.

**HISTORICAL — evidence correction 01** (`P1-4-ACCEPTANCE-CORRECTION-01.md`
`5e7ef92f`). That record reopened P1-4 until the bounded transaction-chain digest producer was
corrected and verified. Both historical acceptances and this correction remain byte-identical
evidence — never rewritten, renamed or erased — and its `1083/0` gate result stays factual:
none of those assertions recomputed the normative chain input. The reopened state it created
is now superseded by the re-acceptance below.

Defect, in the accepted Store producer: normative `storage-digests` §4.1 defines
`tx_digest = digest(Domain::Tx, uncompressed tx BODY)`, excluding file header, block headers
and trailer, with §4.3 chaining that; `tx_file_sha256` stays `raw_sha256` over the **complete
file** as a separate quantity. The maintainer confirms in live source that
`handle::tx_digest_of` returns `encode_tx_image_borrowed`'s second tuple member and
`prepared::encode_impl` computes it as `raw_sha256_bounded(&sink.bytes)` over the whole
encoded file. Wrong **domain** and wrong **byte scope** are independent: prefixing the full
file, or raw-hashing just the body, would each still be wrong. The raw-file hash correctly
serves file identity — its use as `tx_digest` is the error.

Public-bootstrap counterexample `p1-4-root-review/chain-domain-actual-probe`
(`PROBE-MANIFEST` `4e97818e`, log `ce2f6a75`): actual Genesis commits and durable
CURRENT/manifest/tx hashes/frames validate first, then the normative chain assertion fails —
`PROBE_EXIT101`, 0 passed / 1 failed, 5 old tests filtered. Actual chain
`70128f61…fbd099`; expected domain/body chain `5429d6aa…e2084`; `MATCHES_RAW_FILE_CHAIN=true`,
with unequal raw-file and domain/body digests as the discriminator. Its inherited subset has
324 hash-matching inputs with all sage-db inputs present (not a full TS repo), no SUT edit,
and the first probe's Debug-trait setup compile failure is preserved as **setup, not a product
finding**. REPORT-39 (`5f60bda2`) independently confirms the domain **and** scope error; its
"fails to pin order" wording is too broad, since the raw whole-file SHA still binds event
bytes and order — the established defect is disagreement with the specified domain/input and
the resulting protocol-chain incompatibility.

Fix scope is Store-local: keep raw-file-hash API semantics, compute the tx-domain digest from
the **real encoded body bytes** with accepted bounded envelope/digest helpers, and feed that
to `chain_digest`/`FoldedVersion` — no re-encoding via a header that drops extensions, no
changing expected values to raw SHA, no compatibility acceptance of wrong raw-file chains in
P1-5. No canonical Types/format/lease/provider change is needed. Bounded ownership,
allocator and atomic-publication proofs retain their original scopes; they did not test this
obligation. At the time that fix was still outstanding, MAIN carried those bytes with no
service or real-KB write and no rollback. **The fix landed and P1-4 was re-accepted — see
below.** P1-5 was not to be marked released on the defect; it is now released for
implementation only.

**P1-4 RE-ACCEPTED after the transaction-domain correction**
(`P1-4-ACCEPTANCE-02.md` `8db979b2`) on corrected consumer
`/tmp/sage-db-p1.2yjLG4/final-p1-4-v2-20261117T224953Z/repo`, `MANIFEST` `e3730b90` with
**516** payloads and `EVIDENCE` `8bd896ed`. Root verified all **516 live** hashes 0 missing /
0 mismatch and zero included-input drift **at capture** — this index edit changes that metadata
afterwards, so no zero-drift claim is made post-edit. Exactly **2** integration paths, both
existing preimages: `handle` `ec4fa917` → `1d738837` and `commit_readback` `cf96d81d` →
`ed15f8b2`, with the other 514 unchanged; integration evidence `0e14bd87`. No shared
Types/format/lease/provider/CI/lockfile repair was needed.
Correction mechanism: the helper takes the real encoded image from the unchanged prepared
encoder, calls canonical `read_envelope`, then `digest_bounded(Domain::Tx, &envelope.body)`
with the **same** ledger — hashing the complete tx header (including extensions) plus ordered
events while excluding file/block headers and trailer, feeding the existing
`chain_digest`/`FoldedVersion`. `tx_file_sha256` stays raw SHA over the full file, and Format /
Refused / Budget / Stopped classes are intact; all added work is derivation before persistence.
REPORT-40 `802afe21` independently source-checks the exact fix. A new regression through real
public bootstrap, ordinary v2 and v3 reads durable CURRENT/manifest/tx bytes with canonical
readers and predecessor joins, checking each manifest chain against `digest(Tx, actual body)`
and discriminating both raw-file SHA and whole-file `DomainTx` — target `EXIT0`, 1 passed /
5 filtered, and the standing functional suite is now **9** bundled tests.
Gates: second full `scripts/ci.sh` `CI_EXIT=0` with Rust **1084 passed / 0 failed**, contract
**50/0**, main TS **348/0** (raw `e6bc6cf7`), and strict workspace rustdoc `DOC_EXIT=0` (raw
`2fbddac6`). Failed-run history is preserved honestly: the first full CI failed ONE unchanged
TypeScript CLI HTTP search test with cause **unestablished**, that unchanged target then passed
1/0, and the whole unchanged-source CI passed — no TS source, fixture or assertion was
modified and no stage bypassed.
NEW P-NFA packet `34970437` binds the identical external main/backend/fixture/controls to all
516 corrected source hashes: debug **and** release `PROGRAM_EXIT0`, true-entry 0 alloc /
0 realloc / 0 bytes, with allocation-only 1/0 and prebuilt-grow 0/1 executing **inside** the
interval and both rejected. Limits unchanged: one 192-only fixture/capacity on a memory
backend, not universal-input, OS/platform or RSS evidence.

**HISTORICAL.** P1-5 implementation was released (not accepted) to t62 on the corrected base
per `P1-5-IMPLEMENTATION-RELEASE.md` `442a923e`, covering full Genesis replay/history/verify in
an isolated copy, with MAIN runtime then held and P1-6 then held. Both subsequent milestones
(r1 and r7) are preserved below as their own historical verdicts; P1-5 has since been accepted.

**P1-5 milestone01 — HELD, bounded progress only** (`P1-5-MILESTONE01-REVIEW.md`
`85da467a`; independent verify `0513eaa9`). P1-5 is **not accepted**; MAIN P1-5 runtime stays
held; the author continues the full isolated batch in a NEW working candidate. Frozen
milestone `519475f4` source is 520/520 verified by author **and** root copy: versus the
corrected 516 base only `handle`/`lib` changed plus four new files
(`open`/`view`/`p15_open`/`support/shared_fs`), with no base path missing and the other 514
preserved. Source report 41 `d83e4889` is corrected by 41A `efdce67a`: **ordering/named-chain
progress only** — the broad acquisition PASS and "next-commit-metadata-is-fine" claims were
withdrawn. Independent packet `82b75206` has **FIVE** payload entries (not six), all
root-hash-verified, with test `298c3eef` and raw `f0c89ed2` recording `ACTUAL_CARGO_EXIT=101`,
**2 passed / 2 failed**; the historical `e12a607b` log is the deliberate re-execution against
old source `04676f74`, and the swapped-role prose is corrected outside by `53d60ed9`.

**Credited only**: genuine bootstrap → A+B ordinary v2 → A rev2 ordinary v3 → drop → public
reopen on the same model with a fresh finite ledger, where reopened current rows match
pre-drop rows, the selected rev2 fixture agrees with the accepted canonical
`format::project_event` reference, the explicit old pin's values/reservation are unchanged,
and tagged basis joins the actual CURRENT-named manifest FILE hash. That is preservation plus
canonical-projection agreement — **not** a new independent golden serializer, all-family proof
or complete G0 proof. Also credited: exact retry after reopen returns `AlreadyCommitted` at
the **original** version 2 with equal payload, zero **named** mutating seam calls through the
same-instance observer, and unchanged durable names/CURRENT bytes.

**Two real failures**, both genuine product/contract defects, with later assertions on new
predecessor/file/body-domain chain, fresh-view agreement and old-pin stability **unreached**,
not passed: (1) fresh bootstrap+commit has a published pin but `Store::view() == None`, because
the current-view slot is only initialized during reopen; (2) a successfully reopened Ready v2
handle refuses its **new legal advancing v3** request as
`NotCommitted{Derivation(Format(Corrupt(ManifestTxMismatch)))}` — `last_committed` is `None`
after reopen, so `derive_next` supplies no predecessor name/hash and seeds the chain from
zero. Only **exact retry** was measured passing; pre-derivation refusals lie outside this
advancing-commit failure's causal scope rather than having been newly tested as passes. So it
is **not** true that every commit fails.

Still required, as one coherent correction rather than per-finding patches: restore real
predecessor metadata before any Ready publication; one publication basis on every transition;
the actual ordered whole-chain sync with the same ledger; finished acquisition and truthful
error mapping; complete materialization ownership and validation; and the full previously
released deliverable (per-version digests, existing-handle resolution, read-only verify
through the same engine, G0 outcome, retained history image). Author r2 `8431d563` and r3
`5ee61d50` are **different source identities**, with separate evidentiary levels: ROOT read
r2's source call placement **only**, without executing it; r3 is **author-reported progress**,
with no independent source or runtime review. Neither is accepted and their counts cannot be
transplanted. P1-6 held. No MAIN integration, canonical Types/format/lease/provider/model-core
change, or platform/RSS/all-input acceptance follows.

**P1-5 r7 — HELD, not accepted** (`P1-5-R7-CONSOLIDATED-REVIEW.md` `71e82b78`; input seal
`6a720d40`, **522** payloads verified by author and root copy; `full-source-r7/VERIFY.json`
`d1784c22`). Frozen only — the author continues the authorized full batch in a **new r8
candidate**, and no r7 count is transplanted there. Release `442a923e` and binding decisions
`f6a06ad3` remain effective; the corrected base is `e3730b90`, not the known-bad earlier P1-4
capture. Peer REPORT-42 `4823d3fe` is corrected by 42A `645b11ab` and 42B `47418b4b`: the
original blanket "all nine fixed / only one blocker" is **withdrawn**.

Three separate ROOT narrow targets on isolated full r7 copies, each compiled with the input
seal still matching afterwards and every raw exit captured before parsing — product findings,
not setup failures:

| Target | Manifest | Raw log | Actual |
|---|---|---|---|
| `history-probe-r7` | `4bcdb4aa` | `cfb0b02a` | `EXIT101`, **0 passed / 3 failed** |
| `state-probe-r7` | `6f3800c6` | `4343480c` | `EXIT101`, **0 passed / 3 failed** |
| `corruption-reader-r7` | `841eb8c9` | `8eb24721` | `EXIT101`, **1 passed / 1 failed** |

Scopes: history is a RAM `revisions_descending` lookup with **no filesystem or basis
parameter**, so its documentation claim about verifying transaction/blob bytes is false —
observed `source_version` 1 for a version-2 commit, zero additional read/hash seam calls on a
hit, and a fabricated `Budget(Entries,1,0)` at `max_events=0` while the query ledger still had
1,048,576 entries and admitted none. State/G0 must follow every exit: an attempted-Unknown
commit still returned `Found` from `outcome` despite genuine fence/view/pin/coverage
corrections, and a failed new resolution left old valid-looking fresh claims
(`false,false,false,false,true,false` where all true was required) — a genuine failed
resolution, not a claim that the refusal happened inside replay. `Corrupt` is terminal but
`mark_terminal_corrupt` does not set `terminal_corrupt=true` and `resolve` has no initial
terminal guard. The whole committed reference union is missing at its **producer**: only
`fold_actions` constructs `CommittedBlobRef`, replay derives none, so the apparent complete
blob sync iterates zero files.

The reader control for one false old-state claim **PASSES**: despite legal codec bytes,
repaired real file-hash joins and an unchanged valid tail state, public open correctly returns
**Corrupt** for the false prior claim — valid framing and hashes do not make a false state
digest true. The separate **missing-old-blob reader FAILS** and is actual: v2's
historical-only COMMITTED body blob is removed while v3 current body exists and
CURRENT/manifests/transactions are unchanged,
yet public open returns Ready instead of Corrupt — that blob is reachable from
CURRENT → v3 → v2, so it is **not** an orphan. No all-resource, allocator, platform, RSS or
full-phase claim followed from those eight narrow tests. That r7 verdict is **historical**: the
later corrected candidate closed the listed gaps and P1-5 is now accepted below, while MAIN
runtime for P1-6 remains held.

**P1-5 ACCEPTED and MAIN-integrated** (`P1-5-ACCEPTANCE.md` `49abce2c`) on what was, **at that
time**, the only current consumer for recovery and later P1-6 work — now historical navigation,
since `final-p1-6-20261118T152842Z` is the current whole-workspace consumer. Immutable origin:
`final-p1-5-20261118T092938Z/repo`: `MANIFEST` `b831369c` with **525**
payloads, `CAPTURE.json` `0c340537`, `EVIDENCE` `15cee4d4` with **115** non-repo artifacts
(self-excluding, with the source MANIFEST included as a separately hashed artifact). Verified
525/0 missing / 0 mismatch / 0 unlisted **at capture**; source bytes equal live MAIN then.
Corrected P1-4 origin `e3730b90` remains the accepted predecessor, while `19c8aff7` stays
historical known-bad and was never reinstated. Final real `scripts/ci.sh`:
`P15_FINAL_CI_EXIT=0` (raw `5ad16be2`) with Rust **1105 passed / 0 failed** across **70**
`test result` summaries, contract **50/0**, main TS **348/0**; separate strict workspace docs
`P15_FINAL_DOC_EXIT=0` (raw `5e5c9c5a`). Integration is exactly **12** paths — 6 Store source
(`fold`/`handle`/`lib` existing; `open`/`view`/`history` new) plus 6 tests/support — with 3
preimages, 9 new, and **513** outside inputs unchanged; prepared packet `0bdced89`/`19`,
JSON `b49f15d6`, integrator record `64c09263`, no extra gates. Product identities:
`open` `fd99196d`, `view` `c0b07995`, `history` `fcaaa3f6`, `fold` `3a31d80c`, `handle`
`dd755794`, `lib` `0b74c824`; the source **FILES set of 15 Store files includes `lib` and
`lease_tests`**, which must not be described as 15 public modules. Types `persist` `40ee1e54`,
`ledger` `b2943b0c`, Types `Cargo` `aab8cc83`, lease `59752f42`, Store `Cargo` `cfc3f24e`,
lock `07dc5df8`, CI `4416bcde`, and `commit`/`prepared`/`pin`/format/provider/model-core stay
unchanged.
Evidence: source 43+43A bounded PASS; peer 4-v2 and peer complement carry explicit bounds; ROOT
supplements on unchanged product code close the assertion gaps — exact **17** global sync
boundaries `1/0`, an actual second-read hook resolving a known-v3 handle onto observed v2 with
exact **13** old-chain syncs `1/0`, both actual Unknown resolutions `2/0`, and four false
old-semantic claims plus a healthy control `5/0`. **Counts overlap and are filtered runs — they
must not be summed into a unique or all-matrix total.** New `HistoryFrame` owner at the real
allocation seam: 128 requested bytes / one allocation / zero realloc, 128 prediction vs a 120
headerless control, owner block only, with 48 span slots and a synthetic predecessor outside the
window; allocator packet `0cb7265b`/11 plus outside `fc4266e3`. True-entry P-NFA on the final
product identities is 0 alloc / 0 realloc in **debug and release** with both in-window controls
rejected — one fixture/capacity/host, process-global counters. Historical retry-0 and
`RecordOwner 496` keep their own identities and are not transposed.
**Zero source drift is an at-capture fact**: this authorized index update changes the document
hash afterwards, not the immutable capture.

**HISTORICAL — P1-6 implementation release** (`P1-6-IMPLEMENTATION-RELEASE.md`
`ae7410c4`). That release assigned the following then-roles: t62 owned only the test-model
restore plus an owned model lock; t63 owned the real Store finite fault matrix; t61 owned
actual child-process kill and fresh-process recovery over **modeled** durable state —
explicitly **not** OS/device power-loss; t67 was assigned independent source review against a
bound helper. MAIN then held that work with no harness integration; the release itself never
was acceptance. ROOT later took over the matrix and process corrections, and agy supplied the
final two process source files under ROOT lease. Formal acceptance followed on the identified
final source; P1-6 is accepted below.

**P1-6 ACCEPTED — all six P1 scoped software tasks complete at draft major0**
(`P1-6-ACCEPTANCE.md` `ea1637c8`; coherent review `P1-6-FINAL-COHERENT-REVIEW.md`
`9d57a781`). Current **whole-workspace consumer**: immutable
`final-p1-6-20261118T152842Z/repo` — `MANIFEST` `34457fda` with **536** source payloads,
`CAPTURE.json` `25d9f022`, `EVIDENCE` `3d7c889b` with **326** artifacts (self-excluding). Root
verified 536 source + 326 evidence at capture, 0 missing / 0 mismatch / 0 unlisted, and live
equalled the capture **then**. Integration is exactly **13** paths — **2** existing test-model
preimages plus **11** new — with **523** outside inputs unchanged, copy release `58c476a3`,
packet `7c909d43`/20, JSON `75aed6fa`, integrator record `0211c878`. One real `scripts/ci.sh`
on the final 536 bytes: `P16_FINAL_CI_EXIT=0` (raw `f801c2b0`) with Rust **1138 passed /
0 failed** across **76** actual `test result: ok` summaries, contract **50/0**, main TS
**348/0**; strict workspace rustdoc `P16_FINAL_DOC_EXIT=0` (raw `15825fb7`); all 536 verified
before and after both. Reviews 47/50/50A/51/52 are bounded source-only, and 52 corrects the
record to **8 cases with 15 total worlds** and notes that hardcoded arming metadata is **not**
independent evidence.
What was exercised: the final matrix has **104** invocation records — 35 baseline, 68
additional critical-ordinal, and one separate 16-byte quota `NoSpace` — producing **225**
sampled observations that include overlap and are **neither distinct obligations nor
all-inputs**. The process driver runs 8 live `SIG9` kills and 15 fresh recovery processes over
**modeled** exported durable state. The OS-lock probe is real but conservative at one seam on
one host. All **15** production Store files, shared canonical surfaces, Cargo/CI and apps are
byte-identical; only the test harness, model helper, binaries and test scope changed, so no
product source moved. Prior allocator / P-NFA / retry evidence keeps its own identities and is
not transposed.
Limits: this is finite fixture/model/host evidence — **modeled-state restart, not OS/device
power-loss**, and no RSS, all-input or P-NFA inference. Format stays **draft major0**:
`storage.md` §4.4's migration-tool prerequisite is outside the approved P1 scope, **major1 is
not frozen**, and no P2 implementation starts automatically. **Zero drift is an at-capture
fact** — this authorized navigation edit changes the document hash afterwards, not the
immutable capture.

**Canonical P1-4 review/consumer base was v4** — now **historical navigation only**. It was
superseded for consumers by the P1-4 current capture,
`final-p1-4-v2-20261117T224953Z` / `e3730b90`; the earlier `final-p1-4` capture is historical
and known-bad. To be unambiguous: **v4 is not, and never was, a phase acceptance** — it is a
frozen review/consumer composition, not full CI, and not a replacement for the immutable P1-2
capture:
`/tmp/sage-db-p1.2yjLG4/p1-4-root-review/accepted-components-base-v4`, manifest `1e89e6d3`
with **280 payloads** (paths relative to its `repo/`), root-verified exact bijection with
zero included-MAIN drift, and `cargo check -p sage-db-store -p sage-db-store-tests --locked`
exiting 0. It supersedes v2/v3 **for navigation only**. It carries the reviewed cursor
(`aca9`/`b1cd`), the approved provider and CI (`4416`), lease `59752`/`197ef2` with the
measured `lib` `b216`, and attempts `54`, but **no** commit, init, prepared, handle or pin
candidate. Consumer rule: copy this whole base once into a new writable candidate — provider,
lease and reviewed cursor are **already included**, so do not re-overlay older
provider/lib/types/model files; add only your named delta and union its module wiring while
preserving lease and every accepted component. Init/prepared used by an unreviewed candidate
must be named candidate-only, never silently accepted through the base. The base itself
carries reviewed cursor evidence outside the production source tree, which is not an
integration test.

## Current verified subset and open work

Shared primitive, field, registry, Term/Assertion and Claim/Evidence/Link corrections
were independently checked on identified captures in `/tmp/sage-db-p1.2yjLG4`.
See `RECORD-FOUNDATION-MILESTONE-RECHECK.md` and
`CLAIM-EVIDENCE-MILESTONE-RECHECK.md` there.

Collection source `e2b590ff` with `creation_timestamp_parent` `d06cbf4f` is
**verified fixed**, not P1-2 acceptance: the original 17 collection assertions and
the 2 first-revision timestamp consistency tests pass 19/0 on the identified seed
(`parallel-01/seed-recheck.log`). Baseline maintainer independently re-measured the
whole seed format suite: 135 unit + 49 integration assertions, 184 passed, 0 failed,
per-target counts in `parallel-01/baseline-evidence/seed-format-tests.log`.

Harness: the live tree carries `codec_fixtures` `5d52d139`, whose
`independent_extensions_encode_vector` module is **accepted as bounded independent
byte evidence** for the represented TermCreated extension case (tag 14 hand bytes,
key byte order, exact opaque values, legal empty value). Capture
`parallel-01/rechecks/boundary-v3` binds it and the boundary probes:
`COMPOSITION-SHA256SUMS.txt` 179 payloads, paths relative to `sage-db/`, verified
0 missing / 0 mismatches; `../extension-tests.log` records 3 passed / 118 filtered.
This is not whole-harness evidence and not P1-2 acceptance. Earlier
`codec_fixtures` `65990182` / `23674a28` are historical. Two distinct identities
matter and are never interchangeable: the **delivered** `codec_fixtures` is
`95d43cc7` (capture `parallel-01/rechecks/vectors-95d43cc7`, 179-payload manifest
relative to `sage-db/`, verified 0 missing / 0 mismatches), and the **live** file is
currently `5d52d139`. Frozen source/test evidence binds the exact composition hash, so
the live file is not claimed to match it.

On `95d43cc7` the parent accepted **bounded module evidence**, t61 may integrate:
`independent_assertion_revision_vectors` (assertion-tests.log, 4 passed / 124 filtered)
and `independent_legacy_provenance_vector` (legacy-tests.log, 3 passed / 125 filtered).
Hand maps, derived diff sets and the five legacy tags (10, 11, 12, 169, 170) were
reviewed; the derived sets are `[2,9,21]` revised and `[2,4,7,9]` retracted for
Assertion, and the Term revision vectors remain `[2,9,16,21]` / `[2,4,9,16,21]`. No
whole-harness total is claimed (the root count of 128 is not evidenced here).

Digests `state` `735250e5`, `graph` `8ef1ebad`, `bookkeeping` `841daf59` carry **accepted
independent bounded evidence** (t67 REPORT-05 `a61828af`, root-accepted): nine golden
values were rederived from the frozen D10 transactions with independently written
projection/fold code and only then fed to the delivered entry points, and the author's
fixture bytes reproduce the same nine through a disjoint hashing path. Ordering
discriminators are genuine permutations, including ids above `0xff` where byte order and
numeric order actually diverge. Scope of that credit, stated to prevent over-reading: the
assembler accepts **opaque well-framed projected bodies**; it does **not** establish
per-family record admissibility under §7.2 (records lacking `144..148`, a status-only Term
body, or out-of-range `152` are accepted here by design). `Structure` remains a
`ReportDefect` and is never remapped to file corruption. Live formatted copies keep
separate exact identities — the delivered hashes are not asserted for the live tree, and
the formatting-only observation is an inference, not hash authority.

Natural `bf9ae4de` / `a72596bf` and structural `3b838c2b` / `6d525ad1` with the corrected
20-fixture set now equal the **live** files (verified against the tree). Their component
review is **closed** on t67 REPORT-02 and REPORT-06 independent bytes, bounds, refusal and
projection checks plus the root 192 result at 50/0. That closes component review only — not
final integrated P1-2 acceptance, which stays open.

Dispatch representation: on capture `parallel-01/rechecks/dispatch-inline-and-rulesref`
(182-payload manifest relative to `sage-db/`, verified 0 missing / 0 mismatches; dispatch
`5f3356b0`) the previous **allocation issue and the tag `166` omission are both VERIFIED
FIXED on that exact source**. `DispatchedPayload` now holds records **inline**; the
payload is no longer boxed and no allocation helper remains, so the earlier
`admit_box`-with-infallible-`Box::new` defect no longer applies to this representation.
The Asset probe admits exactly its five input `Field` slots, succeeds with zero allowance
remaining, and type-checks the returned variant as `AssetRecord`, which would not compile
for `Box<AssetRecord>`; the direct decoder control also passes. Separately,
`for_each_blob_ref` now enumerates tag `166` on a real family-accepted Genesis event, where
the old capture returned an empty ref list. These are source, type and accounting checks —
**not allocator instrumentation**, and not an RSS claim. `tests.log` exits 0 with 2
inline/control tests and 1 actual rules-reference test; zero-test lib/doc targets establish
nothing additional. The old boxed probe stays historical in `boundary-v3`. Earlier
namespace-gap fix on `7275c283` is superseded by this source.

C1-C4 rulings are reflected as live-only document errata in `storage-format`
`68e5d4a1` (verified present): tag 14 digest exception (§8.4 block), `176` genesis
watermark exception, `tx_header 144` exception, and the `99` Vector-only / `152`
Model-only split. C5 is **withdrawn** as a false 7-vs-6-column finding; the normative
event table was not edited. Event 192 ruling iii is **verified fixed** as a narrow
correction on capture `parallel-01/rechecks/outcome192-v2` (180-payload manifest relative
to `sage-db/`, verified 0 missing / 0 mismatches; natural `bf9ae4de`, test `a72596bf`):
`cargo test -p sage-db-format --offline --test natural_codec` recorded 50 passed /
0 failed, exit 0 (`tests.log`), and t67 REPORT-06 (`2184efa2`) independently agrees at
50/0 from both the writer path and raw-byte mutation, so a file already holding an image
no current writer would emit is also refused. Source review confirms tag `5` is in
`OUTCOME_REQUIRED`, enforced on both encode and decode, the global actor/recorded_by join
unchanged, origins 0/1 with `5` accepted, origins 0/1 without `5` and origin 2 in either
form rejected with nothing published. Live now matches this capture, so the 192 correction
is integrated; the component-review closure above still does not constitute P1-2 acceptance.

New released scope, no normative change: pure TX/MANIFEST writers assigned to t62;
canonical bounded opaque request frames plus their hash under `storage-digests` §4.7
assigned to t65; pure rules blob verification assigned to t66 (`rules_read.rs`), since the
existing `rules.rs` is encode-only. P1-4/P1-5 remain held and P1-2 stays incomplete.

t61's request file `f250e2d9` (`format/src/request.rs`) is **present but unreviewed**; t67 is
working known diagnostic and frame-validation leads on it, so no acceptance is recorded here
and any request corrections remain open. Also still open: TX writer/validator, `rules_read`,
and t63's fixture corrections. Before the final freeze, formatting drift in the P1 files must
be fixed with targeted formatting — it cannot be waived as unrelated baseline.

**Correction to an earlier measurement here.** The paragraph above previously reported
"219 hunks across 219 distinct files, 182 src / 37 tests, 214 format / 5 types". That was
wrong: `219` was the count of `Diff in path:line` **headers**, and the header's line number
was being treated as part of file identity. Numbered headers are not distinct files, so no
per-crate or src/test split was valid either. Preserved here rather than silently replaced
because those figures may appear in earlier messages.

Corrected scope, from the current live `cargo fmt --all -- --check` capture
`parallel-01/final-fmt-scope.log`: **239 headers, 29 unique file paths**. The coordinator's
capture and an independent live run by the maintainer produce the identical 29-path set,
so this is a real measurement, not a projection. All 29 are newly added or modified P1
implementation and tests — 14 `format/src` (including `rules_read.rs`), 13 `format/tests`,
and 2 `types/src` (`ledger.rs`, `persist.rs`), which sums to the 29 — not unrelated
pre-user baseline. There is no tolerance decision outstanding: the sole integrator may
format every authorized P1 file before the final freeze, with assertions and fixtures
unchanged and the new source hashes recorded. Totals are not comparable across the two
captures because the sources differ.

Candidate `160507a2` (with its `33031f30` 14-family companion) is **NOT accepted**: its
fixtures carry `changed_fields` inconsistencies and a reversed profile with wrong rules
hashes, so one correction is assigned. The prior `95d43cc7` vector acceptance stands
unchanged.

Corrected candidate `17d91fc1` / `72ad0087` (`parallel-01/rechecks/families160-corrected`,
183-payload manifest relative to `sage-db/`, verified 0 missing / 0 mismatches) is
**UNACCEPTED — acceptance was WITHDRAWN by the coordinator.** The 5 `rule160` and 23
`family` tests genuinely pass (151 and 133 filtered, both exit 0), but the separate claim
that all four rules identities were independently reproduced was false. Cause, recorded
exactly: the coordinator sent acceptance before inspecting a Python constant check that
exited 1. That is a reporting-sequencing error, **not** a product regression.

Defect is in the fixture constant, not the codec. `RULES_DIGEST_FORMAL` in
`codec_fixtures.rs` is a 32-byte array whose bytes diverge from the actual frozen D10
`golden.rules.formal.bin` domain digest at element 11 (a dropped byte shifts the tail);
the other three constants (`RULES_REF_FORMAL`, `RULES_REF_LEGACY`,
`RULES_DIGEST_LEGACY`) match. The maintainer recomputed all four from the actual D10 blobs
with independent Python and confirmed the three matches and this one divergence. Both the
record and the expected wire bytes reused the wrong constant, which is why the passing tests
could not catch it. t63 is assigned the constant correction plus a control bound to the two
actual bundled D10 rules blobs with provenance; family `72ad0087` is unchanged. Capture
source and logs are preserved. Prior `95d43cc7` acceptance stands.

**Final `dbf903af` candidate is ACCEPTED bounded**; t61 integration assigned. Capture
`parallel-01/rechecks/families160-dbf903af`: 185-payload manifest relative to `sage-db/`
(verified 0 missing / 0 mismatches), `codec_fixtures` `dbf903af` with family `72ad0087`
unchanged. `rule160-tests.log` 5 passed / 151 filtered, `family-tests.log` 23 passed / 133
filtered, both exit 0. The maintainer re-derived all four identities independently from the
actual frozen blobs — `RULES_REF_FORMAL` `04342325`, `RULES_DIGEST_FORMAL` `19fe4869…76ded`,
`RULES_REF_LEGACY` `b6f92f3c`, `RULES_DIGEST_LEGACY` `b4f0366e` — and all four now match the
capture's constants, which is the correction of the withdrawn defect above. The source now
carries a real-blob binding control: `rules_identities_are_bound_to_the_actual_d10_blobs`
recomputes `raw_sha256` and `digest(Domain::Rules, ..)` from two vendored 381-byte blobs via
`include_bytes`, and its own doc comment names the earlier constants-compared-to-constants
check as worthless — so the self-consistent bad-literal failure mode is now caught rather
than trusted. The `17d` withdrawal record above stays as written; history is not rewritten.

No whole-`156` root claim is made. Coverage, precisely: these represented expected-byte cases
plus the earlier 6 Term/Assertion and the reviewed 10 natural/structural span all 30
registered event tags, with `160` overlapping between sets. Optional-value coverage remains
bounded, and no P1-2 acceptance follows. Minor provenance path/offset-label corrections are
assigned to t61 and touch documentation only, not source.

Request component is **review-accepted bounded** on `5bdd63b2` / `request_frame.rs`
`7896d644` (both equal the live files), via a fresh affected matrix plus exact D10 v2
bytes/digest and source check. Semantics recorded as verified: canonical version `0` is
malformed and `>= 2` is unsupported (distinct errors); count, payload and frame limits are
symmetric across read and write; opaque operation bytes are preserved. `digest_of` is a raw
digest with an explicit already-validated-frame precondition, so it is not a validation
entry point. One stale attribution is **withdrawn, not open**: current source reports
`RequestLimit::FrameBytes`, so the earlier `CurrentFile` residual is a reviewer artifact and
must not be carried as a finding. t61 integration assigned.

Rules component review is **COMPLETE / component-accepted** on `rules_read` `8e3956e2` /
`rules_verify.rs` `4376f9be` with 28 fixture blobs, on the bounded REPORT-09..16 chain
(parameters, hash, profile, resource, str/list limits, variable control, and the rule-10
correct domain). All previously known findings in that chain are closed, including the
`cardinality == 1` classification, string/list limits and variable control. The integrated
tree now carries the final `rules_read` `dd3a45a1`, which differs from the accepted
candidate `8e3956e2` only in public documentation of a private method; `rules_verify.rs`
`4376f9be` and all 28 accepted blobs are unchanged.

t63's `dbf903af` four-constant check now actually passes and is accepted separately, as
recorded above.

File component review is **COMPLETE bounded** on capture
`parallel-01/rechecks/files-current-v5` (237-payload manifest relative to `sage-db/`,
verified 0 missing / 0 mismatches; `current` `a670c08d`, `transaction` `7cf2582c`, `encode`
`6346e354`, all three equal to live). `files-tests.log` exits 0: composition_rules 9,
current_codec 15, encode_codec 20, transaction_and_manifest 26, each 0 failed.

Both earlier CURRENT write defects are **VERIFIED FIXED**, with the privacy claim kept
narrow. `private-version-check.log` shows the intended negative build failing with
**E0616** (`version` of `CurrentToEncode` is private), which proves only that a **direct**
mutation of `CurrentToEncode.version` is unconstructible — the old root capture instead
compiled its mutator and emitted 0. It does **not** prove that every `version = 0` write
input is impossible: `DecodedCurrent.version` is public and `from_decoded` copies it
straight into the encode input. That route is closed by the encoder's own explicit `version
== 0` recheck, which returns a typed refusal rather than emitting; behaviour is correct, so
the component acceptance stands. t62 is adding one focused runtime control for that path plus
a doc clarification. The non-empty-`out` guard likewise returns a typed `Internal` refusal
**before** any charge, reservation or mutation and leaves the buffer untouched by design.
Transaction arms were additionally reviewed independently by t66.

This completes the file codec component review. The rules string/list-limits and variable
control work that was pending here is now closed by the accepted rules component above, so
no component item remains outstanding; what remains is the phase-level record in
`P1-2-ACCEPTANCE.md`.

Correction of record: t65's preflight "no encoder" conclusion was bound to the obsolete
outcome-192 capture, not to a current live blocker — the TX/MANIFEST writers are already
integrated. That preflight result must not be read as a present gap.

## Parallel ownership

All new implementation lines use independent source copies and target directories.
They deliver file-scoped patches to t61. Only t61 integrates production changes.
No agent starts recursive workers or edits peer-owned evidence.

| Owner / line | Exclusive implementation scope |
|---|---|
| t61, shared spine/integrator | shared production declarations, format helpers, exports, manifests, lockfile, dependency gates, live integration; P1-4 shared integration |
| t62, Store author (P1-4) | **sole author** of store implementation modules and its own tests, in an isolated copy of the final accepted composition; no concurrent writer of one module |
| t63, verification | independent `store-tests` model/oracle; derives expected states from external attempts, ACKs and the independent model, never from production fold/replay as sole oracle |
| t65 | read-only boundary review on frozen milestones; no Store modules |
| t66 | format integration issues only; retains completed digest and rules-verification scope |
| t67 | read-only candidates; probes/reports in own evidence copy |
| Baseline maintainer | this index and the two project skills; proposes normative changes, never approves them |

Module entry wiring in a private copy may be used only as explicitly assigned and
is reported separately. Canonical declarations are single-owner; no placeholder
type copies or same-name adapters to make a consumer compile. New interfaces are
delivered as real buildable crate declarations before consumers claim binding.

## Shared skills

Read these explicit paths even if the runtime has not discovered them automatically:

- `/Users/binzhan/.codex/skills/sage-db-implementation/SKILL.md`
- `/Users/binzhan/.codex/skills/sage-db-verification/SKILL.md`

Skills define work methods; normative tables stay in the design documents. A local
bug belongs in a regression test, not automatically in a new global rule. The
baseline maintainer batches relevant changes and is woken by coordinator messages;
no automatic hook/daemon has been installed. Candidate findings, verified fixes
and coordinator acceptance remain separate states. Maintainer reports and measured
logs stay under `/tmp/sage-db-p1.2yjLG4/parallel-01/baseline-evidence`; they are
session evidence, not runtime inputs.


## 2026-11-19 continuation — session lifecycle and identified-record reads

P2 remains IN PROGRESS. The next bounded composition adds canonical SessionCheckpoint
128 preparation, write_aux create/revise with exact CAS and outcome-first retry,
separate read authorization with retained hash-verified body, and budgeted numeric
record traversal for later graph/query assembly. These are progress under the
user's autonomous-readiness instruction, not complete P2/P3 or cutover acceptance.
The shared byte-copy helper now admits its u8 slot extent once; previous conservative
double charging was not a resource claim to preserve. No P1 byte codec/schema changed.

The real source is the x99 Markdown KB (41 concepts/173 authored edges), not the
local22/94 subset. A verified73-file read-only copy exists as preparation; final
activation still requires write-stop source recheck and complete lossless mapping.
Session exporter and Linux/Btrfs support work occur in separate isolated candidates;
neither in-progress component is accepted by this paragraph. Old captures remain
immutable and their counts retain their original source identities.
