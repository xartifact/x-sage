//! Isolate the second, borrowed field-reference table from output storage.
use sage_db_format::{WriteError, Writer, record::FieldSet};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
struct L(u64);
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
        Ok(())
    }
}
#[test]
fn borrowed_field_table_is_admitted() {
    let mut l = L(std::mem::size_of::<(u16, Vec<u8>)>() as u64 + 1);
    let mut set = FieldSet::with_capacity(1, &mut l).unwrap();
    set.push(&mut l, 1, 1u8, |w, v| w.u8(v)).unwrap();
    assert_eq!(l.0, 0);
    let mut out = Vec::with_capacity(64);
    let r = set.write_event(&mut Writer::new(&mut out, &mut l), 1);
    assert!(
        matches!(r, Err(WriteError::Budget(_))),
        "second field-reference table escaped byte budget: {r:?}"
    );
}
