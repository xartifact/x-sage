# `depends-on` requires query

Status: one query on one relation. **Not** the full query DSL, not a display or transport layer,
and not a readiness claim.

## What it answers

`requires(view, query, control)` evaluates `source depends-on ?` completely and returns:

* the existing `Answer` untouched — every row, annotation, `has_asserted`/`has_derived`/`disputed`,
  strongest support and proof root, plus the proof arena;
* `prerequisite_order()` — the reachable prerequisites, **dependent-last**: if `B` depends on `C`,
  then `C` precedes `B`. Ties break by ascending `TermId`.
* `cyclic()` — reachable terms the order could not place, if any;
* `steps_used()` — this call's share of the caller's meter.

The source itself is not in the order: this is the prerequisite set, not the subject.

## One evaluator, one meter

Rows come from `OptimizedExecutor::solve_with_work` through the crate-private
`facts::solve_with_work` seam, which keeps the single snapshot-binding / executor /
result-rebinding path that `facts::solve` uses. The ordinary `solve` constructs one `Work` and
delegates, so there is one implementation of that sequence, not two.

The **same** `Work` pays evaluation, `Adjacency::build`, the seed walk and the topology. There is no
reset, no subtraction and no residual-budget reconstruction — which is exactly why the budget test
can require that the full call costs strictly more than evaluation alone.

## Where dependency edges come from

`Adjacency::build` decides which assertions participate. Under `FormalV1` it also admits virtual
definition seeds (`via_definition`) for `depends-on`; under `LegacyV1` only authored assertions
appear. This module consumes that decision rather than re-deriving it, so it cannot disagree with
the evaluator about what a dependency is, and it never enumerates an inferred closure as if it were
authored edges.

## Ordering rules and what they are not

* Dependent-last topological order over the reachable prerequisite set.
* Ties by ascending `TermId` — a property of the data, not of the executor's internal order.
* Layer order is **not** a topological order and is never used.
* Duplicate supports stay in the `Answer` (they are semantically meaningful). The order is computed
  from **one canonical unique-pair set**, and that same set supplies both the in-degrees and every
  decrement — never the raw seed walk. Retaining a duplicate seed therefore cannot inflate a count
  *or* release a dependent early: with `1 -> 9` twice and `1 -> 10` once, a raw-seed decrement would
  release `1` after `9` and place it before `10`.
* `stats()` reports **total** work through the one meter this call used — steps, rows, bytes and
  proof nodes — captured from that same final `Work`, not summed from per-phase reconstructions.
  `answer().stats()` remains the executor's own phase figures; the two answer different questions,
  and `steps_used()` stays as the narrower statement equal to `stats().steps`. The topology builds no
  proofs, so total proof nodes equal the executor's.
* `unique_pairs()` exposes the distinct-pair count the topology actually used, which is the
  observable a broken duplicate handling changes.
* The prerequisite set is taken from the answer's rows **without re-sorting or deduping**. This is an
  inherited precondition, verified against the producer rather than assumed: `infer::assemble` emits
  one row per reached target (`Reach::touch` pushes each term at most once) and sorts by
  `FactRow::sort_key = (to, relation, from)`; for a single `SourceQuery` the relation and source are
  constant, so the rows arrive sorted by `to` and unique. A charged adjacent strict-order check
  refuses loudly if that ever stops holding, instead of silently reordering.
* Every membership, degree, decrement and tie-break comparison is charged on the caller's meter
  (`step(1)` + control checkpoint), so the accounted cost follows the comparisons rather than the
  edge count. The inner zero-degree scan routes through the charged comparison, so it is attended
  rather than one long uncheckpointed walk. `Vec::remove` tail shifts are charged **before** the
  move, so a budget refusal cannot fire after the memory has already shifted. Control is consulted on
  **every** iteration of both the zero-degree selection scan and the shift pre-charge — not only
  where a tie-break comparison happens to run — so a wide scan over mostly nonzero-degree rows, or a
  large early index, cannot charge without a stop opportunity. A failed invariant (overflow, underflow, or
  a reachable set with no zero in-degree term after canonical validation) is a typed error, never a
  clamped or partial order.
* `max_work_rows` governs topology rows through the **same meter**: each projected prerequisite, each
  in-degree entry, each distinct pair and each emitted order entry charges `Work::row()` *before* it
  is materialised. There is no post-hoc comparison against the original ceiling — that check could
  only fire when the evaluator had already refused, because the evaluator charges every row it
  holds. `max_steps` and bytes are not substitutes for rows.

## Absence, cycles and refusals

* **Absence** is reported only by a complete evaluation. A refusal is never an empty order.
* **Cycles**: the ordering code returns a typed invariant failure rather than a partial order.
  In practice a `depends-on` cycle is not a legal candidate graph under the active profile, so the
  executor refuses it first; the test records that, rather than asserting an order for a graph that
  cannot be committed.
* **Snapshot binding** precedes all work, and the executor's echoed snapshot is re-bound before
  use. All four `SnapshotId` fields are checked, each producing `RequiresError::SnapshotMismatch`.
* **Error classes are preserved**: `UnknownTerm`, `BudgetExceeded`, `Stopped` and `InvalidRequest`
  each surface unchanged; none becomes "absent".

## Scope boundaries

* **`depends-on` only.** Any other relation is refused with `RequiresError::UnsupportedRelation`
  before any membership work: a symmetric or non-transitive relation has no prerequisite order, and
  answering it here would fabricate one. This also keeps a symmetric cycle from reaching an ordering
  branch that cannot produce a truthful answer.
* **Unfiltered only.** The existing `FactQuery` has no confidence field, so a `min_conf` API is
  named **absent** in this batch rather than invented. Every successful answer is complete under
  the existing rules.
* No display, truncation, transport bytes, caching or search.
* Production dependencies stay `{types, infer}`; the real-store fixtures are dev-edges only.

## Testing

The integration tests in `tests/requires.rs` use **real committed records**: terms and `depends-on`
assertions go through the canonical writers into a real `Store` on the model filesystem, and every
assertion is read from a real kernel `Snapshot` over it. No hand-built graph stands in for the
production adapter.

Covered: a chain whose id order disagrees with dependency order; a diamond; a duplicate support;
unrelated terms excluded; an unresolved source; all four snapshot fields; a budget ceiling one step
below the full cost; a stop; a cancelled empty graph; and a cyclic graph refused by the profile.

Controlled mutations recorded in the component deliveries:

| Mutation | Failing test |
|---|---|
| select the lowest id regardless of in-degree | `ids_disagreeing_with_dependency_order_are_not_sorted_by_id` |
| give the ordering phase a fresh `Work` | `budget_exhausted_after_evaluation_refuses_without_a_partial_order` (61 vs 61) |
| disable unique-pair dedup | `duplicate_seeds_do_not_release_a_dependent_before_its_other_prerequisite` |
| delete the topology `row()` charges | `topology_rows_are_charged_after_a_successful_evaluation` |
| sort the final order ascending by layer | `an_ascending_layer_sort_would_reverse_this_chain` |
| sort the final order descending by layer | `a_descending_layer_sort_would_reverse_this_chain` |

The second mutation was **not** caught by my first version of that test — the tight ceiling was
consumed by the evaluator, so a fresh meter for phase two was invisible. The test now measures the
evaluator's own ceiling through `answer_relation` and requires the full call to cost strictly more.

The public after-evaluation cancellation test measures the evaluator's checkpoint count, proves that
evaluation succeeds, then cancels at the next checkpoint in the topology. It does not prescribe a
whole-call checkpoint total. Two private selection regressions separately exercise nonzero-degree
slots and the first zero-degree candidate; both must consult control before selection. The same
helper is called by the public topology loop. Removing its per-slot check must fail those regressions.

Root's correction preserves the R4 source and report separately: that delivery claimed the selection
check existed, but the actual loop still lacked it. Its unchanged mutation count did not establish a
graph property. The corrected helper checks every slot, while the pre-charge loop continues to check
every moved-slot charge before `Vec::remove`.

## Limits

The real-kernel cases run on the model filesystem only: no device durability, power-loss, service,
HTTP, real-KB write or cutover claim follows. Test-local allocations are test-local. The cycle
branch in `requires.rs` is unreachable through a real store, as the test states.
