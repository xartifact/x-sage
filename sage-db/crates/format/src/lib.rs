//! `sage-db-format` — explicit byte codecs, canonical digests and format versioning.
//!
//! Pure by construction: no filesystem, network, clock or environment access. The
//! store owns I/O; this crate owns what the bytes mean. That split is what lets the
//! same bytes be produced and checked without a disk.
//!
//! Design authority: `docs/design/storage-format.md` (layout, field and event
//! registries, limits, error classes) and `docs/design/storage-digests.md` (digest
//! domains, coverage, canonical rules bytes, vectors).
//!
//! Versioning: this is `format major 0`. Nothing here is a compatibility promise, and
//! an unknown major/minor must be refused rather than guessed at.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod assertion;
pub mod bookkeeping;
pub mod claim;
pub mod collection;
pub mod current;
pub mod digest;
pub mod dispatch;
pub mod encode;
pub mod event;
pub mod evidence;
pub mod field;
pub mod file;
pub mod graph;
pub mod manifest;
pub mod natural;
pub mod prepared;
pub mod projection;
pub mod record;
pub mod registry;
pub mod request;
pub mod result;
pub mod rules;
pub mod rules_read;
pub mod state;
pub mod structural;
pub mod term;
pub mod transaction;
pub mod wire;

pub use bookkeeping::{bookkeeping_digest, encode_bookkeeping_payload};
pub use digest::{
    BoundedHashError, Domain, StreamingSha256, chain_digest, digest, digest_bounded, raw_sha256,
    raw_sha256_bounded,
};
pub use dispatch::{
    CodecFamily, DispatchedEvent, decode_error_class, encode_internal, expected_record_kind,
    for_each_blob_ref, from_read_error, from_write_error, is_blob_ref_tag, read_dispatched,
    schema_to_format, stopped,
};
pub use event::{
    EVENT_FLAG_REQUIRED, Event, Field, MAX_EVENT_FIELDS, MAX_HEADER_FIELDS, encode_with,
    read_event, read_header, write_event, write_header,
};
pub use graph::{encode_graph_payload, graph_digest};
pub use prepared::{EncodableView, EventView, OwnedEvent, view_for_encode};
pub use registry::{
    ADVISORY_FIELDS, Compat, EVENTS, EventSpec, REGISTERED_FIELDS, RecordClass, SchemaError,
    accept_event, field_compat, is_registered_field, read_accepted_event,
};
pub use rules::{CanonicalRules, Enforcement, RULES_FORMAT_VERSION, VALIDATION_IDS};
pub use state::{DigestAssemblyError, StructureDefect, encode_state_payload, state_digest};
pub use wire::{DecodeError, EncodeError, ReadError, Reader, WriteError, Writer};
