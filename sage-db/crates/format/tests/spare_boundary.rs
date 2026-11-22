//! Crossing existing spare capacity uses additional length, not capacity growth.
use sage_db_format::Writer;
use sage_db_types::ledger::{BudgetError, ControlStop, SharedLedger};
struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}
#[test]
fn append_crossing_spare_capacity_is_reserved() {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&[1; 8]);
    let mut l = Open;
    Writer::new(&mut b, &mut l).bytes(&[2; 9]).unwrap();
    assert_eq!(b.len(), 17);
    assert_eq!(&b[8..], &[2; 9]);
}
