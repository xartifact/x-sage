//! P3 exact facts, dependency prerequisites and authored-graph navigation.
//!
//! # What this crate is
//!
//! A thin, *pure* use-case layer over one immutable [`sage_db_types::snapshot::SnapshotView`]. It answers the
//! three questions the query DSL defines for facts — "what does `source` reach by
//! `relation`", "is this one fact true", and "why" — and it does so by handing the
//! whole evaluation to the real [`sage_db_infer::OptimizedExecutor`].
//!
//! Fact plans own **no** inference. They add no second traversal, graph rebuild or
//! re-derived annotation and no row recomputation here: every `FactRow`, every proof
//! root and evaluator statistic in a fact answer comes from the executor's own
//! [`sage_db_types::query::SourceResult`]. This crate's job is the contract around that call — which
//! snapshot may be asked, which budget is honest, and what an absent point is
//! allowed to mean.
//!
//! # What it deliberately does not do
//!
//! * **No display layer.** `max_rows` and `max_output_bytes` are display limits. They
//!   never shorten a computation and never change a point answer; a caller that sets
//!   `max_rows = 0` still gets the complete row set or a real error. Truncation is
//!   reported by the (absent) display layer, not here.
//! * **No transport encoding.** No JSON, protobuf, DTO or serializer. The result is
//!   the canonical [`sage_db_types::query::SourceResult`] itself.
//! * **Unfiltered `Requires` is supported** ([`fn@requires`]), for the `depends-on` relation only:
//!   it returns the complete answer plus a dependent-last prerequisite order. **No search, no
//!   cache, no all-support detail, and no confidence filter** (`min_conf`) — those are absent and
//!   named as gaps rather than approximated.
//! * **No budget adapter.** [`sage_db_types::ledger::SharedLedger`] governs *snapshot reads*; the inference
//!   budget is P0's [`sage_db_types::budget::Budget`]. They are different currencies and this crate does not
//!   pretend one global workspace budget exists.
//!
//! Navigation separately walks active authored assertions in both directions. It returns a
//! shortest path with original assertion witnesses, never a semantic Fact or inferred edge.
//! Its own steps, work rows and bytes use one canonical meter and caller control.
//!
//! # Snapshot binding
//!
//! A query names a [`sage_db_types::graph::SnapshotId`]. The view answers from its own [`sage_db_types::graph::SnapshotId`]
//! (`SnapshotView::basis`). Every answer is bound to the **view's** basis, and a
//! request naming any other snapshot is refused rather than silently answered from
//! the latest — see [`facts::FactQueryError::SnapshotMismatch`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod facts;
pub mod requires;

pub use facts::{
    Answer, FactAnswer, FactQuery, FactQueryError, SnapshotField, answer_relation, explain_fact,
    point_fact,
};
pub use requires::{RequiresAnswer, RequiresError, requires};

pub mod navigation;
pub use navigation::{Endpoint, Navigation, NavigationError, PathStep, shortest_path};
