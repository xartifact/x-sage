//! Native import preparation and staged execution.
//!
//! The mapper builds canonical records and a self-contained source package without opening
//! a Store. The executor writes an already-opened Store under administrative authorization
//! and verifies before completion. Source selection, the fresh-process driver and
//! production routing remain separate responsibilities.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod execute;
pub mod intent;
pub mod mapping;
