//! Registry holes and real field-table memory admission.
use sage_db_format::{ReadError, Reader, event::read_event, registry::is_registered_field};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
struct NoBytes;
impl SharedLedger for NoBytes {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        if n == 0 {
            Ok(())
        } else {
            Err(BudgetError::new(BudgetUnit::Bytes, n, 0))
        }
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}
#[test]
fn registry_holes_are_not_registered() {
    for tag in [24, 36, 52, 71, 87, 102, 117, 134, 155, 174, 188, 207] {
        assert!(!is_registered_field(tag), "unassigned tag {tag} accepted");
    }
    assert!(is_registered_field(99));
}
#[test]
fn decoded_field_table_requires_byte_budget() {
    let raw = [1, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7];
    let mut r = Reader::new(&raw);
    let result = read_event(&mut r, &mut NoBytes);
    assert!(
        matches!(result, Err(ReadError::Budget(_))),
        "table allocated despite zero bytes: {result:?}"
    );
}
