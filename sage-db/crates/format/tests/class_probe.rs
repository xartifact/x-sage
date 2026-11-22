//! How the public read path classifies a legal file that exceeds a field bound.
use sage_db_format::{
    dispatch::from_read_error,
    field::read_str,
    wire::{DecodeError, ReadError, Reader},
};
use sage_db_types::{
    framing::{FormatError, OperatorAction, classify},
    ledger::{BudgetError, BudgetUnit, SharedLedger},
    query::ControlStop,
};

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

fn classified(error: ReadError) -> FormatError {
    from_read_error::<()>(error)
        .as_format_error()
        .expect("a data error is a classification, not a refusal")
}

#[test]
fn an_over_long_slug_is_a_limit_not_corruption() {
    // 200 bytes against a 120-byte bound: legal structure, over the ceiling.
    let mut bytes = 200u32.to_le_bytes().to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 200));
    let mut led = Open;
    let error = read_str(&mut Reader::new(&bytes), 120, &mut led).unwrap_err();
    assert!(matches!(
        error,
        ReadError::Data(DecodeError::StringTooLong { .. })
    ));
    let class = classified(error);
    assert!(matches!(class, FormatError::Limit(_)), "got {class:?}");
    assert_eq!(
        classify(&sage_db_types::framing::FormatOutcome::<()>::Format(class)),
        OperatorAction::RejectInput
    );
}

#[test]
fn a_zero_budget_read_is_a_refusal_not_a_classification() {
    struct Closed;
    impl SharedLedger for Closed {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Err(BudgetError::new(BudgetUnit::Entries, 1, 0))
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }
    let mut bytes = 4u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(b"abcd");
    let mut led = Closed;
    let outcome: sage_db_types::framing::FormatOutcome<()> =
        from_read_error(read_str(&mut Reader::new(&bytes), 64, &mut led).unwrap_err());
    assert!(
        outcome.as_format_error().is_none(),
        "budget must not become a verdict"
    );
}
