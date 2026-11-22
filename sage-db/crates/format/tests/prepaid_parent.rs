//! Reserve-before-charge and prepaid-credit properties on the real Writer.
use sage_db_format::{WriteError, Writer};
use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
struct Zero;
impl SharedLedger for Zero {
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
fn refusal_precedes_reservation() {
    let mut b = Vec::new();
    let mut l = Zero;
    let before = (b.len(), b.capacity());
    assert!(matches!(
        Writer::new(&mut b, &mut l).bytes(&[1; 4096]),
        Err(WriteError::Budget(_))
    ));
    assert_eq!(
        (b.len(), b.capacity()),
        before,
        "budget refusal retained new capacity"
    );
}
#[test]
fn prepaid_credit_cannot_be_exceeded() {
    let mut b = Vec::new();
    let mut l = Zero;
    let r = Writer::prepaid(&mut b, &mut l, 0).u8(1);
    assert!(r.is_err(), "zero prepaid credit became unlimited allowance");
    assert_eq!((b.len(), b.capacity()), (0, 0));
}
