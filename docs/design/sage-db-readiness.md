# sage-db real-KB readiness

Updated: 2026-11-20. The user authorized continued implementation and a complete
real-KB switch when ready. x-cart was declined earlier for this work.

## Decision

**Not ready for activation. Continue kernel, query, service and migration implementation.** P1 acceptance covers
its bounded software/storage scope; it is not acceptance of the business kernel,
service or migration tooling. The later named x99 OS policy is a separately qualified
component, bounded to its kernel/filesystem/device assumptions.

The local registry currently names only `kb/default`. At assessment it has no
`knowledge.db`, so `packages/service/src/authority.ts` and `store/schema.ts` select
Markdown authority. This is a local-file observation, not an inspection of a
running deployment. No real KB data or routing was changed.

## Actual deployment source (read-only observation, 2026-11-19)

The active target is x99: Linux7.2.11-arch1-1 x86_64 on Btrfs, serving the
Markdown-authority KB at `/opt/xartifact/x-sage/data/kb/default` through the
existing container. It contains41 concepts/173 authored edges,15 proposal files
and5 checkpoints. The local22-concept/94-edge repository is a stale subset;
19 concepts exist only on the server. It must never be used as the complete
migration input.

A read-only preparation capture contains all73 source files/335915bytes. Remote
pre/post inventories and every transferred byte matched; this is a backup/input
snapshot, not the final write-stop activation freeze. No service/source write was
made. Session evidence and source hashes are recorded in
`/private/tmp/sage-db-real-cutover-20261119` (inventory6f2d7436 and capture1b645c8d).
A checked `OsFileSystem::for_root` policy now binds the named x99 kernel/Btrfs context;
it does not override the conservative backend or create a generic Linux allowlist. The
qualification harness exercises actual Store operations and process recovery over private
real files on that device. Device power-loss remains outside the measured evidence.
A caller-bounded source archive and self-contained native preparation preserve all source
files. Staged import verification now passes private model and x99 Btrfs rehearsals. Runtime service,
backup/restore tooling, final source write-freeze and production activation remain incomplete.

## Required sequence

| Boundary | Current state | Exit before activation |
|---|---|---|
| P1 persistence | Accepted bounded software scope; draft major 0 | Preserve its semantics and real-byte recovery regressions |
| OS backend | Checked `for_root` for linux/x86_64 kernel7.2.11-arch1-* on actual Btrfs; conservative/global policy remains NotClaimed | Keep the named kernel/mount/device assumptions; qualify exact deployed bytes and container context; no power-loss claim |
| P2 kernel | KB authorization, budgeted traversal, Claim/existing-Term text and RelationAssert proposals with committed-profile graph validation, auxiliary session/Frontier lifecycle, CAS/retry and atomic decisions | Remaining typed lifecycle, including Term creation/retirement and V7 lifecycle/reference obligations; complete read/write sets and honest conflict outcomes |
| P3 query | Four-family SnapshotView, verified Rules/graph identity, complete fact queries, unfiltered Requires ordering, authored shortest navigation and authorized borrowed catalog; no service/query transport | Bounded queries, search and historical access with consistent snapshots |
| P4 service | Credential verifier, generated protocol, eight-command adapter and canonical response assembly exist; running service remains the existing Bun Markdown path | Real Rust server and authenticated, versioned write/read protocol plus REST compatibility, with explicit sage-db engine selection |
| P5 migration | Archive, full self-contained native preparation and guarded staged executor adopted; private model/Btrfs restart-verification rehearsals pass; no production target activated | Full lossless mapping, frozen source inventory, per-object counts/hash checks, restart verification and controlled activation |
| Format promise | Major 0 | Satisfy storage §4.4 including scoped migration tools before claiming major 1 |

`Store::record` is a borrowed typed read, gated on the SAME durable
publication and coverage as outcome lookup, with actual caller-ledger checkpoints
and entry charges for searched rows. It does not decode a second wire format or
expose a separate publication slot.

`kernel::review::assess_approval` reads the actual stored pending Proposal and its
baseline on one immutable Store borrow. Its required existing-object dependencies
must still be derived by the future payload planner. It returns an assessment,
not a committed decision. The separate `claims` command family now implements
Claim text-edit proposal submission and review. It derives dependencies from the
actual stored payload, rejects unsupported pinned expected versions, and writes
Claim/Proposal/outcome atomically. Rejection or a stale baseline is a committed
Proposal decision with wire result_kind 0; it does not extend the outcome enum.
Exact retries return the original version/payload before stale-state checks.
Auxiliary session checkpoints now have explicit WriteAux create/revise and separately Read-authorized full-content access. Their CAS and real Store outcome cover restart/retry. Existing-Term atom/body proposals and Frontier auxiliary lifecycles are now implemented; other record/graph lifecycles remain absent. A hash-only per-KB credential library exists; runtime credential loading/issuance and HTTP service remain absent.
Ordinary commands do not scan other Pending proposals.

The canonical typed-event bridge delegates to the existing family codecs and
retains exact encoded field bytes. Committed blob reads verify actual returned
bytes and carry a real retained reservation beyond attempt-facade drop. Known
committed damage invalidates normal serving; ceilings, real budget/control,
filesystem transport and internal defects remain separate.

See [kernel-operations](kernel-operations.md) for the explicit bounded operation
registry. This is P2 progress, not full P2 acceptance or activation readiness.

## Activation rules

Follow [migration.md](migration.md): prepare → validate → import-to-new-root →
verify → activate. Preserve all record families, audit, advisory bytes and blobs;
Markdown projection is not a substitute for an authoritative structured export.
The destination stays unrouted until import completion and all reconciliation
checks pass. Freeze writes and recheck source identity before changing routing.
No dual write or silent fallback to Markdown/SQLite is permitted.

Before new sage-db writes, activation may be cancelled back to the frozen source.
After new writes, restarting an older source snapshot loses transactions and is
not rollback; use verified sage-db recovery. No such activation has occurred.

## Evidence limits

Current checks cover isolated implementation and represented fixtures. Model
crash worlds, process kills and OS advisory-lock checks have separate scopes;
none establishes platform power-loss behavior, RSS or all-input allocation bounds.
P1 captures remain immutable; this index and later P2 work have new identities.

The staged-import guard is persisted: an unfinished ImportSession prevents new ordinary
claim/session commands and session reads after restart. An exact prior committed request
still returns its original result. The guard is not itself an administrative importer or
authentication implementation. P2/P3/P4/P5 are not declared complete by these components.

## Prior bounded capture

`final-bounded-snapshot-auth-20261119T182743Z` has572 source payloads (manifest e930a9f0)
and158 evidence artifacts (34bf5eb3). Local CI1303Rust/0,473Bun/0 and52contract/0
(overlapping), strict docs/toolsTS0; identical source on x99 passed workspace Rust,
6 policy and5 real-file Store/process cases, lint/fmt/docs, source checks before/after.
This is component integration, not activation readiness. Three navigation docs changed
after the capture; runtime source remains bound to it. Source reviews are not runtime
counts and all counts are bounded to their actual targets. No production data or route
was changed. Delegation is restricted to omp/pi; agy is no longer used.

## Prior bounded adoption (2026-11-20)

Capture final-bounded-text-frontier-20261119T191342Z binds581 source payloads
(MANIFEST b6fe81bd, EVIDENCE ec203ed9/141, CAPTURE955e62b9). Source integration15paths
=6existing+9new,566 outside unchanged. Exact local CI Rust1394/0 across95 summaries,
Bun473/0, separate contract52/0 overlaps main Bun, docs/toolsTS0. The same581 on x99
passed Rust1394/0, qualification6/0 and real-file5/0 with581 before/after and lint/docs0.
Concept text/Frontier commands and owner-backed current Term/body/all-current scans
are bounded additions, not full P2/query/service/native import readiness. Input plan
still has138 explicit target requirements. No production route or data write occurred.
Zero source drift is an AT-CAPTURE observation; this navigation edit follows it.

## Prior bounded adoption: 602 source payloads (2026-11-20)

Capture `final-bounded-protocol-query-command-20261119T202537Z` binds MANIFEST181ae950/602,
EVIDENCE16c23fb9/143 and CAPTURE9f4ab131. Local CI Rust1486/0 over103 summaries,
Bun475/0 and contract54/0 (overlapping), strict docs/toolsTS0. Fresh x99 verified602
before/after and passed Rust1486/0, policy6/0, actual-file/process5/0, lint/fmt/docs0.
Protocol/query/command integration is bounded source adoption. No listener, response assembly,
complete native import/verification, phase acceptance or cutover is claimed. The separately
authored mapping and response candidates remain held until their own findings close. Three
navigation files changed after the immutable capture; the remaining599 payloads stay exact.
Real x99 Markdown41 remains authority, with no production data, service or route writes.

## Prior bounded adoption: native preparation and response assembly (2026-11-20)

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

## Prior bounded adoption: native execution, catalog and Requires (2026-11-20)

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


## Latest bounded adoption: relation proposals (2026-11-20)

The current capture is `final-bounded-relation-proposals-20261120T014228Z`:
MANIFEST99ec71a9/628, EVIDENCEf3e63cac/802, CAPTURE89b314aa. Root adopted9 paths
(6 existing +3 new), preserving619 outside existing payloads. Local and exact-source
x99 runs each passed Rust1675/0 over114 summaries, with12 ignored. Local Bun476/0
and contract55/0 overlap; strict docs/toolsTS0. x99 policy6/0, actual-file/process5/0,
lint/fmt/docs0 and73 artifact hash comparisons passed. The initial log pull255 remains
recorded; verified recovery copied the unchanged12 logs without rerunning tests.

RelationAssert op10/11 now submit and review authored edges through persisted intent,
endpoint baselines and the committed profile's canonical graph validator. Valid approval
writes Assertion, Proposal, allocation and outcome atomically. Declared attribution is
separate from reviewer identity and accepts empty text through the canonical800-byte
ceiling. Kernel's types/store/infer dependencies and their exact CI contract agree.
The prior executor/catalog/Requires/Navigation components remain byte-identical.

This does not finish P2: Term creation/retirement and other lifecycles remain open.
V7 lifecycle is normative and belongs to the kernel; infer's V1–V6 graph results do not
replace it. Descriptor ingress and offline backup/restore remain isolated candidates,
and no Rust HTTP/RPC runtime is adopted. The read-only x99 recheck at
2026-11-20T01:37:46Z still found73 files/335915 bytes, no drift and no knowledge.db.
No final write-stop freeze, production write, phase acceptance or cutover occurred.
Only three navigation documents change after this immutable capture.

## Prior bounded adoption: authored navigation (2026-11-20)

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
