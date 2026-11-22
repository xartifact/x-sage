//! Opt-in fallible, pre-admitted decoding over `&[u8]` slices.
//!
//! # What this module is for
//!
//! The ordinary JSON decoder allocates freely: the parser's internal string
//! scratch grows with `Vec::push`, and every error is built by `Box::new`.
//! Both are fine for ordinary callers, and both are unacceptable for a decoder
//! that must obey a resource contract: growth has to be *admitted before it
//! happens*, and a refusal must not itself need memory.
//!
//! This module supplies three things and nothing else:
//!
//! * [`Admission`] — the caller's admission policy: one callback that grows the
//!   scratch buffer **fallibly** (checked arithmetic, then `try_reserve_exact`).
//! * [`Admission::checkpoint`] — a bounded progress hook, so a caller can cancel
//!   during long lexical work rather than only at scratch growth.
//! * [`unwind`] — an allocation-free error constructor for a seed's own
//!   refusals, classified as [`DataKind`].
//!
//! # How it composes with the ordinary decoder
//!
//! [`crate::de::Deserializer::from_slice_with_admission`] returns a
//! deserializer whose parser routes every *syntax* error through the inline,
//! allocation-free error arm and whose string parsing draws scratch from
//! the reader's admitted scratch. Everything else — the grammar, the tokens, the
//! `Visitor` protocol, the accepted spellings — is the same code the default
//! build uses. There is one JSON parser in this crate, not two.
//!
//! # What is deliberately *not* here
//!
//! No per-message table, no schema knowledge, no DTO. This module cannot know
//! which fields a message has; a descriptor-driven seed (a separate component)
//! turns these primitives into a typed decode.
//!
//! # Cost of the mode
//!
//! Compiling with the `admission` feature adds a second arm to the private error
//! storage and one discriminant to `Error`. **`Error::custom` changes under the
//! feature**: it stores a classified [`DataKind`] inline and discards the message
//! instead of formatting one, so generic refusals allocate nothing — and ordinary
//! data diagnostics lose their formatted text. Cargo features unify across a
//! dependency graph, so that change is visible to every crate in a build that
//! enables `admission`; classification stays `Category::Data`. With the feature
//! **off**, `custom` formats and boxes exactly as upstream.

use core::cell::RefCell;
use core::fmt;

use crate::error::{DataKind, Error};

/// What an admission policy refused.
///
/// Three distinct cases, deliberately not merged:
///
/// * the caller's own policy said no ([`Self::Refused`]) — the caller knows
///   whether that was a stop, a budget ceiling, or something else, because it
///   decided;
/// * the requested extent is not representable ([`Self::ExtentOverflow`]) —
///   arithmetic, and no allowance would have helped;
/// * the allocator failed *after* admission ([`Self::AllocatorFailed`]) — the
///   charge succeeded and the reservation did not.
///
/// This crate never fabricates a byte figure: `requested` is only ever set from
/// the count actually asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdmissionError {
    /// The caller's policy refuses the charge (stop, budget, or its own reason).
    ///
    /// The canonical payload stays with the caller; this crate only learns "no".
    Refused,
    /// The requested extent cannot be represented.
    ExtentOverflow {
        /// The count that overflowed, as supplied by the parser.
        elements: usize,
    },
    /// Admission succeeded but the allocator could not satisfy the reservation.
    AllocatorFailed {
        /// Bytes the reservation asked for (post-admission).
        requested: usize,
    },
    /// A reservation was accepted for `requested` bytes but the buffer reports a
    /// larger capacity than the admission covered.
    ///
    /// The reported `Vec::capacity()` is part of the observable contract: a buffer
    /// must never hold capacity the caller did not admit. Physical allocator
    /// rounding/RSS is outside what Rust exposes and is not what this checks — it
    /// checks the **reported** figure, which is exactly the value a later
    /// `extend`/`push` would rely on. The deserializer is discarded on this
    /// failure: there is no accepted excess, and no late top-up is performed.
    UnexpectedCapacity {
        /// The extent that was admitted and requested.
        requested: usize,
        /// The capacity the buffer actually reports, which exceeds it.
        actual: usize,
    },
    /// The context's own `RefCell` was already borrowed.
    ///
    /// This is an internal invariant break, not a caller refusal: the seam borrows
    /// for exactly one growth call and drops the borrow before returning, so a
    /// conflict means a bug here, not a policy decision. It is typed rather than a
    /// panic so a bug cannot abort a request, and it is deliberately **not**
    /// `Refused` — inventing a caller refusal for our own invariant failure would
    /// tell the caller to change a policy that was never consulted.
    BorrowConflict,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused => f.write_str("the caller's admission policy refused"),
            Self::ExtentOverflow { elements } => {
                write!(f, "extent {elements} is not representable")
            }
            Self::AllocatorFailed { requested } => {
                write!(f, "the allocator could not provide {requested} bytes")
            }
            Self::UnexpectedCapacity { requested, actual } => write!(
                f,
                "reservation of {requested} bytes reported capacity {actual}"
            ),
            Self::BorrowConflict => f.write_str("internal admission borrow conflict"),
        }
    }
}

/// One decode's admission context, owned on the caller's stack.
///
/// # Why this shape
///
/// The seam needs **one** policy reached by two handles — the reader's string
/// scratch and the deserializer's float scratch — with no allocation at setup.
/// A `Box<dyn>` or `Rc` would allocate per decode, which is itself an unadmitted
/// infallible allocation on every request including a zero-budget one. So the
/// policy is borrowed, not owned: this context holds the caller's `&mut dyn`
/// inside a `RefCell`, and the two handles borrow *this context*.
///
/// # Borrow discipline
///
/// `admit` takes the `RefCell` borrow for the duration of exactly one growth
/// call and drops it before returning. The two handles are therefore never
/// borrowed simultaneously — not by convention but because the borrow is scoped
/// to the call. A `try_borrow_mut` failure would be a logical impossibility in
/// that discipline; it is surfaced as typed `BorrowConflict` rather than a panic so
/// even the impossible case cannot abort.
///
/// # No `unsafe`
///
/// Everything here is safe code: `RefCell` enforces the aliasing rule at run
/// time, and the compiler enforces that the context outlives both handles.
#[cfg(feature = "admission")]
pub struct AdmissionContext<'a> {
    policy: RefCell<&'a mut dyn Admission>,
}

#[cfg(feature = "admission")]
impl<'a> AdmissionContext<'a> {
    /// Borrow a caller-owned policy for one decode. No allocation.
    pub fn new(policy: &'a mut dyn Admission) -> Self {
        Self {
            policy: RefCell::new(policy),
        }
    }

    /// Report progress through the same scoped borrow as growth.
    pub(crate) fn checkpoint(&self, consumed: usize) -> Result<(), AdmissionError> {
        match self.policy.try_borrow_mut() {
            Ok(mut policy) => policy.checkpoint(consumed),
            Err(_) => Err(AdmissionError::BorrowConflict),
        }
    }

    /// Run one growth call, scoped.
    pub(crate) fn grow(&self, additional: usize) -> Result<(), AdmissionError> {
        match self.policy.try_borrow_mut() {
            Ok(mut policy) => policy.grow(additional),
            // Unreachable under the scoped discipline. Typed, not a panic, and NOT
            // `Refused`: our own invariant break must never masquerade as a caller
            // policy decision.
            Err(_) => Err(AdmissionError::BorrowConflict),
        }
    }
}

/// The caller's admission policy for scratch growth.
///
/// One implementor per decode. [`Self::grow`] is called **before** this crate
/// touches the allocator, so a refusal costs no memory. It is never called with
/// `additional == 0`, and never called when the capacity is already sufficient.
///
/// # Ownership
///
/// `grow` takes `&mut self`, so the policy — and any ledger inside it — is
/// borrowed for one growth call and released immediately. That is what lets a
/// caller pass the same ledger to its own visitor work later in the same decode:
/// the borrows are sequential, never overlapping.
pub trait Admission {
    /// Admit `additional` bytes of scratch, or refuse.
    ///
    /// Called before the reservation. Returning `Err` leaves the scratch
    /// unchanged.
    fn grow(&mut self, additional: usize) -> Result<(), AdmissionError>;

    /// Report lexical progress, so a caller can cancel during long work.
    ///
    /// `consumed` is the number of input bytes consumed since the last call. It
    /// is called at a **bounded cadence** — see [`PROGRESS_CADENCE`] — and also at
    /// entry and at completion, so a caller that asked to stop is observed even
    /// when the input is empty.
    ///
    /// The default accepts everything, so an ordinary caller need not implement
    /// it. Returning `Err` aborts the decode through the same typed channel as a
    /// growth refusal; **no allocation is performed** to report it.
    ///
    /// # What this does and does not cover
    ///
    /// It covers the parser's own byte consumption: `next`/`discard`, the bulk
    /// skip inside string scanning, whitespace skipping, number scanning and
    /// ignored values. It does **not** interrupt a single `memchr`/SWAR chunk
    /// search in progress, and it says nothing about CPU instruction counts or
    /// RSS. Whole-input scans performed by `std` (e.g. `str::from_utf8`) are
    /// listed separately in the delivery and are not claimed interruptible.
    fn checkpoint(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        let _ = consumed;
        Ok(())
    }
}

/// Input-byte checkpoint threshold for [`Admission::checkpoint`].
///
/// A power of two so the accounting is cheap. The parser consults the hook when
/// this budget is exhausted, which bounds cancellation latency for any lexical
/// run (long unescaped strings, long numbers, whitespace runs, ignored values) to
/// this threshold. Hexadecimal escapes also advance one byte per accounting call.
/// This bounds lexical consumption, not a whole UTF-8 validator
/// or error-position scan performed by the underlying library.
#[cfg(feature = "admission")]
pub const PROGRESS_CADENCE: usize = 1024;

/// Build an allocation-free error for a seed's own refusal.
///
/// A descriptor-driven seed enforces rules this crate cannot know (unknown
/// field, duplicate field, wrong declared type). Returning a formatted `Error`
/// would allocate on a path that may already be out of memory, so the seed
/// classifies its refusal as a [`DataKind`] and calls this instead.
///
/// The result reports `Category::Data`, carries no rendered message, and
/// allocates nothing. The caller holds the precise classification, so nothing
/// this crate was in a position to describe is lost.
///
/// # Using it where the error type is generic
///
/// A `Visitor` written generically over `E: de::Error` cannot name this type. In
/// that position use [`refuse`], which takes the visitor's error type and routes
/// through this one when it is ours.
#[cfg(feature = "admission")]
#[cold]
#[must_use]
pub fn unwind(kind: DataKind) -> Error {
    Error::data(kind)
}

/// Refuse inside a generic visitor.
///
/// `serde::de::Error` guarantees only `custom<T: Display>`, which is the only
/// construction path a generic visitor has. **Under the `admission` feature this
/// crate's `custom` does not format or box**: it stores the classified
/// [`DataKind`] inline and discards the message, so a generic refusal costs no
/// allocation. That is what makes this helper usable in a seed written against
/// the real serde bounds (no `A::Error` restriction, which E0276 forbids).
///
/// The converse must be stated: the **cost for a foreign `E`** is whatever that
/// type's own `custom` does, which this crate cannot control. For `E =
/// serde_json::Error` the refusal is allocation-free; for any other error type it
/// is only as cheap as that type makes it.
#[cfg(feature = "admission")]
#[cold]
pub fn refuse<E>(kind: DataKind) -> E
where
    E: serde::de::Error,
{
    // No specialization is available on stable Rust, so this is the honest
    // generic form: correctness first, cost documented per call site.
    E::custom(kind)
}

/// A refusal that carries its classification through `Display`.
///
/// `serde::de::Error::custom<T: Display>` is the only construction path a generic
/// visitor has, and it takes a `Display` — not a `DataKind`. This type is the
/// channel: it implements `Display` (so it satisfies the bound) **and** exposes the
/// kind back to [`serde::de::Error::custom`] without formatting a string.
///
/// # How the kind is recovered without allocation
///
/// `carried_kind` cannot downcast a generic `T`. It therefore uses the type's own
/// `Display` output: [`Refusal`]'s `Display` writes the kind's fixed name, and
/// `carried_kind` renders any supplied `Display` into a fixed stack buffer and matches the
/// complete output against the closed vocabulary. An arbitrary message that does
/// not match becomes `Constraint`. This does not identify the message type, and
/// a foreign formatter can perform work of its own; the generated seed passes
/// only the fixed-size `DataKind` value.
#[cfg(feature = "admission")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refusal(pub DataKind);

#[cfg(feature = "admission")]
impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// Recover a [`DataKind`] from a custom message, allocating nothing.
#[cfg(feature = "admission")]
pub(crate) fn carried_kind<T: core::fmt::Display>(msg: &T) -> DataKind {
    // No `to_string`, no `format!`: compare the `Display` output of the *closed*
    // vocabulary by rendering into a fixed stack buffer and matching exactly.
    struct Probe<'a>(&'a mut [u8], usize);
    impl core::fmt::Write for Probe<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.1 + s.len();
            if end > self.0.len() {
                return Err(core::fmt::Error);
            }
            self.0[self.1..end].copy_from_slice(s.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut buf = [0u8; 32];
    let mut probe = Probe(&mut buf, 0);
    if core::fmt::write(&mut probe, format_args!("{msg}")).is_err() {
        return DataKind::Constraint;
    }
    let len = probe.1;
    match &buf[..len] {
        b"unknown field" => DataKind::UnknownField,
        b"duplicate field" => DataKind::DuplicateField,
        b"wrong type for field" => DataKind::WrongType,
        b"missing field" => DataKind::MissingField,
        _ => DataKind::Constraint,
    }
}

/// The bounded progress budget a reader carries.
///
/// Holds the bytes consumed since the last report and the admission context to
/// report **through** — no ledger, no TLS, no allocation. One instance lives on
/// the reader for the decode's lifetime.
#[cfg(feature = "admission")]
pub(crate) struct Progress<'a, 'p> {
    context: &'a AdmissionContext<'p>,
    pending: usize,
    /// A refusal that could not be propagated at the site that produced it.
    ///
    /// `discard` returns `()` and cannot report, so a policy refusal there is
    /// LATCHED here and surfaced at the next fallible progress site. Without this,
    /// the caller's stop would be silently swallowed - a lost refusal, not a
    /// best-effort one.
    latched: Option<AdmissionError>,
    entered: bool,
}

#[cfg(feature = "admission")]
impl<'a, 'p> Progress<'a, 'p> {
    pub(crate) fn new(context: &'a AdmissionContext<'p>) -> Self {
        Self {
            context,
            pending: 0,
            latched: None,
            entered: false,
        }
    }

    /// The first read consults even on empty input. A refusal poisons this decode.
    pub(crate) fn enter(&mut self) -> Result<(), AdmissionError> {
        if let Some(error) = self.latched {
            return Err(error);
        }
        if !self.entered {
            self.entered = true;
            self.report(0)?;
        }
        Ok(())
    }

    fn report(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        if let Some(error) = self.latched {
            return Err(error);
        }
        match self.context.checkpoint(consumed) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.latched = Some(error);
                Err(error)
            }
        }
    }

    /// Account actual newly consumed bytes, never a cumulative scan offset.
    /// Every admitted scan advance is one byte; no bulk scan precedes this.
    #[inline]
    pub(crate) fn advance(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        self.enter()?;
        self.pending = self
            .pending
            .checked_add(consumed)
            .ok_or(AdmissionError::ExtentOverflow { elements: consumed })?;
        if self.pending >= PROGRESS_CADENCE {
            let consumed = self.pending;
            self.pending = 0;
            self.report(consumed)?;
        }
        Ok(())
    }

    /// `discard` has no Result. The first refusal stays latched until drop, and
    /// the next peek/next/scan/end returns it before consuming further input.
    #[inline]
    pub(crate) fn advance_or_latch(&mut self, consumed: usize) {
        if let Err(error) = self.advance(consumed) {
            if self.latched.is_none() {
                self.latched = Some(error);
            }
        }
    }

    /// Flush after trailing whitespace, without clearing a previous refusal.
    #[inline]
    pub(crate) fn finish(&mut self) -> Result<(), AdmissionError> {
        self.enter()?;
        let consumed = self.pending;
        self.pending = 0;
        self.report(consumed)
    }
}
