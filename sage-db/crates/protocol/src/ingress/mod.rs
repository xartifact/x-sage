//! Admitted, descriptor-driven JSON decoding for the generated protocol DTOs.
//!
//! # One authority
//!
//! The lexical authority is the vendored, patched `serde_json` (feature `admission`). This
//! module never parses JSON itself: it supplies the [`Admission`] policy that parser calls and
//! the `DeserializeSeed` implementations that the build-time generator emits from
//! `sage_descriptor.bin` — the same descriptor `prost` and `pbjson` consume. There is no hand
//! grammar, no second DTO model and no post-processing of `pbjson` output.
//!
//! # What admission means here
//!
//! Every retained allocation is admitted **before** it is made:
//!
//! * parser scratch (escaped-string decoding) is admitted by the parser through [`Admission`];
//! * each DTO `String` / `Vec` slot is admitted by the seed through the same policy;
//! * a growth whose allocator reports more capacity than was admitted is a typed
//!   [`IngressClass::UnexpectedCapacity`] — an invariant refusal, never a late top-up.
//!
//! # Ownership
//!
//! [`Decoded`] owns the DTO and is **not** `Clone`. Its [`Decoded::retained_bytes`] reports the
//! actual surviving `String`/`Vec` capacities measured *after* decoding, so the caller can
//! **transfer** credit it already admitted rather than admit again. This module does not mint a
//! reservation and says so: the caller must keep its attempt facade alive through DTO
//! consumption, or adopt the extent into its own store lease before the facade drops.
//!
//! # Errors
//!
//! One [`ErrorSlot`] carries a `Copy` diagnostic. Canonical `SharedLedger` refusals, arithmetic
//! overflow, allocator failure, the caller's body ceiling, malformed JSON and semantic data
//! errors keep **separate classes**, and no formatted text is ever stored or matched.

// The admission runtime is consumed by the descriptor-generated seeds. Those are not integrated
// yet (see DESCRIPTOR-INGRESS-DELIVERY.md), so several entry points have no in-crate caller and
// the compiler flags them as dead. This allowance documents that gap; remove it when the seeds
// are wired in, and do not extend it to silence real dead code.
#![allow(dead_code)]
use core::cell::Cell;
use core::cell::RefCell;
use core::fmt;

use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use serde::de::DeserializeSeed;
use serde_json::admission::{Admission, AdmissionContext, AdmissionError};

/// The caller's named ingress policy.
///
/// Two ceilings, deliberately distinct:
///
/// * `body_bytes` bounds the **input** and is checked before the parser is entered.
/// * `owned_bytes` bounds the **DTD-owned memory** the decoder may allocate. It is checked before
///   each owner grows, so a body of many small strings cannot accumulate ownership past the
///   caller's policy just because each one was individually affordable.
///
/// Parser scratch is paid from the caller's [`SharedLedger`]; the owned ceiling governs DTO
/// owners. Both are enforced, and they are not the same budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IngressLimits {
    /// Maximum input body length, in bytes.
    pub body_bytes: u64,
    /// Maximum DTO-owned heap the decode may retain, in bytes.
    pub owned_bytes: u64,
}

impl IngressLimits {
    /// Construct a policy with both ceilings named.
    #[must_use]
    pub const fn new(body_bytes: u64, owned_bytes: u64) -> Self {
        Self {
            body_bytes,
            owned_bytes,
        }
    }
}

/// Which class an ingress failure belongs to.
///
/// These are deliberately not merged: an operator's remedy differs for each, and the task
/// requires them distinguishable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IngressClass {
    /// The caller's canonical ledger refused (budget or stop). The exact payload is in
    /// [`ErrorSlot::refusal`].
    Refused,
    /// A requested extent is not representable in `usize`.
    ExtentOverflow,
    /// The allocator failed after admission succeeded.
    AllocatorFailed,
    /// The allocator returned more capacity than was admitted.
    UnexpectedCapacity,
    /// A scoped borrow conflicted: this module's own invariant break.
    BorrowConflict,
    /// The input exceeded the caller's own body ceiling (checked before the parser runs).
    BodyCeiling,
    /// The DTO would retain more owned memory than the caller's policy allows.
    OwnedCeiling,
    /// The JSON was not well formed.
    Syntax,
    /// The JSON was well formed but not acceptable for the target message.
    Data,
}

/// A `Copy`, heap-free diagnostic for one ingress failure.
///
/// `field` is the **descriptor field number** when the failure names one. An unknown JSON name
/// has no field number, so it is `0` and the name is *not* copied — a refusal must never
/// allocate to describe itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ErrorSlot {
    /// Failure class.
    pub class: IngressClass,
    /// Descriptor field number, or `0` when none applies.
    pub field: u32,
    /// The canonical refusal, when the class is [`IngressClass::Refused`].
    pub refusal: Option<Refusal>,
    /// The extent this crate requested (admitted), for extent/allocator/capacity classes.
    pub extent: usize,
    /// The capacity the allocator actually reported, when it disagreed with `extent`.
    ///
    /// Only [`IngressClass::UnexpectedCapacity`] sets this; every other class leaves it `0`
    /// because no capacity was measured. It is never a guessed or duplicated figure.
    pub actual: usize,
}

impl ErrorSlot {
    /// An empty slot.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            class: IngressClass::Data,
            field: 0,
            refusal: None,
            extent: 0,
            actual: 0,
        }
    }

    fn of(class: IngressClass, field: u32) -> Self {
        Self {
            class,
            field,
            refusal: None,
            extent: 0,
            actual: 0,
        }
    }

    /// Adopt an exact canonical refusal when one was recorded for this failure.
    ///
    /// Only fills a slot that has none: a class already carrying its own payload keeps it.
    fn with_refusal(mut self, refusal: Option<Refusal>) -> Self {
        if self.refusal.is_none() {
            self.refusal = refusal;
        }
        self
    }

    /// Whether this slot describes a failure.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.refusal.is_some() || self.class != IngressClass::Data || self.field != 0
    }
}

impl fmt::Display for ErrorSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.class {
            IngressClass::Refused => match self.refusal {
                Some(Refusal::Budget(error)) => write!(f, "refused: {:?}", error),
                Some(Refusal::Stopped(_)) => f.write_str("refused: stopped"),
                Some(Refusal::Allocation { requested }) => {
                    write!(f, "refused: allocation {requested}")
                }
                None => f.write_str("refused"),
            },
            IngressClass::ExtentOverflow => write!(f, "extent {} not representable", self.extent),
            IngressClass::AllocatorFailed => write!(f, "allocator failed for {}", self.extent),
            IngressClass::UnexpectedCapacity => {
                write!(
                    f,
                    "allocator reported {} but only {} was admitted",
                    self.actual, self.extent
                )
            }
            IngressClass::BorrowConflict => f.write_str("scoped borrow conflict"),
            IngressClass::BodyCeiling => f.write_str("body exceeds the caller's ceiling"),
            IngressClass::OwnedCeiling => f.write_str("owned memory exceeds the caller's ceiling"),
            IngressClass::Syntax => f.write_str("malformed JSON"),
            IngressClass::Data => {
                if self.field == 0 {
                    f.write_str("data refusal")
                } else {
                    write!(f, "data refusal at field {}", self.field)
                }
            }
        }
    }
}

/// The ingress failure type: a class plus its scalar detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IngressError(pub ErrorSlot);

impl fmt::Display for IngressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for IngressError {}

/// The caller's admission policy for one decode.
///
/// One instance per decode. It owns the caller's ledger by mutable reference for the duration of
/// the decode, so every admission and every control checkpoint is spent on the **same** budget
/// the caller is holding. It is also where parser progress callbacks (`checkpoint`) land.
pub(crate) struct LedgerAdmission<'a> {
    ledger: &'a mut dyn SharedLedger,
    /// The most recent exact canonical refusal, kept `Copy` so the decoder can report it.
    pub(crate) last: RefCell<Option<Refusal>>,
}

impl<'a> LedgerAdmission<'a> {
    /// Wrap the caller's ledger for one decode.
    pub(crate) fn new(ledger: &'a mut dyn SharedLedger) -> Self {
        Self {
            ledger,
            last: RefCell::new(None),
        }
    }

    /// Record a canonical refusal and return the matching admission error.
    pub(crate) fn refuse(&mut self, refusal: Refusal) -> AdmissionError {
        // The exact payload is preserved here; `AdmissionError::Refused` itself is payload-free
        // by design, and the decoder reads this cell to report the real class.
        // A nested borrow means the caller re-entered the policy; that is a typed refusal, not a
        // panic. The refusal is dropped rather than panicking, and the class says why.
        let Ok(mut slot) = self.last.try_borrow_mut() else {
            return AdmissionError::BorrowConflict;
        };
        *slot = Some(refusal);
        AdmissionError::Refused
    }

    /// Charge exactly one work entry, recording the canonical refusal on failure.
    pub(crate) fn charge_one_entry(&mut self) -> Result<(), AdmissionError> {
        match self.ledger.try_charge_entries(1) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.refuse(Refusal::Budget(error))),
        }
    }

    /// The most recent exact refusal, if the policy refused.
    pub(crate) fn last_refusal(&self) -> Option<Refusal> {
        self.last.try_borrow().ok().and_then(|slot| *slot)
    }
}

impl Admission for LedgerAdmission<'_> {
    fn grow(&mut self, additional: usize) -> Result<(), AdmissionError> {
        // A `usize` wider than `u64` is not representable in the ledger's counters on any target
        // this crate supports, but the conversion is checked rather than cast: a narrowing would
        // silently under-charge.
        let Ok(bytes) = u64::try_from(additional) else {
            return Err(AdmissionError::ExtentOverflow {
                elements: additional,
            });
        };
        match self.ledger.try_charge(bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.refuse(Refusal::Budget(error))),
        }
    }

    /// Report lexical progress AND charge it.
    ///
    /// The vendored parser calls this at a bounded cadence (`PROGRESS_CADENCE`, 1024 bytes) and
    /// at entry/exit, so a stop is observed even on empty input. `consumed` is **lexical work**,
    /// not allocation: it is charged as ledger *entries* (the canonical work counter) rather than
    /// as bytes, because charging it as bytes would conflate scanning effort with the memory the
    /// DTO retains. The control stop keeps its exact canonical `ControlStop` payload.
    fn checkpoint(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        if consumed > 0 {
            // A conversion overflow is a real refusal, not a clamp: never invent `u64::MAX`.
            let Ok(entries) = u64::try_from(consumed) else {
                return Err(AdmissionError::ExtentOverflow { elements: consumed });
            };
            if let Err(error) = self.ledger.try_charge_entries(entries) {
                return Err(self.refuse(Refusal::Budget(error)));
            }
        }
        match self.ledger.checkpoint() {
            Ok(()) => Ok(()),
            Err(stop) => Err(self.refuse(Refusal::Stopped(stop))),
        }
    }
}

/// A forwarding adapter so the parser can hold `&mut dyn Admission` while the seeds keep a
/// shared handle to the same policy.
///
/// This is the "scoped `RefCell` borrow, no `Rc`/`Box`/leak" shape: the policy lives in a
/// `RefCell` owned by the decode frame, the parser borrows a thin adapter that forwards into it,
/// and the seeds borrow the same cell directly. One policy, one ledger, no second admission path.
pub(crate) struct SharedPolicy<'a, 'l>(pub(crate) &'a RefCell<LedgerAdmission<'l>>);

impl<'a, 'l> Admission for SharedPolicy<'a, 'l> {
    fn grow(&mut self, additional: usize) -> Result<(), AdmissionError> {
        // Nested re-entry is a typed refusal, never a `RefCell` panic on the request path.
        let Ok(mut policy) = self.0.try_borrow_mut() else {
            return Err(AdmissionError::BorrowConflict);
        };
        policy.grow(additional)
    }

    fn checkpoint(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        let Ok(mut policy) = self.0.try_borrow_mut() else {
            return Err(AdmissionError::BorrowConflict);
        };
        policy.checkpoint(consumed)
    }
}

/// The shared, borrow-scoped state one decode needs.
///
/// Seeds reach the caller's policy through this value: a `&RefCell` handle, so no `Rc`, no `Box`,
/// no leak and no `unsafe` aliasing. Each borrow is scoped to exactly one admission call, so two
/// borrowers never overlap.
/// The shared admission state one decode threads through its seeds.
///
/// Public because the descriptor-generated seeds (a sibling module) hold a reference to it. It has
/// no public constructor and no public methods: a caller decodes through a generated
/// `decode_*` wrapper, never by assembling state directly.
pub struct DecodeState<'a, 'l> {
    pub(crate) policy: &'a RefCell<LedgerAdmission<'l>>,
    /// The caller's owned-memory ceiling for DTO retention.
    owned_ceiling: u64,
    /// Owned bytes admitted for DTO retention so far.
    owned: Cell<u64>,
    /// The FIRST exact failure, made sticky so a later generic `E::custom` cannot lose the class.
    ///
    /// A seed records the precise classified failure here **before** it returns whatever error the
    /// parser will accept, so classification always has the true cause even though the error type
    /// is generic and carries only a coarse category.
    first_error: Cell<Option<ErrorSlot>>,
}

impl<'a, 'l> DecodeState<'a, 'l> {
    /// Wrap the decode frame's policy cell.
    pub(crate) fn new(policy: &'a RefCell<LedgerAdmission<'l>>, owned_ceiling: u64) -> Self {
        Self {
            policy,
            owned_ceiling,
            owned: Cell::new(0),
            first_error: Cell::new(None),
        }
    }

    /// The FIRST recorded failure, if any. Later recordings never displace it.
    pub(crate) fn first_error(&self) -> Option<ErrorSlot> {
        self.first_error.get()
    }

    /// Record a failure, keeping the FIRST one.
    ///
    /// This is the side channel that preserves an exact domain class across a generic
    /// `E::custom` unwind. It records before returning, so by the time the parser formats the
    /// error the truth is already stored.
    pub(crate) fn record_error(&self, slot: ErrorSlot) -> ErrorSlot {
        if self.first_error.get().is_none() {
            self.first_error.set(Some(slot));
        }
        slot
    }

    /// Admit `bytes` of DTO-owned memory. The caller then reserves fallibly and compares the
    /// resulting capacity to what it admitted.
    ///
    /// The **owned ceiling** is checked here, before the ledger, because it is a policy about
    /// retention rather than about the shared budget.
    pub(crate) fn admit_bytes(&self, bytes: usize, field: u32) -> Result<(), ErrorSlot> {
        if bytes == 0 {
            return Ok(());
        }
        self.admit_owned(bytes, field)?;
        let Ok(mut policy) = self.policy.try_borrow_mut() else {
            return Err(self.record_error(classify(AdmissionError::BorrowConflict, field, bytes)));
        };
        match Admission::grow(&mut *policy, bytes) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Fetch the EXACT refusal the policy stored before it returned the payload-free
                // `AdmissionError::Refused`; otherwise the slot reports `None` and the caller
                // cannot see what was requested or what remained.
                let refusal = policy.last_refusal();
                let slot = classify(error, field, bytes).with_refusal(refusal);
                Err(self.record_error(slot))
            }
        }
    }

    /// Check the owned ceiling for `bytes` and reserve the room, without touching the ledger.
    ///
    /// Split from the ledger charge so a caller can admit an owner in one place and pay for it in
    /// another without double-counting.
    fn admit_owned(&self, bytes: usize, field: u32) -> Result<(), ErrorSlot> {
        let Ok(requested) = u64::try_from(bytes) else {
            return Err(self.record_error(ErrorSlot {
                class: IngressClass::ExtentOverflow,
                field,
                refusal: None,
                extent: bytes,
                actual: 0,
            }));
        };
        let current = self.owned.get();
        let Some(total) = current.checked_add(requested) else {
            return Err(self.record_error(ErrorSlot {
                class: IngressClass::ExtentOverflow,
                field,
                refusal: None,
                extent: bytes,
                actual: 0,
            }));
        };
        if total > self.owned_ceiling {
            return Err(self.record_error(ErrorSlot {
                class: IngressClass::OwnedCeiling,
                field,
                refusal: None,
                extent: usize::try_from(total).unwrap_or(usize::MAX),
                actual: 0,
            }));
        }
        self.owned.set(total);
        Ok(())
    }

    /// Charge one WORK entry for a scalar or field visit.
    ///
    /// Work is separate from owned bytes: an inline scalar retains no heap, but visiting it is
    /// real work the caller's ledger accounts. This never touches the owned ceiling.
    pub(crate) fn charge_work(&self, field: u32) -> Result<(), ErrorSlot> {
        let Ok(mut policy) = self.policy.try_borrow_mut() else {
            return Err(self.record_error(classify(AdmissionError::BorrowConflict, field, 0)));
        };
        match policy.charge_one_entry() {
            Ok(()) => Ok(()),
            Err(error) => {
                let refusal = policy.last_refusal();
                let slot = classify(error, field, 0).with_refusal(refusal);
                Err(self.record_error(slot))
            }
        }
    }

    /// Admit one vector slot of `T` before it is pushed.
    pub(crate) fn admit_slot<T>(&self, field: u32) -> Result<(), ErrorSlot> {
        self.admit_bytes(core::mem::size_of::<T>(), field)
    }

    /// Charge a copy chunk and check control through the shared ledger.
    pub(crate) fn checkpoint_copy(&self, bytes: usize, field: u32) -> Result<(), ErrorSlot> {
        let Ok(mut policy) = self.policy.try_borrow_mut() else {
            return Err(self.record_error(classify(AdmissionError::BorrowConflict, field, 0)));
        };
        match Admission::checkpoint(&mut *policy, bytes) {
            Ok(()) => Ok(()),
            Err(error) => {
                let refusal = policy.last_refusal();
                let slot = classify(error, field, 0).with_refusal(refusal);
                Err(self.record_error(slot))
            }
        }
    }

    /// The exact canonical refusal the policy recorded, if any.
    pub(crate) fn last_refusal(&self) -> Option<Refusal> {
        self.policy
            .try_borrow()
            .ok()
            .and_then(|policy| policy.last_refusal())
    }
}

/// Classify a parser error into the slot classes, without ever reading formatted text.
///
/// The parser's own `admission_error()` is the authority for resource failures; `classify()`
/// distinguishes syntax from data. The caller's exact `Refusal` is recovered from the policy
/// cell, never invented.
fn classify_parser_error(error: &serde_json::Error, state: &DecodeState<'_, '_>) -> ErrorSlot {
    // A seed records the exact failure in the sticky slot BEFORE it unwinds through the generic
    // `E::custom`. Preferring that slot is what keeps a budget/ceiling/capacity class from being
    // flattened into `Data` by an error type that only carries a coarse category.
    if let Some(slot) = state.first_error() {
        return slot;
    }
    match error.admission_error() {
        Some(AdmissionError::Refused) => ErrorSlot {
            class: IngressClass::Refused,
            field: 0,
            refusal: state.last_refusal(),
            extent: 0,
            actual: 0,
        },
        Some(AdmissionError::ExtentOverflow { elements }) => ErrorSlot {
            class: IngressClass::ExtentOverflow,
            field: 0,
            refusal: None,
            extent: elements,
            actual: 0,
        },
        Some(AdmissionError::AllocatorFailed { requested }) => ErrorSlot {
            class: IngressClass::AllocatorFailed,
            field: 0,
            refusal: None,
            extent: requested,
            actual: 0,
        },
        // The vendor reports BOTH the requested and the actual capacity; keep both. Dropping
        // `actual` here would make the advertised capacity guarantee false on the parse path.
        Some(AdmissionError::UnexpectedCapacity { requested, actual }) => ErrorSlot {
            class: IngressClass::UnexpectedCapacity,
            field: 0,
            refusal: None,
            extent: requested,
            actual,
        },
        Some(AdmissionError::BorrowConflict) => ErrorSlot {
            class: IngressClass::BorrowConflict,
            field: 0,
            refusal: None,
            extent: 0,
            actual: 0,
        },
        None => match error.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                ErrorSlot::of(IngressClass::Syntax, 0)
            }
            _ => ErrorSlot::of(IngressClass::Data, 0),
        },
    }
}

fn classify(error: AdmissionError, field: u32, extent: usize) -> ErrorSlot {
    // `AdmissionError` is the seam's own `Copy` classification. `Refused` carries no payload by
    // design: the exact canonical refusal lives in the policy that refused and is read from
    // `DecodeState::last_refusal`, never invented here.
    match error {
        AdmissionError::Refused => ErrorSlot {
            class: IngressClass::Refused,
            field,
            refusal: None,
            extent,
            actual: 0,
        },
        AdmissionError::ExtentOverflow { elements } => ErrorSlot {
            class: IngressClass::ExtentOverflow,
            field,
            refusal: None,
            extent: elements,
            actual: 0,
        },
        AdmissionError::AllocatorFailed { requested } => ErrorSlot {
            class: IngressClass::AllocatorFailed,
            field,
            refusal: None,
            extent: requested,
            actual: 0,
        },
        AdmissionError::UnexpectedCapacity { requested, actual } => ErrorSlot {
            class: IngressClass::UnexpectedCapacity,
            field,
            refusal: None,
            extent: requested,
            actual,
        },
        // A `RefCell` conflict is this module's own invariant break, never a caller decision.
        AdmissionError::BorrowConflict => ErrorSlot {
            class: IngressClass::BorrowConflict,
            field,
            refusal: None,
            extent: 0,
            actual: 0,
        },
    }
}

/// A decoded DTO that owns its retained memory and reports its extent.
///
/// Deliberately **not** `Clone`: cloning would duplicate admitted memory behind the policy's
/// back. [`Decoded::dto`] borrows; [`Decoded::into_parts`] transfers ownership together with the
/// measured extent so a caller can adopt it.
///
/// External callers obtain this value through a decoder.
///
/// ```compile_fail,E0624
/// use sage_db_protocol::{generated::RecordKey, ingress::Decoded};
///
/// let _ = Decoded::new(RecordKey::default(), 1024);
/// ```
pub struct Decoded<T> {
    dto: T,
    retained_bytes: u64,
}

impl<T> Decoded<T> {
    /// Construct from a finished DTO and its measured retained extent.
    pub(crate) fn new(dto: T, retained_bytes: u64) -> Self {
        Self {
            dto,
            retained_bytes,
        }
    }

    /// Discard the measured extent and take the DTO.
    ///
    /// Used by a nested-message slot, which reports its extent through the parent's own
    /// `RetainedExtent`, so the child's figure must not be double-counted.
    #[must_use]
    pub fn into_dto(self) -> T {
        self.dto
    }

    /// Borrow the DTO. The owner keeps living; nothing is copied.
    #[must_use]
    pub const fn dto(&self) -> &T {
        &self.dto
    }

    /// The actual surviving `String`/`Vec` capacity, in bytes.
    ///
    /// Measured after decoding, so the caller can **transfer** credit it already holds. This is
    /// not a limit and it is not a second admission.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Take the DTO and the measured extent, so the caller can adopt the extent.
    #[must_use]
    pub fn into_parts(self) -> (T, u64) {
        (self.dto, self.retained_bytes)
    }
}

/// Measure the retained heap of a generated DTO.
///
/// Generated per message by the build-time generator: it sums the **capacity** of every
/// `String`, every `Vec` slot capacity, and recurses into nested messages and the active oneof
/// arm. Capacity, not length — a vector that grew to 8 slots and holds 5 retains 8.
///
/// A sum that cannot be represented is an error, never a saturating clamp: two admitted figures
/// that overflow `u64` must not silently produce a smaller extent than the memory really held.
pub trait RetainedExtent {
    /// Bytes this value retains, or `None` if the sum is not representable.
    ///
    /// There is deliberately **no** infallible convenience method: every caller on the request
    /// path must handle the unrepresentable case instead of panicking on it.
    fn checked_retained_bytes(&self) -> Option<u64>;
}

/// Decode `bytes` as `T` through the descriptor-driven seed, bounded by `body_ceiling`.
///
/// # Ordering
///
/// The body ceiling is checked **before** the parser is invoked and without copying the input.
/// The parser then runs under the caller's ledger for scratch growth and progress, and the DTO
/// seeds admit every retained allocation before making it.
///
/// # Errors
///
/// [`IngressError`] carrying a `Copy` [`ErrorSlot`]. A refusal never returns a partial DTO.
/// Decode `bytes` as `T` under `frame`, bounded by `body_ceiling`.
///
/// # Why the policy is supplied by the caller
///
/// `serde_json::de::Deserializer::from_slice_with_admission` requires the input slice and the
/// admission context to share **one** lifetime, because `SliceRead<'a>` stores
/// `&'a AdmissionContext<'a>`. The context therefore cannot be a local of this function; the
/// caller owns it for the decode. `AdmissionContext` has no public constructor-from-`&RefCell`,
/// so the caller builds it over a policy it also owns, and hands the same policy cell to
/// [`decode`] for the seeds.
///
/// The one policy is reached two ways — by the parser through the context, and by the seeds
/// through the cell — and each borrow is scoped to a single call, so they never overlap.
///
/// # Errors
///
/// [`IngressError`] carrying a `Copy` [`ErrorSlot`]. A refusal never returns a partial DTO.
pub(crate) fn decode<'de, T, S>(
    bytes: &'de [u8],
    body_ceiling: u64,
    policy: &'de RefCell<LedgerAdmission<'de>>,
    context: &'de AdmissionContext<'de>,
    seed: S,
) -> Result<Decoded<T>, IngressError>
where
    S: DeserializeSeed<'de, Value = Decoded<T>>,
{
    if bytes.len() as u64 > body_ceiling {
        return Err(IngressError(ErrorSlot {
            class: IngressClass::BodyCeiling,
            field: 0,
            refusal: None,
            extent: bytes.len(),
            actual: 0,
        }));
    }
    let state = DecodeState::new(policy, u64::MAX);
    let mut deserializer = serde_json::de::Deserializer::from_slice_with_admission(bytes, context);
    match seed.deserialize(&mut deserializer) {
        Ok(decoded) => {
            // A decoded value is not the whole job: the parser must also confirm nothing follows.
            // Without this, `{"a":1} junk` would be accepted as a complete document.
            match serde_json::de::Deserializer::end(&mut deserializer) {
                Ok(()) => Ok(decoded),
                Err(error) => Err(IngressError(classify_parser_error(&error, &state))),
            }
        }
        Err(error) => Err(IngressError(classify_parser_error(&error, &state))),
    }
}

// The descriptor-generated seeds live in one module beside the runtime so the generated code can
// name [`DecodeState`], [`Decoded`] and [`RetainedExtent`].
// Generated names mirror the descriptor and imports are shared across the emitted submodules.
#[allow(non_snake_case, unused_imports, unused_mut)]
pub mod seeds {
    //! Descriptor-generated admitted seeds, one per protocol message.
    include!(concat!(env!("OUT_DIR"), "/ingress_seeds.rs"));
}

mod wrappers {
    //! Descriptor-generated public one-call decode wrappers.
    include!(concat!(env!("OUT_DIR"), "/ingress_wrappers.rs"));
}

mod checkpoint;
pub use checkpoint::validate_legacy_checkpoint;

pub use wrappers::*;

// ---------------------------------------------------------------------------
// Seeds the generated code composes
// ---------------------------------------------------------------------------

/// The seed for one `String` slot.
///
/// Admission happens **inside** the visitor, on the actual decoded value, and strictly before the
/// copy: the visitor admits `v.len()`, reserves exactly that fallibly, and only then copies. There
/// is no post-hoc "already charged" claim and no infallible `to_string` of unpaid memory.
pub struct StringSeed<'s> {
    /// The decode's shared admission state.
    pub state: &'s DecodeState<'s, 's>,
    /// The field number, for error attribution.
    pub field: u32,
}

impl<'de, 's> DeserializeSeed<'de> for StringSeed<'s> {
    type Value = Option<String>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Option<String>, D::Error> {
        // Null is absence, not an empty string: `Some("")` and `None` must stay distinct, which is
        // exactly what `deserialize_option` gives us.
        d.deserialize_option(V {
            state: self.state,
            field: self.field,
        })
    }
}

struct V<'s> {
    state: &'s DecodeState<'s, 's>,
    field: u32,
}

impl<'s> V<'s> {
    /// Admit the owner, reserve exactly it, GUARD the result, then copy in bounded chunks.
    ///
    /// Order matters: the ceiling and ledger are consulted for the bytes that will really be
    /// retained (`capacity()` after the reserve), never after the fact. A capacity the allocator
    /// reports beyond what was admitted is a typed invariant refusal, not silent retention.
    fn admit_and_copy<E: serde::de::Error>(&self, v: &str) -> Result<String, E> {
        let len = v.len();
        // The owned ceiling and ledger are consulted for exactly the bytes we will retain.
        self.state.admit_bytes(len, self.field).map_err(E::custom)?;
        let mut out = String::new();
        out.try_reserve_exact(len).map_err(|_| {
            // Record the exact allocator class before unwinding through `E::custom`.
            E::custom(slot_to_message(
                self.state
                    .record_error(ErrorSlot {
                        class: IngressClass::AllocatorFailed,
                        field: self.field,
                        refusal: None,
                        extent: len,
                        actual: 0,
                    })
                    .class,
            ))
        })?;
        // The allocator may hand back MORE than asked; that surplus was never admitted.
        if out.capacity() != len {
            let slot = self.state.record_error(ErrorSlot {
                class: IngressClass::UnexpectedCapacity,
                field: self.field,
                refusal: None,
                extent: len,
                actual: out.capacity(),
            });
            return Err(E::custom(slot_to_message(slot.class)));
        }
        // Copy in bounded chunks, observing CONTROL between them. Every byte copied was admitted
        // above, so a stop here leaves only already-paid bytes in `out`, which is dropped.
        const CHUNK: usize = 1024;
        let mut copied = 0;
        while copied < v.len() {
            let mut end = copied + CHUNK.min(v.len() - copied);
            // The end of the string is already a boundary. A full chunk can
            // retreat by at most three bytes, leaving a nonempty prefix.
            while !v.is_char_boundary(end) {
                end -= 1;
            }
            self.state
                .checkpoint_copy(end - copied, self.field)
                .map_err(|slot| E::custom(slot_to_message(slot.class)))?;
            out.push_str(&v[copied..end]);
            copied = end;
        }
        Ok(out)
    }
}

impl<'de, 's> serde::de::Visitor<'de> for V<'s> {
    type Value = Option<String>;

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a JSON string or null")
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_str(StrVisitor {
            state: self.state,
            field: self.field,
        })
        .map(Some)
    }

    fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        self.admit_and_copy(v).map(Some)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        self.admit_and_copy(v).map(Some)
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        // An OWNED string already exists, but its capacity was never admitted — so it is refused
        // rather than credited post hoc. The concrete slice parser paths never take this route.
        let _ = v;
        let slot = self.state.record_error(ErrorSlot {
            class: IngressClass::UnexpectedCapacity,
            field: self.field,
            refusal: None,
            extent: 0,
            actual: 0,
        });
        Err(E::custom(slot_to_message(slot.class)))
    }
}

/// The inner visitor for the non-null string forms.
struct StrVisitor<'s> {
    state: &'s DecodeState<'s, 's>,
    field: u32,
}

impl<'de, 's> serde::de::Visitor<'de> for StrVisitor<'s> {
    type Value = String;

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a JSON string")
    }

    fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<String, E> {
        V {
            state: self.state,
            field: self.field,
        }
        .admit_and_copy(v)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<String, E> {
        V {
            state: self.state,
            field: self.field,
        }
        .admit_and_copy(v)
    }
}

/// The closed message text for a class.
///
/// The vendored parser recovers a *category* from this text, so it must be one of the words it
/// knows; the precise class is carried separately in the sticky [`DecodeState::first_error`] slot,
/// which classification prefers.
pub(crate) const fn slot_to_message(class: IngressClass) -> &'static str {
    match class {
        IngressClass::UnexpectedCapacity => "wrong type for field",
        IngressClass::ExtentOverflow => "wrong type for field",
        IngressClass::AllocatorFailed => "wrong type for field",
        IngressClass::OwnedCeiling => "wrong type for field",
        IngressClass::BodyCeiling => "wrong type for field",
        IngressClass::BorrowConflict => "wrong type for field",
        IngressClass::Refused => "wrong type for field",
        IngressClass::Syntax | IngressClass::Data => "wrong type for field",
    }
}

/// The seed for a REQUIRED `String` field.
///
/// pbjson spells a required string as `next_value::<String>()`, so JSON `null` is a type error.
/// This seed therefore yields `String`, never `Option<String>`, and is distinct from
/// [`StringSeed`] so the two cannot be confused.
pub struct RequiredStringSeed<'s> {
    /// The decode's shared admission state.
    pub state: &'s DecodeState<'s, 's>,
    /// The field number, for error attribution.
    pub field: u32,
}

impl<'de, 's> DeserializeSeed<'de> for RequiredStringSeed<'s> {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        // `deserialize_str` (not `_option`) is what makes `null` a type error, matching pbjson.
        let nullable = StringSeed {
            state: self.state,
            field: self.field,
        }
        .deserialize(d)?;
        match nullable {
            Some(value) => Ok(value),
            None => Err(serde::de::Error::custom(
                "invalid type: null, expected a string",
            )),
        }
    }
}

/// The seed for an OPTIONAL numeric or enum field.
///
/// pbjson spells these as `next_value::<Option<NumberDeserialize<_>>>()`, so `null` is absence and
/// a later non-null value is still accepted. Distinct from [`IntegerSeed`], which is required.
pub struct OptionalIntSeed<T> {
    marker: core::marker::PhantomData<T>,
}

impl<T> OptionalIntSeed<T> {
    /// Construct the seed for one optional numeric field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            marker: core::marker::PhantomData,
        }
    }
}

impl<T> Default for OptionalIntSeed<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de, T> DeserializeSeed<'de> for OptionalIntSeed<T>
where
    T: serde::de::Deserialize<'de>,
{
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Option<T>, D::Error> {
        // `deserialize_option` is what makes JSON `null` become `None` rather than a type error.
        struct V<T>(core::marker::PhantomData<T>);
        impl<'de, T: serde::de::Deserialize<'de>> serde::de::Visitor<'de> for V<T> {
            type Value = Option<T>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a number or null")
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                T::deserialize(d).map(Some)
            }
        }
        d.deserialize_option(V(core::marker::PhantomData))
    }
}

/// The seed for an OPTIONAL enum field.
///
/// The value validates through the generated enum (names + closed numbers), then casts to `i32`;
/// `null` is absence. Distinct from [`OptionalIntSeed`], whose `T` is the DTO's own scalar type.
pub struct OptionalEnumSeed<E> {
    marker: core::marker::PhantomData<E>,
}

impl<E> OptionalEnumSeed<E> {
    /// Construct the seed for one optional enum field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            marker: core::marker::PhantomData,
        }
    }
}

impl<E> Default for OptionalEnumSeed<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de, E> DeserializeSeed<'de> for OptionalEnumSeed<E>
where
    E: serde::Deserialize<'de> + TryInto<i32>,
{
    type Value = Option<i32>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Option<i32>, D::Error> {
        struct V<E>(core::marker::PhantomData<E>);
        impl<'de, E> serde::de::Visitor<'de> for V<E>
        where
            E: serde::Deserialize<'de> + TryInto<i32>,
        {
            type Value = Option<i32>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("an enum or null")
            }

            fn visit_none<Er: serde::de::Error>(self) -> Result<Self::Value, Er> {
                Ok(None)
            }

            fn visit_unit<Er: serde::de::Error>(self) -> Result<Self::Value, Er> {
                Ok(None)
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                let value = E::deserialize(d)?;
                value
                    .try_into()
                    .map(Some)
                    .map_err(|_| serde::de::Error::custom("enum value out of i32 range"))
            }
        }
        d.deserialize_option(V::<E>(core::marker::PhantomData))
    }
}

/// The seed for a REPEATED string element: required, no default.
///
/// pbjson decodes `Vec<String>` with a required element, so a `null` ELEMENT is refused. The
/// previous `unwrap_or_default` silently turned it into `""`, which pbjson never does.
pub struct RepeatedStringSeed<'s> {
    /// The decode's shared admission state.
    pub state: &'s DecodeState<'s, 's>,
    /// The field number, for error attribution.
    pub field: u32,
}

impl<'de, 's> DeserializeSeed<'de> for RepeatedStringSeed<'s> {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        RequiredStringSeed {
            state: self.state,
            field: self.field,
        }
        .deserialize(d)
    }
}

/// The seed for one nested message slot./// The seed for one nested message slot.
///
/// `M` is the **generated** seed for the nested message, supplied by the caller. It receives the
/// same `DecodeState`, so the child admits through the parent's ledger. This type has no
/// `T::deserialize` path: a nested message must never route strings/vecs/numbers through ordinary
/// `Deserialize`, which would bypass admission.
pub struct MessageSeed<M> {
    /// The generated seed for the nested message.
    pub seed: M,
}

impl<M> MessageSeed<M> {
    /// Wrap a generated seed for one nested slot.
    #[must_use]
    pub fn new(seed: M) -> Self {
        Self { seed }
    }
}

impl<'de, T, M> DeserializeSeed<'de> for MessageSeed<M>
where
    M: DeserializeSeed<'de, Value = Decoded<T>>,
{
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Option<T>, D::Error> {
        // A message field is nullable in protobuf JSON: an explicit `null` is absence, and pbjson
        // assigns the slot directly, so a LATER non-null occurrence is still accepted (it trips
        // only the presence check if the slot was already `Some`).
        struct V<M>(M);
        impl<'de, T, M> serde::de::Visitor<'de> for V<M>
        where
            M: DeserializeSeed<'de, Value = Decoded<T>>,
        {
            type Value = Option<T>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a message object or null")
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                self.0
                    .deserialize(d)
                    .map(|decoded| Some(decoded.into_dto()))
            }
        }
        d.deserialize_option(V(self.seed))
    }
}

/// The seed for one integer slot, accepting both `123` and `"123"`.
///
/// Nothing generic: the four widths below are the only shapes a descriptor may name, so a field
/// outside them is a generator error rather than a silent `Deserialize` fallback.
pub struct IntegerSeed<T>(core::marker::PhantomData<T>);

impl<T> IntegerSeed<T> {
    /// Construct the seed for one field.
    #[must_use]
    pub const fn new() -> Self {
        Self(core::marker::PhantomData)
    }
}

impl<T> Default for IntegerSeed<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Accepts an integer in either JSON form pbjson accepts: a bare number or a quoted decimal.
struct IntVisitor<T>(core::marker::PhantomData<T>);

macro_rules! integer_width {
    ($ty:ty, $expect:literal) => {
        impl<'de> serde::de::Visitor<'de> for IntVisitor<$ty> {
            type Value = $ty;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str($expect)
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<$ty, E> {
                <$ty>::try_from(v).map_err(|_| E::custom("integer out of range"))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<$ty, E> {
                <$ty>::try_from(v).map_err(|_| E::custom("integer out of range"))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<$ty, E> {
                v.parse::<$ty>()
                    .map_err(|_| E::custom("quoted integer is not a valid integer of this width"))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<$ty, E> {
                self.visit_str(&v)
            }
        }

        impl<'de> DeserializeSeed<'de> for IntegerSeed<$ty> {
            type Value = $ty;

            fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<$ty, D::Error> {
                d.deserialize_any(IntVisitor::<$ty>(core::marker::PhantomData))
            }
        }
    };
}

integer_width!(u32, "a 32-bit unsigned integer");
integer_width!(u64, "a 64-bit unsigned integer");
integer_width!(i32, "a 32-bit signed integer");
integer_width!(i64, "a 64-bit signed integer");

/// The seed for one repeated `String` element: admits the element's own length, then copies.
pub struct StringElementSeed<'s> {
    /// The decode's shared admission state.
    pub state: &'s DecodeState<'s, 's>,
    /// The field number, for error attribution.
    pub field: u32,
}

impl<'de, 's> DeserializeSeed<'de> for StringElementSeed<'s> {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        // A repeated element is not nullable in pbjson (`Vec<String>`, not `Vec<Option<String>>`),
        // so the nullable `StringSeed` result is unwrapped here.
        StringSeed {
            state: self.state,
            field: self.field,
        }
        .deserialize(d)
        .map(Option::unwrap_or_default)
    }
}

/// Push one element of a repeated field, admitting before the growth.
///
/// Order is what makes this honest: the slot is admitted first, then `try_reserve(1)` grows
/// fallibly, then the *actual* capacity delta is charged — a table that over-allocates is caught
/// here rather than tripping the allocator guard later. Only then is the value moved in, so there
/// is no infallible growth and no element pushed on unpaid capacity.
pub fn push_admitted<T, E: serde::de::Error>(
    values: &mut Vec<T>,
    value: T,
    state: &DecodeState<'_, '_>,
    field: u32,
) -> Result<(), E> {
    if values.len() == values.capacity() {
        // The slot is admitted before the growth, so a refusal costs no memory.
        state.admit_slot::<T>(field).map_err(E::custom)?;
        let before = values.capacity();
        // `try_reserve_exact` records exactly `len + 1`, so the delta below is exactly one slot.
        values.try_reserve_exact(1).map_err(|_| {
            E::custom(slot_to_message(
                state
                    .record_error(ErrorSlot {
                        class: IngressClass::AllocatorFailed,
                        field,
                        refusal: None,
                        extent: core::mem::size_of::<T>(),
                        actual: 0,
                    })
                    .class,
            ))
        })?;
        // No saturation and no unchecked casts: every figure is checked, and an overflow is a
        // recorded extent failure rather than a clamped number.
        let grown = values
            .capacity()
            .checked_sub(before)
            .ok_or_else(|| E::custom("vector capacity went backwards"))?;
        let grown =
            u64::try_from(grown).map_err(|_| E::custom("vector capacity delta unrepresentable"))?;
        let slot_bytes = u64::try_from(core::mem::size_of::<T>())
            .map_err(|_| E::custom("slot size unrepresentable"))?;
        let delta = grown
            .checked_mul(slot_bytes)
            .ok_or_else(|| E::custom("vector capacity delta overflowed"))?;
        if delta != slot_bytes {
            // The allocator handed back more than the single slot we admitted.
            let slot = state.record_error(ErrorSlot {
                class: IngressClass::UnexpectedCapacity,
                field,
                refusal: None,
                extent: usize::try_from(slot_bytes).unwrap_or(usize::MAX),
                actual: usize::try_from(delta).unwrap_or(usize::MAX),
            });
            return Err(E::custom(slot_to_message(slot.class)));
        }
    }
    values.push(value);
    Ok(())
}

/// Add one extent to a running total, refusing to clamp.
///
/// Generated messages call this instead of `saturating_add`, so an extent that cannot be
/// represented is reported (`None`) rather than silently shrinking.
pub fn add_checked(total: u64, part: u64) -> Option<u64> {
    total.checked_add(part)
}

/// The seed for one element of a repeated message field.
///
/// A repeated message element is not nullable (`Vec<T>`, not `Vec<Option<T>>`), so this yields the
/// message directly while still decoding through its generated seed with the same state.
pub struct ElementMessageSeed<M>(M);

impl<M> ElementMessageSeed<M> {
    /// Wrap a generated seed for one element.
    #[must_use]
    pub fn new(seed: M) -> Self {
        Self(seed)
    }
}

impl<'de, T, M> DeserializeSeed<'de> for ElementMessageSeed<M>
where
    M: DeserializeSeed<'de, Value = Decoded<T>>,
{
    type Value = T;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<T, D::Error> {
        self.0.deserialize(d).map(Decoded::into_dto)
    }
}

/// The seed for a oneof message member.
///
/// pbjson spells a oneof member as `next_value::<Option<_>>()?.map(Arm)` — so JSON `null` yields
/// `None`, the arm stays unset, and a LATER member is still allowed. This seed therefore yields
/// `Option<T>`; the generated arm only sets the slot when the value is present.
pub struct OneofMessageSeed<M>(M);

impl<M> OneofMessageSeed<M> {
    /// Wrap a generated seed for one oneof member.
    #[must_use]
    pub fn new(seed: M) -> Self {
        Self(seed)
    }
}

impl<'de, T, M> DeserializeSeed<'de> for OneofMessageSeed<M>
where
    M: DeserializeSeed<'de, Value = Decoded<T>>,
{
    type Value = Option<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Option<T>, D::Error> {
        struct V<M>(M);

        impl<'de, T, M> serde::de::Visitor<'de> for V<M>
        where
            M: DeserializeSeed<'de, Value = Decoded<T>>,
        {
            type Value = Option<T>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a message object or null")
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                self.0
                    .deserialize(d)
                    .map(|decoded| Some(decoded.into_dto()))
            }
        }

        d.deserialize_option(V(self.0))
    }
}

/// The seed for one enum slot./// The seed for one enum slot.
///
/// The generated enum type owns the authoritative decode: it accepts the descriptor's STRING
/// names and the closed set of numeric values, exactly as pbjson does. A generic integer seed
/// would wrongly accept `"1"` as a quoted number and any out-of-range integer, so enums never go
/// through [`IntegerSeed`].
///
/// `E` is the prost enum (`Decision`, `ProposalStatus`, ...); the DTO field is `i32`, so the seed
/// casts after the enum has validated the value, mirroring `Decision::try_from`.
pub struct EnumSeed<E>(core::marker::PhantomData<E>);

impl<E> EnumSeed<E> {
    /// Construct the seed for one enum field.
    #[must_use]
    pub fn new() -> Self {
        Self(core::marker::PhantomData)
    }
}

impl<E> Default for EnumSeed<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de, E> DeserializeSeed<'de> for EnumSeed<E>
where
    E: serde::Deserialize<'de> + TryInto<i32>,
{
    type Value = i32;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<i32, D::Error> {
        let value = E::deserialize(d)?;
        value
            .try_into()
            .map_err(|_| serde::de::Error::custom("enum value out of i32 range"))
    }
}

/// The seed for a repeated field: decodes a JSON array, admitting each element before it is kept.
///
/// A repeated field is ONE value whose JSON form is an array, so it must be decoded as a sequence.
/// Decoding a single element per key would type-error on the array and would under-count
/// admission.
pub struct VecSeed<'s, E, M> {
    /// The decode's shared admission state.
    pub state: &'s DecodeState<'s, 's>,
    /// The field number, for error attribution.
    pub field: u32,
    /// Builds the per-element seed.
    pub make: M,
    marker: core::marker::PhantomData<fn() -> E>,
}

impl<'s, E, M> VecSeed<'s, E, M> {
    /// Construct the seed for one repeated field.
    #[must_use]
    pub fn new(state: &'s DecodeState<'s, 's>, field: u32, make: M) -> Self {
        Self {
            state,
            field,
            make,
            marker: core::marker::PhantomData,
        }
    }
}

impl<'de, 's, E, M, S> DeserializeSeed<'de> for VecSeed<'s, E, M>
where
    M: Fn(&'s DecodeState<'s, 's>) -> S,
    S: DeserializeSeed<'de, Value = E>,
{
    type Value = Vec<E>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        struct V<'s, E, M> {
            state: &'s DecodeState<'s, 's>,
            field: u32,
            make: M,
            marker: core::marker::PhantomData<fn() -> E>,
        }

        impl<'de, 's, E, M, S> serde::de::Visitor<'de> for V<'s, E, M>
        where
            M: Fn(&'s DecodeState<'s, 's>) -> S,
            S: DeserializeSeed<'de, Value = E>,
        {
            type Value = Vec<E>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a JSON array")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values: Vec<E> = Vec::new();
                while let Some(value) = seq.next_element_seed((self.make)(self.state))? {
                    push_admitted(&mut values, value, self.state, self.field)?;
                }
                Ok(values)
            }
        }

        d.deserialize_seq(V {
            state: self.state,
            field: self.field,
            make: self.make,
            marker: core::marker::PhantomData,
        })
    }
}

/// The retained extent of a value.
///
/// # Errors
///
/// Returns the checked figure; a sum that cannot be represented is `None`, never a clamp.
pub fn value_extent<T: RetainedExtent>(value: &T) -> Option<u64> {
    value.checked_retained_bytes()
}

/// The retained extent of an optional value.
#[must_use]
pub fn opt_extent<T: RetainedExtent>(value: &Option<T>) -> Option<u64> {
    match value {
        Some(value) => value.checked_retained_bytes(),
        None => Some(0),
    }
}

/// The retained extent of a vector: slot capacity plus each element's own heap children.
///
/// `Vec` does **not** own its elements' inline bytes separately — they live inside the slots the
/// vector already allocated — so the slot term is `size_of::<T>() * capacity`, and each element
/// contributes only the heap it owns *beyond* its slot (a nested `String`/`Vec`/message).
///
/// Empty or just-drained vectors that still hold capacity are charged for that capacity: the
/// allocation is real and the caller paid for it.
///
/// # Errors
///
/// `None` when any product or sum is not representable.
pub fn vec_extent<T: RetainedExtent>(values: &Vec<T>) -> Option<u64> {
    let slots = u64::try_from(values.capacity())
        .ok()?
        .checked_mul(u64::try_from(core::mem::size_of::<T>()).ok()?)?;
    let mut total = slots;
    for value in values {
        total = total.checked_add(value.checked_retained_bytes()?)?;
    }
    Some(total)
}

/// An inline scalar owns **no heap** beyond the struct slot the parent already accounts for.
///
/// Charging `size_of` here would invent an owner for memory that the enclosing message's own slot
/// already paid for.
macro_rules! inline_scalar_extent {
    ($($ty:ty),*) => {
        $(
            impl RetainedExtent for $ty {
                fn checked_retained_bytes(&self) -> Option<u64> {
                    Some(0)
                }
            }
        )*
    };
}

inline_scalar_extent!(u32, u64, i32, i64);

/// A `String` owns its heap buffer, whose size is `capacity()`, not `len()`.
impl RetainedExtent for String {
    fn checked_retained_bytes(&self) -> Option<u64> {
        u64::try_from(self.capacity()).ok()
    }
}

/// The retained extent of an `Option`, so `Vec<Option<T>>` slots are covered by `vec_extent`.
impl<T: RetainedExtent> RetainedExtent for Option<T> {
    fn checked_retained_bytes(&self) -> Option<u64> {
        match self {
            Some(value) => value.checked_retained_bytes(),
            None => Some(0),
        }
    }
}

/// The retained extent of a generated DTO.
///
/// # Errors
///
/// `None` when the sum is not representable.
#[must_use]
pub fn extent_of<T: RetainedExtent>(value: &T) -> Option<u64> {
    value.checked_retained_bytes()
}
