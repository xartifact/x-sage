//! Public ingress checks with an exact lexical and copy budget.

use sage_db_protocol::ingress::{IngressClass, IngressLimits, decode_record_key};
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

struct Ledger {
    remaining: u64,
    stop_after_admission: bool,
    admitted: bool,
}

impl SharedLedger for Ledger {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        self.admitted = true;
        Ok(())
    }

    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        let Some(remaining) = self.remaining.checked_sub(count) else {
            return Err(BudgetError::new(BudgetUnit::Entries, count, self.remaining));
        };
        self.remaining = remaining;
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.stop_after_admission && self.admitted {
            return Err(ControlStop::Cancelled);
        }
        Ok(())
    }
}

#[test]
fn copy_spends_only_lexical_bytes_and_bytes_actually_copied() {
    let limits = IngressLimits::new(65536, 65536);
    for id in [
        "a".to_owned(),
        format!("{}é{}", "a".repeat(1023), "x".repeat(1024)),
    ] {
        let body = format!(r#"{{"id":"{id}"}}"#);
        let exact = u64::try_from(body.len() + id.len()).unwrap();
        let mut ledger = Ledger {
            remaining: exact,
            stop_after_admission: false,
            admitted: false,
        };
        let decoded = decode_record_key(body.as_bytes(), limits, &mut ledger)
            .expect("actual lexical and copy work fits the exact entry budget");
        assert_eq!(decoded.dto().id, id);
        assert_eq!(ledger.remaining, 0);
    }
}

#[test]
fn a_stop_during_copy_retains_the_field_and_stop_details() {
    let mut ledger = Ledger {
        remaining: 2048,
        stop_after_admission: true,
        admitted: false,
    };
    let result = decode_record_key(br#"{"id":"a"}"#, IngressLimits::new(16, 16), &mut ledger);
    let error = result.err().expect("copy must stop").0;
    assert_eq!(error.class, IngressClass::Refused);
    assert_eq!(
        error.refusal,
        Some(Refusal::Stopped(ControlStop::Cancelled))
    );
    assert_eq!(error.field, 2);
}
