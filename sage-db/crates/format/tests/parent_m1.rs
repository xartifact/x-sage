//! Parent reviewer's M1 counterexamples, preserved verbatim from
//! `codec-m1-parent.tuy1u2y7/sage-db/crates/format/tests/parent_m1.rs`.
//! They failed on the first snapshot (exit 101) and must keep passing.
use sage_db_format::{Reader, Writer, raw_sha256_bounded};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
struct L(u64, bool);
impl SharedLedger for L {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        if n > self.0 {
            return Err(BudgetError::new(BudgetUnit::Bytes, n, self.0));
        }
        self.0 -= n;
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.1 {
            Err(ControlStop::Cancelled)
        } else {
            Ok(())
        }
    }
}
#[test]
fn refused_string_keeps_output_unchanged() {
    let mut b = vec![];
    let mut l = L(4, false);
    let e = Writer::new(&mut b, &mut l).str("x");
    assert!(e.is_err());
    assert!(b.is_empty(), "refused string left prefix {b:?}");
}
#[test]
fn budget_error_is_not_data_error() {
    let mut b = vec![];
    let mut l = L(0, false);
    let e = Writer::new(&mut b, &mut l).str("x");
    assert_ne!(
        format!("{e:?}"),
        "Err(CountTooLong)",
        "budget laundered into data verdict"
    );
}
#[test]
fn failed_blob_read_does_not_consume_prefix() {
    let mut r = Reader::new(&[5, 0, 0, 0, 1]);
    assert!(r.blob(8).is_err());
    assert_eq!(r.offset(), 0, "failed composite read consumed prefix");
}
#[test]
fn empty_hash_observes_cancellation() {
    let mut l = L(0, true);
    assert!(
        raw_sha256_bounded(&[], &mut l).is_err(),
        "empty input bypassed control"
    );
}
