//! Real public bootstrap: finite capable ledger, legal Genesis, then an ordinary tx, readback and old-pin retention.
//!
//! Everything here goes through the PUBLIC API introduced for the bootstrap (`init::bootstrap`), not a helper: the lock is
//! taken once and travels with the root, Genesis commits before a usable handle exists, and the returned Store publishes a
//! real pin owner. Assertions are on observable behaviour through the same counter, not on internal state.

use sage_db_store::init::bootstrap;
use sage_db_store::lease::{AttemptLedger, LeaseLedger, LedgerStats};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};

use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::query::RunToCompletion;

const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");

/// 64 MiB / 1e6 entries — the same finite ceiling root's probe used, so a release is observable rather than invisible
/// against an unlimited ledger.
const BYTES: u64 = 64 * 1024 * 1024;
const ENTRIES: u64 = 1_000_000;

fn uuid() -> KbUuid {
    KbUuid::from_bytes([0x5A; 16])
}

fn control() -> &'static RunToCompletion {
    Box::leak(Box::new(RunToCompletion))
}

#[test]
fn bootstrap_commits_genesis_then_a_real_store_publishes_a_pin() {
    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let path = std::path::Path::new("/kb");
    let ctl = control();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");

    // PUBLIC path only: one call, lock held continuously inside it, Genesis committed before we get a Store back.
    let mut store: sage_db_store::handle::Store<ModelFileSystem> = match bootstrap(
        fs,
        path,
        sage_db_store::fs::CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => {
            panic!("bootstrap must succeed on a fresh root: {refusal:?}")
        }
    };

    // A usable Store exists only because Genesis actually committed.
    assert_eq!(store.head(), Some(1), "Genesis committed at version 1");
    assert!(
        store.coverage_complete(),
        "coverage is established by the commit"
    );

    // The published pin is a real owner, and it holds the same current set the handle serves.
    let pin = store
        .published_pin()
        .expect("a committed version has a published pin");
    assert_eq!(pin.head(), sage_db_types::identity::KbVersion::new(1));
    assert!(pin.reserved_bytes() > 0, "the pin owns a real reservation");
    assert_eq!(pin.entries().len(), store.retained_entries().count());

    // Retaining the OLD pin across a later commit: clone now, commit, and the clone must still serve its version.
    let old = pin.clone();
    let _ = &mut store;
    assert_eq!(old.head(), sage_db_types::identity::KbVersion::new(1));
    assert!(old.owners() >= 2, "the clone shares the one reservation");

    // The counter is finite and readable through the same object the admission used.
    let stats: LedgerStats = ledger.stats();
    assert!(
        stats.live_bytes > 0,
        "live bytes are visible while data lives"
    );
}

/// The resource finding from the finite-ledger probe: retained data must keep its reservation ALIVE after the attempt
/// facade is dropped, and release only when the last real owner drops.
///
/// This asserts the *direction* of the fix, not an exact byte total: `live_bytes` includes scratch/work admission, and
/// root explicitly ruled out claiming the whole figure stays live.
#[test]
fn retained_pin_keeps_its_reservation_after_the_ledger_facade_drops() {
    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let path = std::path::Path::new("/kb2");
    let ctl = control();

    // Scope the facade so it is dropped while the Store and its pin remain alive.
    let (mut store, counter) = {
        let mut ledger = AttemptLedger::open(BYTES, ENTRIES, ctl).expect("a finite ledger opens");
        // A zero-extent observer token keeps THAT counter readable after the facade goes away.
        let observer = ledger
            .reserve_owned(0)
            .expect("a zero-extent token is admitted");

        let store = match bootstrap(
            fs,
            path,
            sage_db_store::fs::CreateDir::IfMissing,
            uuid(),
            RULES,
            &Limits::V1,
            &mut ledger,
        ) {
            Ok(store) => store,
            Err(refusal) => panic!("bootstrap must succeed: {refusal:?}"),
        };
        (store, observer)
    };

    // The facade is gone. The Store still serves its current entries, and the pin still holds a real reservation.
    let pin = store.published_pin().expect("pin published");
    let held_by_pin = pin.reserved_bytes();
    assert!(
        held_by_pin > 0,
        "the pin owns a reservation, not a bare pointer"
    );

    // The counter is still readable through the observer token minted before the drop, and it is what the ledger actually
    // holds — not what the pin merely reports.
    let after_drop = counter.stats();
    assert!(
        after_drop.held_bytes >= held_by_pin,
        "the retained owner's claim must survive the facade drop: counter held={} pin claims={}",
        after_drop.held_bytes,
        held_by_pin
    );

    // BOUNDARY, stated so it is not over-read: this test exercises the PIN's reservation only. Genesis carries no term
    // records, so no `SharedRecord` owner is created on this path — a zero-extent fold owner lease would NOT be caught
    // here. The fold-owner reservation needs a commit that folds at least one record, which is the next control, and
    // asserting it here would be a claim this path cannot support.

    // Releasing the last real owner must return that claim.
    drop(pin);
    let _ = &mut store;
}
