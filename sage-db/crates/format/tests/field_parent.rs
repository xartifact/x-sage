//! Independent declared-width and typed-value checks.
use sage_db_format::{ReadError, Reader, event::encode_with, field};
use sage_db_types::{
    RelationId,
    ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger},
    persist::{IdSpace, Watermark},
};
struct L(bool);
impl SharedLedger for L {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        if self.0 || n == 0 {
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
fn relation_has_two_byte_wire_width() {
    let v = RelationId::ALL[0];
    let b = encode_with(&mut L(true), |w| field::write_relation(w, v)).unwrap();
    assert_eq!(b, (v.get() as u16).to_le_bytes());
    let mut r = Reader::new(&[0, 0]);
    field::read_relation(&mut r).unwrap();
    assert!(r.is_empty());
}
#[test]
fn nul_string_is_rejected_on_write() {
    assert!(encode_with(&mut L(true), |w| field::write_str(w, "a\0b", 64)).is_err());
}
#[test]
fn nul_string_is_rejected_on_read() {
    let raw = [3, 0, 0, 0, b'a', 0, b'b'];
    assert!(field::validate_str(&mut Reader::new(&raw), 64).is_err());
}
#[test]
fn string_list_needs_byte_budget() {
    let raw = [1, 0, 0, 0, 1, 0, 0, 0, b'x'];
    let r = field::read_str_list(&mut Reader::new(&raw), 64, 128, &mut L(false));
    assert!(
        matches!(r, Err(ReadError::Budget(_))),
        "unmetered owned list {r:?}"
    );
}
#[test]
fn term_watermark_stays_within_term_space() {
    let m = Watermark {
        space: IdSpace::TermId,
        hwm: u32::MAX as u128 + 1,
    };
    assert!(encode_with(&mut L(true), |w| field::write_watermark(w, m)).is_err());
}
