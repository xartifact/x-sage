# Navigation query — bounded shortest paths over the authored base graph

Status: **use-case slice**. Not inference, not a Fact, not a proof, and not a
readiness/cutover statement.

Code: `sage-db/crates/query/src/navigation.rs`, tests
`sage-db/crates/query/tests/navigation.rs`.

## 1. What it answers

One question, and only one:

> Between these two active terms of *this* committed snapshot, is there a route of authored
> assertions, and if so what is one shortest route?

That is the Rust counterpart of the shipped
`packages/core/src/graph/traverse.ts` `shortestPath` and `packages/service/src/query.ts`
`findPath`, restricted to what a committed snapshot actually contains.

```
shortest_path(view, requested_snapshot, from, to, budget, control) -> Result<Navigation, NavigationError>
```

## 2. What it is not

Per `query-dsl` §3 a bounded navigation "uses a separate operation and its result is labelled a
path, not a Fact"; per §5 and `axiomatics` §5 a navigation path "must not be output as a new
semantic fact". The type reinforces that:

| Not this | Why |
|---|---|
| a `FactRow` / `SourceResult` | a path is not a derived relation between the endpoints |
| a `ProofRef` / proof arena | no rule was applied, so there is nothing to prove |
| semantic closure | no relation is composed with another |
| R3 virtual edges | only `BaseGraph` assertions are walked; virtual edges are not in a `BaseGraph` |
| display truncation | a short path is not a partial answer, it is a wrong one |

`Navigation` therefore carries **nodes** and **witness assertions**, and nothing in it can be
handed to a fact consumer as though derivation had occurred.

## 3. Traversal

Breadth-first over **undirected** adjacency:

* an authored assertion `from -> to` is a possible step in both directions;
* every relation kind participates, including `contradicts` and `analogous-to`;
* a witness always reports its **authored** direction (see §5).

BFS is what makes the answer shortest. `shortestPath` in the shipped JavaScript is also BFS, but it
pushed `[...current.path, next]` for every candidate, materialising a whole path per queue slot —
quadratic in the worst case. Here the search writes **one predecessor slot per visited node** and
reconstructs the path once at the end.

## 4. Determinism and the tie rule

Equal-length alternatives are broken by the graph's own canonical order, not by caller input
order. For each frontier term the walk scans

* outgoing assertions in `(from, relation, target, assertion_id)` order — the stored
  `visit_key` order, and
* incoming assertions in `(target, visit_key)` order,

and a term is marked visited when enqueued. The returned path is therefore the one whose
predecessors were discovered earliest in that combined canonical scan.

**This is deliberately not claimed to match the JavaScript byte-for-byte.** That implementation
iterated a flat `graph.edges` array, so its tie-breaking depended on the caller's input order. Only
"shortest, deterministic at equal length" is claimed; `Navigation::tie_rule()` states it in the
type, and a test asserts the rule string rather than asserting JS equivalence.

## 5. Witness direction

A step records `from`/`to` in the **traversal** direction, plus:

* `witness: AssertionRecord` — the canonical authored record (identity, revision, relation, and
  the author's own `from`/`target`);
* `reversed: bool` — whether the walk crossed it backwards.

Rewriting a backwards step into a forwards one would attribute a direction the author never
wrote. `axiomatics` §2 is explicit that the reverse reading of an asserted edge is a *derived*
matter, and this module produces no derived facts at all, so it must not fabricate direction
either.

## 6. Endpoints, basis, and refusals

| situation | result |
|---|---|
| requested snapshot ≠ view basis (any of the four fields) | `SnapshotMismatch` |
| endpoint absent from the graph | `UnknownTerm { side, term }` |
| the **same** unknown term on both sides | `UnknownTerm`, never a fabricated zero-hop |
| both endpoints known and identical | a real zero-hop path: one node, no steps |
| both known, no route | `NoPath` — **after a complete search** |
| budget exhausted | `BudgetExceeded(limit)` |
| caller stopped | `Stopped(stop)` |

`NoPath` is the only "no" that means the search finished. Every refusal means it did not, so a
caller can never read a budget failure as "these terms are unrelated".

## 7. Cost — the meter sees the algorithm's shape

One `sage_db_infer::meter::Work` pays for all of it, and the finished meter's own counters are the
result's `ExecStats` (no second meter, no second stats type).

**Membership is a charged binary search, not a linear scan.** The graph already owns a sorted term
table, so discovery and the endpoint checks binary-search it and charge one step per comparison.
A predecessor is located by a bounded scan over the *discovery prefix* (`0..index`), and every one
of those comparisons is charged too. There is therefore no `visited.iter().any(..)` and no
uncharged `position()` anywhere in the module.

**Control is per row, not every 1024 steps.** `Work::check` (not `periodic_check`) runs at the head
of each adjacency row, each binary-search comparison, each predecessor comparison and each queue
pop, so a stop lands inside the traversal even on a small graph.

**Reservation and materialisation are separate events.** A table's **bytes** are charged once and
then `try_reserve_exact` is called **directly** — not through `infer::meter::grow`, because `grow`
reports a failed allocation as `BudgetExceeded`, which would make a genuine allocator refusal
indistinguishable from a budget ceiling. A failed `try_reserve_exact` is
`NavigationError::Allocation { requested }` with the real computed byte figure.

**Over-allocation is refused, not tolerated.** `try_reserve_exact` guarantees *at least* the ask;
capacity above the admitted extent would leave retained bytes unaccounted, so it is
`NavigationError::UnexpectedCapacity { requested, actual }`, matching the graph and snapshot
builders. There is no late top-up anywhere.

**Rows are charged exactly once, immediately before each write.** Reservation charges no rows: an
empty table has none. Every actual materialisation — a zero-hop node, a BFS node slot, a queue slot,
a bitmap word, a reconstruction chain entry, an output node, an output step — costs exactly one row
at the moment it is written. A one-node answer therefore reports `work_rows == 1`, and a
`max_work_rows: 1` budget admits it while `0` refuses.

**The final reordering is attended.** The path is reconstructed destination-first and then flipped by
an explicit swap pass that charges one step and one control check per swap, rather than an unmetered
`slice::reverse`.

All arithmetic on counts is checked (`checked_add`/`try_from`); overflow is
`NavigationError::ExtentOverflow { count }`, never a `u64::MAX` or a saturated figure.

A broken internal consistency check — a chain whose length disagrees with its measured depth, a
reconstructed path whose endpoints are not the request, or an adjacency endpoint that is not a term
of the graph — is `NavigationError::Invariant { detail }`, carrying a static diagnostic so the error
path stays heap-free. It is deliberately **not** `ExtentOverflow`: no arithmetic failed.

## 8. Tests

All cases run against a **real** `bootstrap`ped store → committed Terms and Assertions → kernel
`Snapshot` (`sage_db_kernel::snapshot::Snapshot::open`). No hand-built `SnapshotView`, no session
record, no `/tmp` fixture.

| test | property |
|---|---|
| `a_direct_shortcut_wins_over_the_longer_chain` | 1 hop, not 3 |
| `a_reverse_path_walks_the_same_authored_assertions_backwards` | backward traversal, authored witness |
| `a_mixed_direction_path_is_two_hops` | symmetric per edge, not "reverse the whole path" |
| `every_relation_kind_participates` | `contradicts` is navigable |
| `equal_length_alternatives_follow_canonical_order_not_insertion_order` | a REAL tie: three parallel supports, higher id authored first, canonical-first id required in both directions |
| `the_result_reports_the_stats_of_the_meter_that_bounded_it` | real steps/rows/bytes, zero proof nodes |
| `a_multi_hop_chain_reports_its_exact_hop_count` | count and node count agree |
| `a_known_term_reaches_itself_in_zero_hops_with_one_node` | zero-hop identity |
| `an_unknown_term_is_refused_on_either_side_and_never_fabricates_a_zero_hop` | unknown==unknown |
| `disconnected_known_terms_are_a_complete_no_path` | island → `NoPath` |
| `a_foreign_snapshot_field_is_refused` | basis binding |
| `a_budget_exhausted_after_a_real_prefix_is_a_typed_refusal_not_a_path` | the same query first succeeds, then a partial budget refuses |
| `a_zero_row_limit_refuses_inside_the_scan` | nested row limit, distinct from steps |
| `a_stop_during_traversal_returns_no_path` | stop mid-walk |
| `a_stop_at_the_empty_entry_is_observed` | entry check precedes work |
| `parallel_support_keeps_its_own_witness_identity` | duplicate supports keep real ids |

Seven mutations were verified to fail the suite: **zero-hop double charge** (rows 2 ≠ 1),
**reservation also charging rows** (25 ≠ 19), **over-admitting capacity** (every case refuses via
`UnexpectedCapacity`), **unmetered `reverse()`** (steps 48 ≠ 50), plus the four below: **forward-only** traversal (5 failures),
**returning a longer route** (7 failures, including the shortcut test), **reversed canonical tie
order** (the tie test returns id 60 instead of the canonical 7), and a **zero work-row limit**
refusing at materialisation.

## 8.1 Ownership

`Navigation` is **not** `Clone`. Its nodes and steps are two heap tables paid for once on the
query's meter; a derived `Clone` would duplicate them infallibly and uncharged, letting a caller
hold two copies for the price of one. This matches the fact layer's `Answer`, which retains its
owned result rather than offering a copy. Accessors borrow. `NavigationError` *is* `Copy`, because
every variant is scalar after the snapshot mismatch was reduced to its differing field.

## 9. Limits

* The private root and the model filesystem are test scaffolding. Nothing here is device-durability,
  power-loss, or production evidence.
* No transport encoding, no HTTP route, no `SnapshotView` extension, no caching.
* No phase, readiness, or cutover claim.
* Equal-length determinism is stated relative to `BaseGraph` order; it is **not** a statement about
  the JavaScript implementation's tie-breaking.
