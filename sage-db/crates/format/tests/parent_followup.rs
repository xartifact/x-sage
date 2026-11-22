//! Independent M1 resource follow-up properties.
use sage_db_format::{CanonicalRules, WriteError, Writer};
use sage_db_types::{
    RuleProfile,
    ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger},
};
struct L {
    remaining: u64,
    spent: u64,
    cancelled: bool,
}
impl SharedLedger for L {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        if n > self.remaining {
            return Err(BudgetError::new(BudgetUnit::Bytes, n, self.remaining));
        }
        self.remaining -= n;
        self.spent += n;
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.cancelled {
            Err(ControlStop::Cancelled)
        } else {
            Ok(())
        }
    }
}
#[test]
fn exact_rules_budget_is_not_charged_twice() {
    let mut l = L {
        remaining: 381,
        spent: 0,
        cancelled: false,
    };
    let r = CanonicalRules::new(RuleProfile::FormalV1).encode(&mut l);
    assert!(r.is_ok(), "exact budget refused {r:?}, spent {}", l.spent);
    assert_eq!(l.spent, 381);
}
#[test]
fn cancelled_writer_appends_nothing() {
    let mut l = L {
        remaining: 8,
        spent: 0,
        cancelled: true,
    };
    let mut b = vec![];
    assert!(matches!(
        Writer::new(&mut b, &mut l).u8(1),
        Err(WriteError::Stopped(ControlStop::Cancelled))
    ));
    assert!(b.is_empty());
}
#[test]
fn writer_capacity_is_admitted() {
    let mut l = L {
        remaining: 100,
        spent: 0,
        cancelled: false,
    };
    let mut b = vec![];
    Writer::new(&mut b, &mut l).u8(1).unwrap();
    assert!(
        b.capacity() as u64 <= l.spent,
        "capacity {} exceeds charge {}",
        b.capacity(),
        l.spent
    );
}
#[test]
fn budget_error_preserves_typed_arm() {
    let mut l = L {
        remaining: 0,
        spent: 0,
        cancelled: false,
    };
    let mut b = vec![];
    assert!(matches!(
        Writer::new(&mut b, &mut l).str("x"),
        Err(WriteError::Budget(_))
    ));
}
