//! Primitive encoders and the canonical-order rules they rest on.
//!
//! Every writer in this crate goes through these functions, so there is exactly one
//! definition of "little-endian, fixed width, no padding" and one place to audit.
//!
//! Two rules that are cheap to get wrong and expensive to discover later:
//!
//! * **Ordering is numeric, not byte-wise.** A little-endian `u128` sorts differently
//!   from its numeric value, so "sort the encoded bytes" is a bug that looks correct
//!   until the first id above 2^32.
//! * **Absence is absence.** An optional value is either a present field or no field at
//!   all; there is no zero-length sentinel and no `present` flag byte, because two
//!   encodings of the same absence make the canonical digest depend on the writer.
//!
//! ## Resource contract
//!
//! Three separate failure kinds travel in [`WriteError`] / [`ReadError`] and are never
//! converted into one another:
//!
//! | kind | meaning | who fixes it |
//! |---|---|---|
//! | [`EncodeError`] / [`DecodeError`] | the data cannot be represented or parsed | the caller's input |
//! | [`BudgetError`] | this attempt's shared budget was too small | the caller's budget |
//! | [`ControlStop`] | the caller's policy said stop | the caller |
//!
//! ## Atomicity
//!
//! A **field** that is refused leaves the caller's buffer exactly as it was: the whole
//! append is admitted and capacity-reserved *before* the first byte is written. A
//! composite structure that fails partway may leave earlier complete fields behind —
//! that is stated rather than implied, and callers that need all-or-nothing build into
//! a scratch buffer and copy once.
//!
//! Reads are transactional per read: a failed read restores the cursor, so a caller can
//! retry a shorter read or report a precise offset without a half-consumed structure.

use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

/// Why a write was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteError {
    /// The value cannot be represented in canonical form. No budget would help.
    Data(EncodeError),
    /// This attempt's budget refused the append. Nothing was written.
    Budget(BudgetError),
    /// The caller asked to stop. Nothing was written.
    Stopped(ControlStop),
    /// The allocator could not provide the space. Nothing was written.
    Allocation {
        /// Bytes the append needed.
        requested: usize,
    },
    /// A field that this event requires was not supplied.
    MissingRequiredField {
        /// The event tag.
        event: u16,
        /// The required field tag.
        field: u16,
    },
    /// This crate's own invariant was broken by the caller's usage - for example pushing
    /// more fields than the admitted capacity allows. Never a data verdict.
    Internal {
        /// A fixed description of the violated invariant.
        what: &'static str,
    },
}

impl From<EncodeError> for WriteError {
    fn from(error: EncodeError) -> Self {
        Self::Data(error)
    }
}

impl From<BudgetError> for WriteError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ControlStop> for WriteError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// Why a value cannot be represented.
///
/// Deliberately *not* convertible from [`BudgetError`]: a budget refusal is not a data
/// verdict, and a `From` impl between them is how "retry with more budget" becomes
/// "your input is malformed".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EncodeError {
    /// An invalid slug or session ordinal in a public checkpoint locator.
    InvalidCheckpointLocation,
    /// A string exceeded the `u32` length prefix.
    StringTooLong,
    /// A byte string exceeded the `u32` length prefix.
    BlobTooLong,
    /// A collection count exceeded the `u32` length prefix.
    CountTooLong,
    /// A field's value was empty; every registered type has a minimum size of one byte.
    EmptyField {
        /// The offending tag.
        tag: u16,
    },
    /// A field tag appeared out of ascending order or twice.
    FieldOrder {
        /// The offending tag.
        tag: u16,
    },
    /// `Event.flags` carried a reserved bit; the writer must not emit a value its own
    /// reader refuses.
    EventFlags {
        /// The flags that were requested.
        flags: u16,
    },
    /// `Field.flags` must be zero and the writer was asked to emit otherwise.
    FieldFlags {
        /// The flags that were requested.
        flags: u16,
    },
    /// An identifier did not fit the bound its kind declares.
    IdOutOfRange {
        /// The record kind whose bound was exceeded.
        kind: u16,
    },
    /// The text contained a NUL byte, which the canonical string form forbids.
    NulInString,
    /// A field that this event may not carry was supplied.
    ForbiddenField {
        /// The event tag.
        event: u16,
        /// The forbidden field tag.
        field: u16,
    },
    /// A field's value contradicts the event kind.
    StatusMismatch {
        /// The event tag.
        event: u16,
    },
    /// `recorded_by` did not agree with `actor_origin`.
    ActorOriginMismatch {
        /// The field tag involved.
        tag: u16,
    },
    /// A creation event carried a revision other than the first, or a non-creation
    /// event carried the first revision.
    WrongRevision {
        /// The event tag.
        event: u16,
        /// The revision supplied.
        revision: u64,
    },
    /// A field that this event requires was not supplied.
    MissingRequiredField {
        /// The event tag.
        event: u16,
        /// The required field tag.
        field: u16,
    },
    /// A closed-domain field carried a value outside its registered set.
    UnknownEnumValue {
        /// The field tag.
        tag: u16,
        /// The value that was supplied.
        value: u32,
    },
}

/// Why a read was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// The bytes are not a legal encoding.
    Data(DecodeError),
    /// This attempt's budget refused an allocation the decode needed.
    Budget(BudgetError),
    /// The caller asked to stop before the read.
    Stopped(ControlStop),
    /// The allocator could not provide the requested space.
    Allocation {
        /// Bytes the decode needed.
        requested: usize,
    },
    /// The envelope framed correctly but the schema does not accept it.
    ///
    /// Kept separate from [`Self::Data`] on purpose: "the bytes are malformed" and "this
    /// major cannot interpret these well-formed bytes" lead to different operator
    /// actions (restore versus migrate).
    Uninterpretable(crate::registry::SchemaError),
}

impl From<BudgetError> for ReadError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<DecodeError> for ReadError {
    fn from(error: DecodeError) -> Self {
        Self::Data(error)
    }
}

impl From<ControlStop> for ReadError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// Appends canonical bytes, admitting every append before it mutates.
pub struct Writer<'a> {
    buf: &'a mut Vec<u8>,
    ledger: &'a mut dyn SharedLedger,
    /// Capacity already covered by admission. Every reservation compares against this
    /// and charges the delta **before** the allocator is asked for anything, so a
    /// refusal cannot leave new capacity behind.
    admitted_capacity: usize,
    /// Remaining prepaid credit. `None` means "charge per append"; `Some(n)` means the
    /// caller already paid for `n` more bytes of capacity and appends draw from it.
    ///
    /// This is a *credit*, not an exemption: it decreases as capacity is admitted and a
    /// zero credit refuses the next append. A boolean "prepaid" flag would be an
    /// unlimited allowance wearing a payer's name.
    prepaid_credit: Option<usize>,
}

impl<'a> Writer<'a> {
    /// Wrap a caller-owned buffer and the attempt's ledger.
    ///
    /// A non-empty buffer arrives with capacity the caller already owns and already
    /// paid for elsewhere; that extent is recorded as previously admitted so it is
    /// neither re-charged nor re-reserved. Only growth beyond it is charged here.
    pub fn new(buf: &'a mut Vec<u8>, ledger: &'a mut dyn SharedLedger) -> Self {
        let existing = buf.capacity();
        Self {
            buf,
            ledger,
            admitted_capacity: existing,
            prepaid_credit: None,
        }
    }

    /// Wrap a buffer whose capacity was already charged by the caller.
    ///
    /// `precharged` is the number of bytes the caller paid for. Appends draw from that
    /// credit and are refused when it runs out; growth beyond it is not silently free.
    ///
    /// Kept public because the property it now enforces (credit, not exemption) is
    /// testable from outside; it is not an internal escape hatch.
    pub fn prepaid(
        buf: &'a mut Vec<u8>,
        ledger: &'a mut dyn SharedLedger,
        precharged: usize,
    ) -> Self {
        // Existing capacity belongs to the caller, as for `new`; prepaid credit covers
        // additional growth only.
        let existing = buf.capacity();
        Self {
            buf,
            ledger,
            admitted_capacity: existing,
            prepaid_credit: Some(precharged),
        }
    }

    /// Admit control and capacity for `len` more bytes, then reserve.
    ///
    /// Order matters and is the whole point of this method:
    ///
    /// 1. control checkpoint — a cancelled attempt touches nothing;
    /// 2. compute the capacity actually needed (`len` at minimum) and the delta beyond
    ///    what is already admitted;
    /// 3. charge that delta (or draw it from prepaid credit) — **before** the allocator
    ///    is asked for anything, so a refusal leaves capacity exactly as it was;
    /// 4. reserve exactly that many bytes, fallibly;
    /// 5. record the admitted capacity.
    ///
    /// Step 4 cannot fail for budget because step 3 already paid for it.
    /// The attempt's ledger.
    ///
    /// Exposed so a record codec can encode small field values into their own buffers
    /// under the same accounting. It is a reborrow of the one ledger, not a second
    /// counter: bytes charged through it are charged to this writer's attempt.
    pub fn ledger(&mut self) -> &mut dyn SharedLedger {
        self.ledger
    }

    fn admit(&mut self, len: usize) -> Result<(), WriteError> {
        self.ledger.checkpoint()?;
        // FOUR quantities, and conflating any two of them has produced a real bug:
        //   len_now      - bytes already present
        //   needed       - length the buffer must hold after this append
        //   wanted_cap   - capacity that satisfies `needed` (current capacity if it
        //                  already fits, else `needed`)
        //   reserve_add  - what `try_reserve_exact` takes, which is ADDITIONAL relative
        //                  to `len()`, i.e. `needed - len_now`
        //
        // The charge is a *capacity* delta (`wanted_cap - admitted_capacity`); the
        // reservation argument is a *length* delta. Passing capacity growth as the
        // reserve argument under-reserves whenever spare capacity is partly used:
        // cap16/len8 needing 17 has growth 1, and reserving 1 only guarantees 9.
        let len_now = self.buf.len();
        let needed = len_now
            .checked_add(len)
            .ok_or(WriteError::Allocation { requested: len })?;
        let current_cap = self.buf.capacity();
        let wanted_cap = if needed <= current_cap {
            current_cap
        } else {
            needed
        };
        let reserve_add = needed - len_now;
        let delta = wanted_cap.saturating_sub(self.admitted_capacity);

        if delta > 0 {
            match self.prepaid_credit.as_mut() {
                Some(credit) => {
                    // Draw from prepaid credit; refuse rather than overdraw.
                    if delta > *credit {
                        return Err(WriteError::Budget(BudgetError::new(
                            BudgetUnit::Bytes,
                            delta as u64,
                            *credit as u64,
                        )));
                    }
                    *credit -= delta;
                }
                None => {
                    self.ledger.try_charge(delta as u64)?;
                }
            }
        }

        // Reserve the LENGTH delta, not the capacity delta. This is the step that must
        // guarantee the append can complete without the allocator being consulted again.
        self.buf
            .try_reserve_exact(reserve_add)
            .map_err(|_| WriteError::Allocation {
                requested: reserve_add,
            })?;
        // Future deltas compare against what the buffer can now hold.
        self.admitted_capacity = self.buf.capacity();
        Ok(())
    }

    /// Append bytes already admitted by [`Self::admit`].
    fn append_admitted(&mut self, value: &[u8]) {
        debug_assert!(
            self.buf.capacity() - self.buf.len() >= value.len(),
            "append_admitted requires a preceding admit for this length"
        );
        self.buf.extend_from_slice(value);
    }

    /// Append raw bytes atomically.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn bytes(&mut self, value: &[u8]) -> Result<(), WriteError> {
        self.admit(value.len())?;
        self.append_admitted(value);
        Ok(())
    }

    /// Append one byte.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn u8(&mut self, value: u8) -> Result<(), WriteError> {
        self.bytes(&[value])
    }

    /// Append a `u16`, little-endian.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn u16(&mut self, value: u16) -> Result<(), WriteError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a `u32`, little-endian.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn u32(&mut self, value: u32) -> Result<(), WriteError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a `u64`, little-endian.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn u64(&mut self, value: u64) -> Result<(), WriteError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a `u128`, little-endian.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn u128(&mut self, value: u128) -> Result<(), WriteError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append an `i64`, little-endian two's complement.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is appended on failure.
    pub fn i64(&mut self, value: i64) -> Result<(), WriteError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a length-prefixed UTF-8 string atomically.
    ///
    /// The prefix and body are one field: a refusal writes neither, so the buffer never
    /// holds a length whose bytes are missing.
    ///
    /// # Errors
    ///
    /// [`WriteError::Data`] above `u32::MAX` bytes, when the text contains a NUL byte
    /// (`storage-format` §1 forbids it and §9.2 classifies it as corruption), or a
    /// budget/allocation refusal.
    pub fn str(&mut self, value: &str) -> Result<(), WriteError> {
        if value.as_bytes().contains(&0) {
            return Err(WriteError::Data(EncodeError::NulInString));
        }
        self.blob(value.as_bytes())
    }

    /// Append a length-prefixed byte string atomically.
    ///
    /// # Errors
    ///
    /// [`WriteError::Data`] above `u32::MAX` bytes, or a budget/allocation refusal.
    pub fn blob(&mut self, value: &[u8]) -> Result<(), WriteError> {
        let len = u32::try_from(value.len()).map_err(|_| EncodeError::BlobTooLong)?;
        self.admit(4 + value.len())?;
        self.append_admitted(&len.to_le_bytes());
        self.append_admitted(value);
        Ok(())
    }

    /// Append a `u32` count for `len` elements.
    ///
    /// The caller writes the elements; this does not sort or validate order, because
    /// silently sorting would hide a caller that built a set in the wrong order.
    ///
    /// # Errors
    ///
    /// [`WriteError::Data`] above `u32::MAX`, or a budget/allocation refusal.
    pub fn count(&mut self, len: usize) -> Result<(), WriteError> {
        let n = u32::try_from(len).map_err(|_| EncodeError::CountTooLong)?;
        self.u32(n)
    }
}

/// A cursor over canonical bytes.
///
/// Every read is bounds-checked and transactional: on failure the cursor is restored,
/// so no half-consumed structure is ever reported.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wrap a byte slice.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Remaining unread bytes.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Current offset, for error reporting.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.pos
    }

    /// The next event's tag, without consuming it.
    ///
    /// A dispatcher needs the tag before it can choose a family's reader, and that
    /// reader parses the envelope itself. Peeking (rather than calling
    /// [`crate::event::read_event`] and re-encoding) keeps one parse per event and
    /// cannot disagree with what the family reader will see.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than two bytes remain.
    pub fn peek_event_tag(&self) -> Result<u16, DecodeError> {
        if self.remaining() < 2 {
            return Err(DecodeError::Truncated {
                offset: self.pos,
                wanted: 2,
            });
        }
        Ok(u16::from_le_bytes([
            self.bytes[self.pos],
            self.bytes[self.pos + 1],
        ]))
    }

    /// Run `f` and restore the cursor if it fails.
    ///
    /// This is what makes the per-read transaction claim true rather than aspirational.
    fn transactional<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<T, DecodeError> {
        let start = self.pos;
        match f(self) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.pos = start;
                Err(error)
            }
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let start = self.pos;
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated {
            offset: start,
            wanted: n,
        })?;
        if end > self.bytes.len() {
            return Err(DecodeError::Truncated {
                offset: start,
                wanted: n,
            });
        }
        let out = &self.bytes[start..end];
        self.pos = end;
        Ok(out)
    }

    /// Read one byte.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than one byte remains.
    pub fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.transactional(|r| Ok(r.take(1)?[0]))?)
    }

    /// Read a `u16`, little-endian.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than two bytes remain.
    pub fn u16(&mut self) -> Result<u16, ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(2)?;
            Ok(u16::from_le_bytes([b[0], b[1]]))
        })?)
    }

    /// Read a `u32`, little-endian.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than four bytes remain.
    pub fn u32(&mut self) -> Result<u32, ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        })?)
    }

    /// Read a `u64`, little-endian.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than eight bytes remain.
    pub fn u64(&mut self) -> Result<u64, ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            Ok(u64::from_le_bytes(a))
        })?)
    }

    /// Read a `u128`, little-endian.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than sixteen bytes remain.
    pub fn u128(&mut self) -> Result<u128, ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(16)?;
            let mut a = [0u8; 16];
            a.copy_from_slice(b);
            Ok(u128::from_le_bytes(a))
        })?)
    }

    /// Read an `i64`, little-endian two's complement.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than eight bytes remain.
    pub fn i64(&mut self) -> Result<i64, ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            Ok(i64::from_le_bytes(a))
        })?)
    }

    /// Read a `u32` length then that many UTF-8 bytes, transactionally.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`], [`DecodeError::NotUtf8`],
    /// [`DecodeError::NulInString`], or [`DecodeError::StringTooLong`] against `max`.
    pub fn str(&mut self, max: u32) -> Result<&'a str, ReadError> {
        Ok(self.transactional(|r| {
            let raw = r.blob_inner(max)?;
            let text = core::str::from_utf8(raw).map_err(|_| DecodeError::NotUtf8)?;
            if text.as_bytes().contains(&0) {
                return Err(DecodeError::NulInString);
            }
            Ok(text)
        })?)
    }

    /// Read a `u32` length then that many bytes, transactionally.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`], or [`DecodeError::StringTooLong`] against `max`.
    pub fn blob(&mut self, max: u32) -> Result<&'a [u8], ReadError> {
        Ok(self.transactional(|r| r.blob_inner(max))?)
    }

    fn blob_inner(&mut self, max: u32) -> Result<&'a [u8], DecodeError> {
        let start = self.pos;
        let len = self.u32_inner()?;
        if len > max {
            return Err(DecodeError::StringTooLong { len, max });
        }
        // Read the body only after the length was accepted, so an oversize length is
        // refused before any bytes are consumed.
        match self.take(len as usize) {
            Ok(body) => Ok(body),
            Err(_) => {
                self.pos = start;
                Err(DecodeError::Truncated {
                    offset: start,
                    wanted: 4 + len as usize,
                })
            }
        }
    }

    fn u32_inner(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a `u32` count, requiring it not to exceed `max`.
    ///
    /// The check happens before the caller allocates, so a hostile count cannot become
    /// a `Vec::with_capacity`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::CountTooLong`] against `max`.
    pub fn count(&mut self, max: u32) -> Result<u32, ReadError> {
        Ok(self.transactional(|r| {
            let n = r.u32_inner()?;
            if n > max {
                return Err(DecodeError::CountTooLong { count: n, max });
            }
            Ok(n)
        })?)
    }

    /// Read exactly `n` bytes.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than `n` remain.
    pub fn take_exact(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        Ok(self.transactional(|r| r.take(n))?)
    }

    /// Read a fixed 32-byte digest.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] when fewer than 32 bytes remain.
    pub fn digest32(&mut self) -> Result<[u8; 32], ReadError> {
        Ok(self.transactional(|r| {
            let b = r.take(32)?;
            let mut a = [0u8; 32];
            a.copy_from_slice(b);
            Ok(a)
        })?)
    }

    /// Fail if bytes remain.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Trailing`] when the structure did not consume the whole input.
    pub fn finish(self) -> Result<(), ReadError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ReadError::Data(DecodeError::Trailing {
                remaining: self.remaining(),
            }))
        }
    }
}

/// A byte sequence is not a legal canonical encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// An invalid slug or session ordinal in a public checkpoint locator.
    InvalidCheckpointLocation,
    /// The buffer ended before the structure did.
    Truncated {
        /// Offset where the read began.
        offset: usize,
        /// Bytes wanted.
        wanted: usize,
    },
    /// A string was not valid UTF-8.
    NotUtf8,
    /// A length exceeded the field's declared maximum.
    StringTooLong {
        /// Declared length.
        len: u32,
        /// Field maximum.
        max: u32,
    },
    /// A count exceeded the collection's declared maximum.
    CountTooLong {
        /// Declared count.
        count: u32,
        /// Collection maximum.
        max: u32,
    },
    /// Bytes remained after the structure was fully decoded.
    Trailing {
        /// Unconsumed bytes.
        remaining: usize,
    },
    /// A field tag appeared out of ascending order or twice.
    FieldOrder {
        /// The offending tag.
        tag: u16,
    },
    /// A field's encoded length was zero, which no registered type allows.
    ZeroLengthField {
        /// The offending tag.
        tag: u16,
    },
    /// `Field.flags` carried a non-zero value; it is reserved and must be zero.
    FieldFlags {
        /// The offending tag.
        tag: u16,
        /// The flags that were present.
        flags: u16,
    },
    /// `Event.flags` carried a reserved bit.
    EventFlags {
        /// The flags that were present.
        flags: u16,
    },
    /// A field tag is not registered in this format major.
    ///
    /// Distinct from a framing error: the bytes are well-formed, the tag simply has no
    /// meaning this major can honour.
    UnregisteredField {
        /// The offending tag.
        tag: u16,
    },
    /// An enum discriminant had no registered meaning.
    UnknownEnum {
        /// The field tag being decoded.
        tag: u16,
        /// The value that appeared.
        value: u16,
    },
    /// A revision was zero, which no record uses.
    InvalidRevision {
        /// The value that appeared.
        value: u64,
    },
    /// The text contained a NUL byte, which the canonical string form forbids.
    NulInString,
    /// An extension key violated its grammar or limits.
    InvalidExtensionKey,
    /// A required field for this event was absent.
    MissingField {
        /// The event tag.
        event: u16,
        /// The missing field tag.
        field: u16,
    },
    /// A field this event may not carry was present.
    ForbiddenField {
        /// The event tag.
        event: u16,
        /// The forbidden field tag.
        field: u16,
    },
    /// The status field contradicted the event kind.
    StatusMismatch {
        /// The event tag.
        event: u16,
    },
    /// `recorded_by` did not agree with `actor_origin`.
    ActorOriginMismatch {
        /// The field tag involved.
        tag: u16,
    },
    /// A creation event carried a revision other than the first, or a non-creation
    /// event carried the first revision.
    WrongRevision {
        /// The event tag.
        event: u16,
        /// The revision present.
        revision: u64,
    },
    /// The record key's kind did not match the event's record class.
    RecordKeyKindMismatch {
        /// The event tag.
        event: u16,
        /// The kind that appeared.
        key_kind: u16,
    },
    /// An identifier did not fit the bound its space declares.
    WatermarkOutOfRange {
        /// The allocation space whose bound was exceeded.
        space: u16,
        /// The value that appeared.
        value: u128,
    },
    /// An identifier did not fit the bound its kind declares.
    IdOutOfRange {
        /// The record kind whose bound was exceeded.
        kind: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetUnit;

    struct Open;

    impl SharedLedger for Open {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    struct WithBytes(u64);

    impl SharedLedger for WithBytes {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.0 {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.0));
            }
            self.0 -= bytes;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    #[test]
    fn integers_round_trip_at_their_declared_widths() {
        let mut buf = Vec::new();
        let mut led = Open;
        {
            let mut w = Writer::new(&mut buf, &mut led);
            w.u8(0x11).unwrap();
            w.u16(0x2233).unwrap();
            w.u32(0x44556677).unwrap();
            w.u64(0x8899_aabb_ccdd_eeff).unwrap();
            w.u128(u128::MAX).unwrap();
            w.i64(-2).unwrap();
        }
        assert_eq!(buf.len(), 1 + 2 + 4 + 8 + 16 + 8);
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8().unwrap(), 0x11);
        assert_eq!(r.u16().unwrap(), 0x2233);
        assert_eq!(r.u32().unwrap(), 0x44556677);
        assert_eq!(r.u64().unwrap(), 0x8899_aabb_ccdd_eeff);
        assert_eq!(r.u128().unwrap(), u128::MAX);
        assert_eq!(r.i64().unwrap(), -2);
        assert!(r.is_empty());
    }

    /// A budget refusal must arrive as a budget refusal, never as a data verdict.
    #[test]
    fn a_budget_refusal_is_not_laundered_into_a_data_error() {
        let mut buf = Vec::new();
        let mut led = WithBytes(0);
        let err = Writer::new(&mut buf, &mut led)
            .blob(b"x")
            .expect_err("no budget");
        assert_eq!(
            err,
            WriteError::Budget(BudgetError::new(BudgetUnit::Bytes, 5, 0)),
            "a budget refusal must stay a budget refusal"
        );
        assert!(!matches!(err, WriteError::Data(_)));
    }

    /// A refused field leaves the caller's buffer untouched: prefix and body together.
    #[test]
    fn a_refused_field_is_atomic() {
        let mut buf = Vec::new();
        // Exactly 4 bytes: enough for a length prefix, not enough for prefix + body.
        let mut led = WithBytes(4);
        let err = Writer::new(&mut buf, &mut led)
            .str("x")
            .expect_err("cannot fit prefix plus body");
        assert!(matches!(err, WriteError::Budget(_)));
        assert!(
            buf.is_empty(),
            "a refused string must not leave a length prefix behind; got {buf:?}"
        );
    }

    #[test]
    fn truncation_is_typed_and_consumes_nothing() {
        let mut r = Reader::new(&[0x01]);
        let err = r.u32().expect_err("too short");
        assert_eq!(
            err,
            ReadError::Data(DecodeError::Truncated {
                offset: 0,
                wanted: 4
            })
        );
        assert_eq!(r.offset(), 0, "a failed read must not advance the cursor");
        assert_eq!(r.remaining(), 1);
    }

    /// The parent's counterexample: an oversize blob length must not consume the length
    /// prefix before failing.
    #[test]
    fn an_oversize_length_does_not_consume_the_prefix() {
        let bytes = [5u8, 0, 0, 0, 1];
        let mut r = Reader::new(&bytes);
        let err = r.blob(4).expect_err("declared 5 exceeds max 4");
        assert_eq!(
            err,
            ReadError::Data(DecodeError::StringTooLong { len: 5, max: 4 })
        );
        assert_eq!(r.offset(), 0, "cursor must be restored, not left at 4");
    }

    /// A body short of its declared length must also restore the cursor.
    #[test]
    fn a_short_body_restores_the_cursor() {
        let bytes = [9u8, 0, 0, 0, b'a', b'b'];
        let mut r = Reader::new(&bytes);
        let err = r.blob(16).expect_err("declared 9, only 2 present");
        assert!(matches!(
            err,
            ReadError::Data(DecodeError::Truncated { offset: 0, .. })
        ));
        assert_eq!(r.offset(), 0);
    }

    #[test]
    fn counts_are_checked_against_the_collection_maximum() {
        let mut buf = Vec::new();
        let mut led = Open;
        Writer::new(&mut buf, &mut led).count(1000).unwrap();
        let mut r = Reader::new(&buf);
        let err = r.count(16).expect_err("1000 exceeds max 16");
        assert_eq!(
            err,
            ReadError::Data(DecodeError::CountTooLong {
                count: 1000,
                max: 16
            })
        );
        assert_eq!(r.offset(), 0);
    }

    #[test]
    fn strings_round_trip_and_reject_non_utf8() {
        let mut buf = Vec::new();
        let mut led = Open;
        Writer::new(&mut buf, &mut led).str("terminal λ").unwrap();
        assert_eq!(Reader::new(&buf).str(64).unwrap(), "terminal λ");

        let mut bad = Vec::new();
        Writer::new(&mut bad, &mut led).u32(1).unwrap();
        bad.push(0xFF);
        let mut r = Reader::new(&bad);
        assert_eq!(r.str(64), Err(ReadError::Data(DecodeError::NotUtf8)));
        assert_eq!(r.offset(), 0, "a non-UTF-8 string consumes nothing");
    }

    /// Strengthened ordering test: the ledger must be consulted *before* any reservation,
    /// so a refusal on the very first charge cannot have grown the buffer.
    ///
    /// The earlier version of this test passed even with reserve-before-charge, because
    /// a 4096-byte request against an empty `Vec` happens not to change capacity in that
    /// particular allocator path. This variant fails deterministically instead: it
    /// records the charge count and asserts capacity is still zero afterwards.
    #[test]
    fn charging_precedes_any_reservation() {
        struct CountRefusals {
            calls: u32,
        }
        impl SharedLedger for CountRefusals {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                self.calls += 1;
                if bytes == 0 {
                    Ok(())
                } else {
                    Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
                }
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let mut buf = Vec::new();
        let mut led = CountRefusals { calls: 0 };
        let err = Writer::new(&mut buf, &mut led)
            .bytes(&[1u8; 4096])
            .expect_err("no budget");
        assert!(matches!(err, WriteError::Budget(_)));
        assert_eq!(
            led.calls, 1,
            "the ledger must be consulted exactly once here"
        );
        assert_eq!(
            buf.capacity(),
            0,
            "capacity grew before the charge succeeded"
        );
        assert_eq!(buf.len(), 0);
    }

    /// A budget refusal must happen BEFORE the allocator is asked, so a refused append
    /// cannot leave capacity behind. Refusal is not just "nothing written".
    #[test]
    fn a_refusal_leaves_capacity_unchanged() {
        let mut buf = Vec::new();
        let mut led = WithBytes(0);
        let before = (buf.len(), buf.capacity());
        let err = Writer::new(&mut buf, &mut led)
            .bytes(&[1u8; 4096])
            .expect_err("no budget");
        assert!(matches!(err, WriteError::Budget(_)));
        assert_eq!(
            (buf.len(), buf.capacity()),
            before,
            "a refused append reserved capacity anyway"
        );
    }

    /// Growth is charged for the delta beyond what was already admitted, not for the
    /// requested length, and not twice for the same extent.
    #[test]
    fn growth_is_charged_once_per_extent() {
        struct Spend(u64);
        impl SharedLedger for Spend {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                self.0 += bytes;
                Ok(())
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let mut buf = Vec::new();
        let mut led = Spend(0);
        {
            let mut w = Writer::new(&mut buf, &mut led);
            for _ in 0..64 {
                w.u8(0).unwrap();
            }
        }
        // Exact reservation means capacity tracks the bytes written, so the total
        // charge equals the final capacity rather than the sum of per-append requests.
        assert_eq!(led.0, buf.capacity() as u64);
        assert_eq!(buf.len(), 64);
    }

    /// The reservation/capacity table the parent asked for: every branch of `admit`,
    /// with the exact numbers written down instead of inferred.
    ///
    /// Columns: scenario, initial (len, cap), append, resulting len, whether the append
    /// must be refused, and the charge expected for that append.
    #[test]
    fn reservation_table_covers_every_branch() {
        struct Metered {
            spent: u64,
        }
        impl SharedLedger for Metered {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                self.spent += bytes;
                Ok(())
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        // (name, initial_len, capacity, append, expect_ok, expect_charge)
        let cases: [(&str, usize, usize, usize, bool, u64); 5] = [
            // fits entirely in spare capacity: nothing reserved, nothing charged
            ("fit", 8, 16, 8, true, 0),
            // exactly fills existing capacity: no growth needed
            ("exactly full", 8, 16, 8, true, 0),
            // crosses spare capacity: len8 + 9 = 17 > cap16, so one byte of growth
            ("cross spare", 8, 16, 9, true, 1),
            // no spare at all: len == cap, every appended byte is growth
            ("no spare", 8, 8, 4, true, 4),
            // same as "cross spare" but paid from prepaid credit instead of the ledger
            ("cross spare prepaid", 8, 16, 9, true, 0),
        ];

        for (name, initial_len, capacity, append, expect_ok, expect_charge) in cases {
            let mut buf = Vec::with_capacity(capacity);
            buf.extend(std::iter::repeat_n(0u8, initial_len));
            assert_eq!(buf.len(), initial_len, "{name}: setup len");
            assert_eq!(buf.capacity(), capacity, "{name}: setup cap");
            let cap_before = buf.capacity();
            let mut led = Metered { spent: 0 };
            let prepaid = name.contains("prepaid");
            let result = {
                let mut w = if prepaid {
                    Writer::prepaid(&mut buf, &mut led, append)
                } else {
                    Writer::new(&mut buf, &mut led)
                };
                w.bytes(&vec![0xA5u8; append])
            };

            assert_eq!(result.is_ok(), expect_ok, "{name}: outcome");
            if expect_ok {
                assert_eq!(buf.len(), initial_len + append, "{name}: resulting len");
                assert!(
                    &buf[initial_len..] == vec![0xA5u8; append].as_slice(),
                    "{name}: appended bytes must be present"
                );
                assert_eq!(
                    led.spent,
                    expect_charge,
                    "{name}: charge (cap {cap_before} -> {})",
                    buf.capacity()
                );
                assert!(
                    buf.capacity() >= buf.len(),
                    "{name}: capacity must cover the appended length"
                );
            }
        }
    }

    /// A failure branch: no budget, so nothing is reserved and nothing is appended.
    #[test]
    fn reservation_table_failure_branch() {
        let mut buf = Vec::with_capacity(16);
        buf.extend(std::iter::repeat_n(0u8, 8));
        let before = (buf.len(), buf.capacity());
        let mut led = WithBytes(0);
        let err = Writer::new(&mut buf, &mut led).bytes(&[1u8; 9]);
        assert!(matches!(err, Err(WriteError::Budget(_))), "got {err:?}");
        assert_eq!((buf.len(), buf.capacity()), before);
    }

    /// A non-empty caller buffer must not be re-charged and must not be over-reserved.
    ///
    /// The previous arithmetic treated the capacity delta as if `try_reserve_exact`
    /// took an absolute target, so 16 existing bytes plus one appended byte requested
    /// 33 additional and charged 17.
    #[test]
    fn a_nonempty_buffer_is_neither_recharged_nor_over_reserved() {
        struct Metered {
            spent: u64,
        }
        impl SharedLedger for Metered {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                self.spent += bytes;
                Ok(())
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        // Case A: the append FITS in spare capacity, so nothing is charged or reserved.
        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(b"0123456789abcdef"); // len 16, capacity 32
        let existing_capacity = buf.capacity();
        let mut led = Metered { spent: 0 };
        {
            let mut w = Writer::new(&mut buf, &mut led);
            w.u8(b'!').expect("one byte into spare capacity");
        }
        assert_eq!(buf.len(), 17);
        assert_eq!(
            led.spent, 0,
            "spare caller capacity must not be charged; spent {}",
            led.spent
        );
        assert_eq!(
            buf.capacity(),
            existing_capacity,
            "no reservation was needed, yet capacity moved from {existing_capacity}"
        );

        // Case B: the buffer is exactly full (len == capacity), so one appended byte
        // genuinely needs growth. Exactly that byte is charged - not 17, which is what
        // the pre-fix arithmetic did.
        let mut full = Vec::with_capacity(16);
        full.extend_from_slice(b"0123456789abcdef"); // len 16, capacity 16
        assert_eq!(full.capacity(), full.len());
        let mut led_full = Metered { spent: 0 };
        {
            let mut w = Writer::new(&mut full, &mut led_full);
            w.u8(b'!').expect("needs one byte of growth");
        }
        assert_eq!(full.len(), 17);
        assert_eq!(
            led_full.spent, 1,
            "only the single byte of growth is chargeable, got {}",
            led_full.spent
        );
        assert_eq!(full.capacity(), 17, "growth is exactly what was needed");

        // And growing past the existing capacity charges only the growth.
        let mut buf2 = Vec::with_capacity(4);
        buf2.extend_from_slice(b"abcd"); // len 4, capacity exactly 4
        let base = buf2.capacity();
        let mut led2 = Metered { spent: 0 };
        {
            let mut w = Writer::new(&mut buf2, &mut led2);
            w.bytes(&[0u8; 4]).expect("needs four more");
        }
        assert_eq!(buf2.len(), 8);
        assert_eq!(
            led2.spent,
            (buf2.capacity() - base) as u64,
            "charge must equal the growth beyond existing capacity"
        );
    }

    /// Prepaid credit also ignores pre-existing capacity: credit covers growth only.
    #[test]
    fn prepaid_credit_covers_growth_not_existing_capacity() {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(b"1234"); // len 4, capacity >= 8
        let mut led = WithBytes(0);
        {
            let mut w = Writer::prepaid(&mut buf, &mut led, 0);
            // Fits in existing capacity, so no credit is consumed.
            w.bytes(b"5678").expect("fits in existing capacity");
        }
        assert_eq!(buf.len(), 8);

        // One more byte needs new capacity, and zero credit must refuse it.
        let mut buf2 = Vec::with_capacity(8);
        buf2.extend_from_slice(b"12345678"); // len 8, capacity exactly 8
        let mut led2 = WithBytes(0);
        let err = Writer::prepaid(&mut buf2, &mut led2, 0)
            .u8(b'!')
            .expect_err("growth requires credit");
        assert!(matches!(err, WriteError::Budget(_)));
        assert_eq!(buf2.len(), 8);
    }

    /// Prepaid credit is a decreasing allowance: exhausting it refuses the next append
    /// without writing or reserving anything.
    #[test]
    fn prepaid_credit_is_finite() {
        let mut buf = Vec::new();
        let mut led = WithBytes(0);
        let err = Writer::prepaid(&mut buf, &mut led, 0)
            .u8(1)
            .expect_err("zero credit must refuse");
        assert!(matches!(err, WriteError::Budget(_)), "got {err:?}");
        assert_eq!((buf.len(), buf.capacity()), (0, 0));

        // Exactly enough credit succeeds, and one byte more is refused.
        let mut buf2 = Vec::new();
        let mut led2 = WithBytes(0);
        {
            let mut w = Writer::prepaid(&mut buf2, &mut led2, 1);
            w.u8(7).expect("one byte of credit covers one byte");
            let err = w.u8(8).expect_err("credit exhausted");
            assert!(matches!(err, WriteError::Budget(_)));
        }
        assert_eq!(buf2.len(), 1, "the refused byte was not appended");
    }

    #[test]
    fn finish_rejects_trailing_bytes() {
        let mut buf = Vec::new();
        let mut led = Open;
        Writer::new(&mut buf, &mut led).u16(1).unwrap();
        buf.push(0xFF);
        let mut r = Reader::new(&buf);
        r.u16().unwrap();
        assert_eq!(
            r.finish(),
            Err(ReadError::Data(DecodeError::Trailing { remaining: 1 }))
        );
    }
}
