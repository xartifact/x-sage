//! Business admission primitives for sage-db.
//!
//! Authorization is KB-scoped and capabilities never imply one another. Approval
//! baselines compare the proposal's own object revisions; an unrelated KB commit
//! is not a conflict. These checks allocate nothing and use the caller's ledger.
//!
//! The claims writer composes payload-derived dependency checks and atomic
//! proposal/content persistence for its supported text-edit family. Auxiliary
//! session commands persist full checkpoints with CAS. Other record lifecycles
//! and candidate graph validation remain incomplete. A successful admission
//! check alone is never a durable commit acknowledgement.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
mod candidate;
pub mod review;
pub mod revision;

pub mod claims;
pub mod intent;

pub mod session;
pub mod session_intent;

pub mod snapshot;

pub mod frontier;
pub mod frontier_document;

pub mod frontier_intent;

pub mod catalog;

pub mod term_create;
pub mod term_read;

pub mod concept;

pub mod concept_intent;

pub mod legacy_proposal;
pub mod relations;
