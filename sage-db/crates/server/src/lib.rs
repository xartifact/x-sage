//! Server-side credential verification for sage-db.
//!
//! This crate is an early P4 library slice covering **borrowed credential binding
//! validation** and **bearer token verification** that yields canonical
//! [`sage_db_kernel::auth::Grants`] values scoped to a single [`sage_db_types::identity::KbUuid`]
//! and explicit [`sage_db_kernel::auth::CapabilitySet`].
//!
//! Deliberately out of scope for this slice (do not treat this crate as complete P4):
//!
//! * HTTP/1.1 or HTTP/2 transport, protocol framing, or route handlers;
//! * file, environment, or configuration parsers;
//! * high-entropy opaque token generation / issuance (a future named dependency).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod commands;
pub mod credentials;

pub mod responses;
