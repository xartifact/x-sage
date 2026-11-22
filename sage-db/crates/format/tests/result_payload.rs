//! Independent literal layout and malformed boundaries for persisted result bytes.
use sage_db_format::{
    result::{ResultReadError, read_result_payload},
    wire::{DecodeError, ReadError},
};
use sage_db_types::{
    framing::Refusal,
    identity::RecordRevision,
    ledger::{BudgetError, BudgetUnit, SharedLedger},
    persist::{IdSpace, RecordKey, RevPair, Watermark},
    query::ControlStop,
};
struct Meter {
    left: u64,
    charged: u64,
    bytes: u64,
    checks: u64,
    stop_at: Option<u64>,
}
impl Meter {
    fn open() -> Self {
        Self {
            left: 1000,
            charged: 0,
            bytes: 0,
            checks: 0,
            stop_at: None,
        }
    }
}
impl SharedLedger for Meter {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        self.bytes += n;
        Ok(())
    }
    fn try_charge_entries(&mut self, n: u64) -> Result<(), BudgetError> {
        if n > self.left {
            return Err(BudgetError::new(BudgetUnit::Entries, n, self.left));
        }
        self.left -= n;
        self.charged += n;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.checks += 1;
        if self.stop_at == Some(self.checks) {
            Err(ControlStop::Cancelled)
        } else {
            Ok(())
        }
    }
}
fn pair(kind: u16, id: u128, revision: u64) -> Vec<u8> {
    let mut b = kind.to_le_bytes().to_vec();
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&revision.to_le_bytes());
    b
}
fn counter(space: u16, hwm: u128) -> Vec<u8> {
    let mut b = space.to_le_bytes().to_vec();
    b.extend_from_slice(&hwm.to_le_bytes());
    b
}
fn bytes(pairs: &[Vec<u8>], counters: &[Vec<u8>]) -> Vec<u8> {
    let mut b = 0u16.to_le_bytes().to_vec();
    b.extend_from_slice(&2u64.to_le_bytes());
    b.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for p in pairs {
        b.extend_from_slice(p);
    }
    b.extend_from_slice(&(counters.len() as u32).to_le_bytes());
    for p in counters {
        b.extend_from_slice(p);
    }
    b
}
#[test]
fn independently_composed_tiny_layout_and_fixed_width_access() {
    let b = bytes(&[pair(1, 1, 1)], &[counter(0, 1)]);
    assert_eq!(b.len(), 62);
    let mut l = Meter::open();
    let r = read_result_payload(&b, 62, &mut l).unwrap();
    assert_eq!(
        (r.version(), r.affected_count(), r.counter_count()),
        (2, 1, 1)
    );
    assert_eq!(l.charged, 3);
    assert_eq!(l.bytes, 0);
    assert_eq!(
        r.affected_at(0, &mut l).unwrap(),
        Some(RevPair {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST
        })
    );
    assert_eq!(
        r.counter_at(0, &mut l).unwrap(),
        Some(Watermark {
            space: IdSpace::TermId,
            hwm: 1
        })
    );
    assert_eq!(r.affected_at(1, &mut l).unwrap(), None);
    assert_eq!(r.counter_at(u32::MAX, &mut l).unwrap(), None);
}
#[test]
fn ceiling_accepts_exact_bytes_and_refuses_first_smaller_policy() {
    let b = bytes(&[], &[]);
    let mut l = Meter::open();
    assert!(read_result_payload(&b, b.len() as u64, &mut l).is_ok());
    assert!(matches!(
        read_result_payload(&b, b.len() as u64 - 1, &mut l),
        Err(ResultReadError::Ceiling {
            size: 18,
            ceiling: 17
        })
    ));
}
#[test]
fn every_truncation_and_trailing_byte_refuses_no_partial_result() {
    let b = bytes(&[pair(1, 1, 1)], &[counter(0, 1)]);
    for end in 0..b.len() {
        assert!(matches!(
            read_result_payload(&b[..end], 1000, &mut Meter::open()),
            Err(ResultReadError::Wire(ReadError::Data(
                DecodeError::Truncated { .. }
            )))
        ));
    }
    let mut extra = b;
    extra.push(0);
    assert!(matches!(
        read_result_payload(&extra, 1000, &mut Meter::open()),
        Err(ResultReadError::Wire(ReadError::Data(
            DecodeError::Trailing { remaining: 1 }
        )))
    ));
}
#[test]
fn huge_framed_counts_refuse_before_any_declared_row_work() {
    let mut b = bytes(&[], &[]);
    b[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut l = Meter::open();
    assert!(matches!(
        read_result_payload(&b, 1000, &mut l),
        Err(ResultReadError::Wire(ReadError::Data(
            DecodeError::Truncated { .. }
        )))
    ));
    assert_eq!(l.charged, 1);
}
#[test]
fn unsupported_kind_and_zero_commit_version_remain_distinct() {
    let mut b = bytes(&[], &[]);
    b[0] = 1;
    assert_eq!(
        read_result_payload(&b, 1000, &mut Meter::open()).unwrap_err(),
        ResultReadError::UnsupportedKind(1)
    );
    b[0] = 0;
    b[2..10].fill(0);
    assert_eq!(
        read_result_payload(&b, 1000, &mut Meter::open()).unwrap_err(),
        ResultReadError::InvalidVersion
    );
}
#[test]
fn affected_pair_order_and_counter_order_are_strict() {
    for p in [
        vec![pair(1, 2, 1), pair(1, 1, 1)],
        vec![pair(1, 1, 1), pair(1, 1, 1)],
    ] {
        assert_eq!(
            read_result_payload(&bytes(&p, &[]), 1000, &mut Meter::open()).unwrap_err(),
            ResultReadError::AffectedOrder { index: 1 }
        );
    }
    for c in [
        vec![counter(2, 1), counter(0, 1)],
        vec![counter(0, 1), counter(0, 2)],
    ] {
        assert_eq!(
            read_result_payload(&bytes(&[], &c), 1000, &mut Meter::open()).unwrap_err(),
            ResultReadError::CounterOrder { index: 1 }
        );
    }
}
#[test]
fn key_revision_and_watermark_domains_use_the_canonical_readers() {
    for p in [
        pair(99, 1, 1),
        pair(1, u32::MAX as u128 + 1, 1),
        pair(1, 1, 0),
    ] {
        assert!(matches!(
            read_result_payload(&bytes(&[p], &[]), 1000, &mut Meter::open()),
            Err(ResultReadError::Wire(ReadError::Data(_)))
        ));
    }
    for c in [counter(99, 1), counter(0, u32::MAX as u128 + 1)] {
        assert!(matches!(
            read_result_payload(&bytes(&[], &[c]), 1000, &mut Meter::open()),
            Err(ResultReadError::Wire(ReadError::Data(_)))
        ));
    }
    let b = bytes(&[pair(8, u128::MAX, 1)], &[counter(7, u128::MAX)]);
    assert!(read_result_payload(&b, 1000, &mut Meter::open()).is_ok());
}
#[test]
fn real_entry_allowance_and_control_are_kept_distinct_even_empty() {
    let b = bytes(&[pair(1, 1, 1)], &[counter(0, 1)]);
    let mut l = Meter::open();
    l.left = 2;
    assert!(
        matches!(read_result_payload(&b,1000,&mut l),Err(ResultReadError::Refused(Refusal::Budget(e))) if e.unit()==BudgetUnit::Entries&&e.requested()==1&&e.remaining()==0)
    );
    let mut l = Meter::open();
    l.left = 3;
    assert!(read_result_payload(&b, 1000, &mut l).is_ok());
    assert_eq!(l.left, 0);
    for cp in 1..=4 {
        let mut l = Meter::open();
        l.stop_at = Some(cp);
        assert_eq!(
            read_result_payload(&b, 1000, &mut l).unwrap_err(),
            ResultReadError::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );
    }
    for cp in 1..=2 {
        let mut l = Meter::open();
        l.stop_at = Some(cp);
        assert_eq!(
            read_result_payload(&bytes(&[], &[]), 1000, &mut l).unwrap_err(),
            ResultReadError::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );
    }
}
#[test]
fn borrowed_access_has_entry_and_exit_control_and_no_copies() {
    let b = bytes(&[pair(1, 1, 1)], &[]);
    let r = read_result_payload(&b, 1000, &mut Meter::open()).unwrap();
    for cp in 1..=2 {
        let mut l = Meter::open();
        l.stop_at = Some(cp);
        assert_eq!(
            r.affected_at(0, &mut l).unwrap_err(),
            ResultReadError::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );
    }
}
