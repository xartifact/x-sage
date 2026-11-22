//! `sage-db-store` — the filesystem seam, the reservation primitives and durable
//! publication primitives.
//!
//! This crate declares the filesystem seam and its typed failures, and owns the
//! shared reservation accounting an attempt charges against (`lease`). Explicit fresh-root
//! bootstrap and atomic transaction publication are exposed through `init` and `handle`.
//! Existing-root open, full integrity replay, tagged views, resolution and bounded history share that same seam.
//! Store-local recovery performs no business inference or authority repair.
//!
//! Dependency direction, asserted by CI:
//!   store -> { types, format, cap-std, triomphe }
//!   store must never depend on `sage-db-infer` (no inference here) and never on
//!   `sage-db-store-tests` in any edge, including dev-dependencies.
//!
//! `lease` extends the canonical `SharedLedger` instead of redefining it, so the
//! storage types stay dependency-free and every existing implementation is unchanged.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod backup;
pub mod commit;
pub mod fold;
pub mod fs;
pub mod handle;
pub mod history;
pub mod init;
pub mod lease;
pub mod names;
pub mod open;
pub mod os;
pub mod pin;
pub mod prepared;
pub mod typed;
pub mod view;

pub use fs::{
    Capabilities, CreateDir, Each, Effect, Family, FileSystem, FsError, FsErrorKind, HashReport,
    PlatformCapability, PublishMode, Published, WriteOutcome,
};
pub use names::{
    EntryName, FAMILIES, Id128, MAX_NAME_BYTES, NameError, RootName, StagingKind, WalkedName,
    classify_walked, encode_id128, is_valid_entry_name,
};
pub use os::{DurabilityBasis, OsDir, OsFileSystem, OsLock, OsReadFile, OsWriteFile};

#[cfg(test)]
mod lease_tests;
