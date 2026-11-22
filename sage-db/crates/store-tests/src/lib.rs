//! `sage-db-store-tests` — independent model, crash oracle and integration harness.
//!
//! **Not production code, not published.** This crate exists so store correctness can be
//! judged against a model that does not share code with the implementation under test
//! (`docs/design/system.md` I1/I2/I14, `P1-IMPLEMENTATION-RELEASE.md` §t63).
//!
//! Dependency direction is one-way:
//!
//! ```text
//! sage-db-store-tests ──> sage-db-store ──> sage-db-format ──> sage-db-types
//!            └──────────────────────────────┴──────────────────────┘
//! ```
//!
//! `sage-db-store` must never depend back on this crate, in any edge including dev.
//! The reverse edge would let an oracle reach production code and would make "independent"
//! meaningless.
//!
//! # What lives here, and what deliberately does not
//!
//! * [`model`] — a filesystem model that keeps **visible** and **durable** state on four
//!   independent axes: inode content, name→inode bindings, a directory's own binding in its
//!   parent, and inode aliasing. It is written as plain data plus pure functions, with no
//!   trait and no I/O, until the real `FileSystem` trait is bound.
//! * [`oracle`] — the legal-recovered-state oracle, derived from externally observed
//!   operations and acknowledgements only. Production replay/fold is never the oracle.
//!
//! There is deliberately **no** store interface, no fake commit implementation and no
//! second ledger here. Tests that need the not-yet-released commit/open API wait for P1-4/P1-5
//! rather than inventing a parallel store.

#![forbid(unsafe_code)]

#[cfg(test)]
mod codec_family_vectors;
mod codec_fixtures;
pub mod faults;
pub mod fixtures;
pub mod model;
pub mod oracle;
pub mod schedule;
