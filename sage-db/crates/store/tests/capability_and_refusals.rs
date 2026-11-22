//! Implementation tests for the seam's typed refusals and capability gate (P1-3, store owner).
//!
//! These exercise the **actual** public surface, not stand-ins: `Capabilities::product_eligible`
//! and `missing_required` are the single product gate, and `FsError` carries the ledger's own
//! budget payload and the caller's `ControlStop` distinctly.
//!
//! Scope note: a `Capabilities` value with a qualifying set is a **predicate fixture**. It proves
//! the gate's logic, never that any real platform or backend provides those capabilities, and
//! says nothing about durability.

use sage_db_store::fs::{
    Capabilities, Effect, FsError, FsErrorKind, PlatformCapability, PublishMode, Published,
};
use sage_db_types::ledger::{BudgetError, BudgetUnit};
use sage_db_types::query::ControlStop;

fn all_supported() -> Vec<PlatformCapability> {
    PlatformCapability::REQUIRED_FOR_PRODUCT.to_vec()
}

#[test]
fn required_set_has_six_entries_including_same_device() {
    // The accepted S2 set is six; SameDevice was missing from the scaffold and is required for
    // the same-filesystem rule.
    assert_eq!(PlatformCapability::REQUIRED_FOR_PRODUCT.len(), 6);
    assert!(PlatformCapability::REQUIRED_FOR_PRODUCT.contains(&PlatformCapability::SameDevice));
    assert!(PlatformCapability::REQUIRED_FOR_PRODUCT.contains(&PlatformCapability::DirSync));
    assert!(PlatformCapability::REQUIRED_FOR_PRODUCT.contains(&PlatformCapability::ExclusiveLock));
}

#[test]
fn fully_qualifying_capabilities_pass_the_gate() {
    let caps = Capabilities {
        supported: all_supported(),
        durable_sync: true,
    };
    assert_eq!(caps.missing_required(), None);
    assert!(caps.product_eligible());
}

#[test]
fn a_missing_required_capability_refuses_product_eligibility() {
    // Each required capability, removed in turn, must refuse — and name itself.
    for missing in PlatformCapability::REQUIRED_FOR_PRODUCT {
        let mut supported = all_supported();
        supported.retain(|c| *c != missing);
        let caps = Capabilities {
            supported,
            durable_sync: true,
        };
        assert_eq!(
            caps.missing_required(),
            Some(missing),
            "{missing:?} must be reported as the missing capability"
        );
        assert!(
            !caps.product_eligible(),
            "{missing:?} missing must refuse a product handle even with durable_sync"
        );
    }
}

#[test]
fn durable_sync_false_refuses_even_with_every_primitive() {
    // Every syscall present, but the platform does not document the durability semantics.
    let caps = Capabilities {
        supported: all_supported(),
        durable_sync: false,
    };
    assert_eq!(caps.missing_required(), None, "no primitive is missing");
    assert!(
        !caps.product_eligible(),
        "durable_sync = false must refuse a product handle"
    );
}

#[test]
fn empty_capabilities_refuse() {
    let caps = Capabilities {
        supported: Vec::new(),
        durable_sync: false,
    };
    assert!(caps.missing_required().is_some());
    assert!(!caps.product_eligible());
}

#[test]
fn budget_refusal_preserves_unit_requested_and_remaining() {
    let ledger_refusal = BudgetError::new(BudgetUnit::Entries, 5, 2);
    let fs: FsError = ledger_refusal.into();

    // Classification: a budget refusal is not a data verdict and certainly did not take effect.
    assert_eq!(fs.kind(), FsErrorKind::BudgetExceeded);
    assert_eq!(fs.effect(), Effect::NoEffect);
    assert!(!fs.is_ambiguous());

    // The payload survives: dropping it would make Entries and Bytes indistinguishable.
    let preserved = fs.budget().expect("budget payload must be preserved");
    assert_eq!(preserved.unit(), BudgetUnit::Entries);
    assert_eq!(preserved.requested(), 5);
    assert_eq!(preserved.remaining(), 2);
}

#[test]
fn bytes_and_entries_refusals_remain_distinguishable() {
    let bytes: FsError = BudgetError::new(BudgetUnit::Bytes, 10, 0).into();
    let entries: FsError = BudgetError::new(BudgetUnit::Entries, 10, 0).into();
    assert_eq!(
        bytes.budget().map(BudgetError::unit),
        Some(BudgetUnit::Bytes)
    );
    assert_eq!(
        entries.budget().map(BudgetError::unit),
        Some(BudgetUnit::Entries)
    );
    // Same kind, same amounts — still distinguishable through the preserved unit.
    assert_ne!(bytes.budget(), entries.budget());
}

#[test]
fn cancellation_is_distinct_from_a_budget_refusal() {
    let stopped: FsError = ControlStop::DeadlineExceeded.into();
    assert_eq!(stopped.kind(), FsErrorKind::Interrupted);
    assert_eq!(stopped.effect(), Effect::NoEffect);
    assert!(stopped.budget().is_none(), "a stop is not a budget refusal");
    assert_eq!(stopped.control(), Some(ControlStop::DeadlineExceeded));

    let budget: FsError = BudgetError::new(BudgetUnit::Bytes, 1, 0).into();
    assert!(budget.control().is_none(), "a budget refusal is not a stop");
    assert!(budget.budget().is_some());

    // Cancelled vs deadline is itself distinguishable, because the remedy differs.
    let cancelled: FsError = ControlStop::Cancelled.into();
    assert_eq!(cancelled.control(), Some(ControlStop::Cancelled));
    assert_ne!(cancelled.control(), stopped.control());
}

#[test]
fn plain_io_error_has_neither_payload() {
    let io = FsError::new(FsErrorKind::NotFound, Effect::NoEffect);
    assert!(io.budget().is_none());
    assert!(io.control().is_none());
    assert!(!io.is_ambiguous());
}

#[test]
fn unknown_effect_is_ambiguous_and_not_durable_success() {
    // `Unknown` means "resolve", never "assume it worked". The seam exposes it as ambiguity, and
    // nothing in this crate converts it into a durable claim.
    let ambiguous = FsError::new(FsErrorKind::Io(5), Effect::Unknown);
    assert!(ambiguous.is_ambiguous());
    assert_eq!(ambiguous.effect(), Effect::Unknown);
    assert!(!matches!(ambiguous.effect(), Effect::NoEffect));
}

#[test]
fn publish_mode_and_flags_express_identity_without_durability() {
    // A reused name proves byte identity only; durability is still owed by the caller.
    let reused = Published {
        how: PublishMode::AlreadyPresent,
        needs_sync: true,
    };
    assert!(reused.needs_sync, "identity never implies durability");
    assert_eq!(reused.how, PublishMode::AlreadyPresent);
}
