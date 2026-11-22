//! The sage-db command protocol: generated Rust types and standard protobuf JSON.
//!
//! Everything wire-shaped in this crate is **generated** from
//! `proto/sage/v1/service.proto` by `build.rs`. Two modules are hand-written:
//!
//! * [`validation`], a transport syntax gate over already-decoded messages — not a second
//!   codec, not a kernel business re-implementation, and not a proof that decoding is bounded;
//! * [`ingress`], the admitted-decoding boundary: it supplies the ledger-backed admission
//!   policy and the descriptor-generated `DeserializeSeed` entry points. It parses no JSON
//!   itself — the vendored patched `serde_json` remains the only lexical authority.
//!
//! # What this crate is not
//!
//! Defining a message or a method here binds a *wire shape*, never a running implementation.
//! No handler exists in this crate, no method is claimed to be supported at activation, and
//! nothing here authenticates a caller, allocates a record id, validates a business rule or
//! touches durable state. Those remain the server's, the executor's and the kernel's.
//!
//! # Dependency direction
//!
//! `protocol -> {sage-db-types, serde plumbing}`. The edge to `types` is deliberate: the
//! canonical ledger, refusal and record-family vocabulary lives there, and re-declaring any of
//! it here would create a second authority whose refusals no call site could match. `types`
//! itself stays dependency-free — this edge adds nothing to it.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod ingress;

pub mod validation;

// Both generated files share one module, exactly as `pbjson-build` documents: the serde
// implementations name the prost types in their own module scope, so splitting them into a
// nested module would leave those names unresolved.
/// Types generated from `sage/v1/service.proto`, plus their standard JSON support.
///
/// Do not hand-edit: `build.rs` runs both generators over the single proto authority, so
/// these are always derived from the same descriptor set.
///
/// # Lint allowances, tightly scoped
///
/// `missing_docs` is allowed because the generator emits one field per proto field and cannot
/// carry Rust doc comments onto each; the documentation lives in the proto source, which is
/// the authority. `clippy::all` is allowed for the same reason — `pbjson-build` emits
/// `write!(…, &FIELDS)` and similar shapes that current clippy flags as redundant borrows in
/// *generated* code. Neither allowance is a judgement about hand-written code: every
/// hand-written item in this crate is documented and clippy-clean, and the workspace's
/// `-D warnings` gate still applies to all of it.
///
/// The alternative — post-processing generated output — would mean editing what the generator
/// produced, which is exactly the drift this crate exists to prevent.
#[allow(missing_docs, clippy::all, clippy::pedantic, clippy::nursery)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/sage.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/sage.v1.serde.rs"));
}
