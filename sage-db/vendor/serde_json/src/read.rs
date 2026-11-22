use crate::error::{Error, ErrorCode, Result};
use alloc::vec::Vec;
use core::cmp;
use core::mem;
use core::ops::Deref;
use core::str;

#[cfg(feature = "std")]
use crate::io;
#[cfg(feature = "std")]
use crate::iter::LineColIterator;

#[cfg(feature = "raw_value")]
use crate::raw::BorrowedRawDeserializer;
#[cfg(all(feature = "raw_value", feature = "std"))]
use crate::raw::OwnedRawDeserializer;
#[cfg(all(feature = "raw_value", feature = "std"))]
use alloc::string::String;
#[cfg(feature = "raw_value")]
use serde::de::Visitor;

/// Where a decoded string's escapes are accumulated.
///
/// Two implementors and **one grammar**: [`PlainScratch`] is the historical
/// `Vec<u8>` (infallible, unchanged for every default caller) and, under the
/// `admission` feature, [`AdmissionScratch`] routes every growth through the
/// caller's [`crate::admission::Admission`] policy. The parser below is written
/// once against this trait, so the two modes cannot drift apart.
pub(crate) trait ScratchSink {
    fn push_byte(&mut self, byte: u8) -> Result<()>;
    fn push_bytes(&mut self, bytes: &[u8]) -> Result<()>;
    fn is_empty(&self) -> bool;
}

impl ScratchSink for Vec<u8> {
    #[inline]
    fn push_byte(&mut self, byte: u8) -> Result<()> {
        self.push(byte);
        Ok(())
    }
    #[inline]
    fn push_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
    #[inline]
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

/// Scratch that admits every growth through the caller's policy.
///
/// Holds the buffer and the policy separately so the policy can be handed back
/// to the reader after one string is parsed: the borrow is confined to a single
/// `parse_str` call, which is what lets a seed borrow the same ledger for its
/// own work without any overlapping `&mut`.
#[cfg(feature = "admission")]
pub(crate) struct AdmissionScratch<'a, 's, 'p> {
    buf: &'s mut Vec<u8>,
    policy: &'a crate::admission::AdmissionContext<'p>,
}

#[cfg(feature = "admission")]
impl<'a, 's, 'p> AdmissionScratch<'a, 's, 'p> {
    pub(crate) fn new(
        buf: &'s mut Vec<u8>,
        policy: &'a crate::admission::AdmissionContext<'p>,
    ) -> Self {
        Self { buf, policy }
    }
}

#[cfg(feature = "admission")]
impl ScratchSink for AdmissionScratch<'_, '_, '_> {
    fn push_byte(&mut self, byte: u8) -> Result<()> {
        self.push_bytes(&[byte])
    }
    fn push_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.admit(bytes.len())?;
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(feature = "admission")]
impl AdmissionScratch<'_, '_, '_> {
    /// Checked arithmetic, then the policy, then a fallible reservation.
    /// Admit then reserve, preserving the real failure class.
    ///
    /// `additional` is the number of BYTES TO APPEND (the API argument), not the
    /// new buffer extent: the required extent is `len + additional`, checked. Only
    /// the **capacity delta** (`needed - capacity`) is charged, so slack is not
    /// re-billed, and when the capacity already fits nothing is charged and nothing
    /// is reserved.
    ///
    /// Every failure keeps its own identity in the returned `Error`:
    /// the caller's policy refusal, an extent overflow, or an allocator failure
    /// after admission. None of them is flattened to a bare `Admission` code, and no
    /// budget figure is invented for any of them.
    fn admit(&mut self, additional: usize) -> Result<()> {
        use crate::admission::AdmissionError;

        if additional == 0 {
            return Ok(());
        }
        let needed = match self.buf.len().checked_add(additional) {
            Some(n) => n,
            None => {
                return Err(Error::syntax_inline(
                    ErrorCode::Admission(AdmissionError::ExtentOverflow {
                        elements: additional,
                    }),
                    0,
                    0,
                ))
            }
        };
        let capacity = self.buf.capacity();
        if needed <= capacity {
            // Already admitted by an earlier growth: no charge, no allocation.
            return Ok(());
        }
        // Charge exactly the capacity delta this reservation will request.
        let delta = needed - capacity;
        self.policy
            .grow(delta)
            .map_err(|error| Error::syntax_inline(ErrorCode::Admission(error), 0, 0))?;
        self.buf.try_reserve_exact(additional).map_err(|_| {
            Error::syntax_inline(
                ErrorCode::Admission(AdmissionError::AllocatorFailed {
                    requested: additional,
                }),
                0,
                0,
            )
        })?;
        // The reported capacity is part of the contract: a buffer must never hold
        // capacity the caller did not admit. `needed` is the extent that was
        // admitted; anything above it is unadmitted excess and refuses the decode.
        if self.buf.capacity() > needed {
            return Err(Error::syntax_inline(
                ErrorCode::Admission(AdmissionError::UnexpectedCapacity {
                    requested: needed,
                    actual: self.buf.capacity(),
                }),
                0,
                0,
            ));
        }
        Ok(())
    }
}

/// Where a scanned JSON string lives.
///
/// `Borrowed` names a range of the input slice (zero-copy fast path);
/// `Copied` means the decoded bytes are in the scratch buffer.
#[derive(Clone, Debug)]
pub(crate) enum StrScan {
    Borrowed(core::ops::Range<usize>),
    Copied,
}

/// Trait used by the deserializer for iterating over input. This is manually
/// "specialized" for iterating over `&[u8]`. Once feature(specialization) is
/// stable we can use actual specialization.
///
/// This trait is sealed and cannot be implemented for types outside of
/// `serde_json`.
pub trait Read<'de>: private::Sealed {
    #[doc(hidden)]
    fn next(&mut self) -> Result<Option<u8>>;
    #[doc(hidden)]
    fn peek(&mut self) -> Result<Option<u8>>;

    /// Only valid after a call to peek(). Discards the peeked byte.
    #[doc(hidden)]
    fn discard(&mut self);

    /// Position of the most recent call to next().
    ///
    /// The most recent call was probably next() and not peek(), but this method
    /// should try to return a sensible result if the most recent call was
    /// actually peek() because we don't always know.
    ///
    /// Only called in case of an error, so performance is not important.
    #[doc(hidden)]
    fn position(&self) -> Position;

    /// Position of the most recent call to peek().
    ///
    /// The most recent call was probably peek() and not next(), but this method
    /// should try to return a sensible result if the most recent call was
    /// actually next() because we don't always know.
    ///
    /// Only called in case of an error, so performance is not important.
    #[doc(hidden)]
    fn peek_position(&self) -> Position;

    /// Offset from the beginning of the input to the next byte that would be
    /// returned by next() or peek().
    #[doc(hidden)]
    fn byte_offset(&self) -> usize;

    /// The admission context this reader carries, if any.
    ///
    /// Exists so the deserializer's float scratch can admit through the **same**
    /// policy without storing a borrow of its own (which would require a
    /// lifetime parameter on the public `Deserializer` type).
    #[cfg(feature = "admission")]
    #[doc(hidden)]
    fn admission_grow(
        &self,
        additional: usize,
    ) -> core::result::Result<(), crate::admission::AdmissionError>;

    /// Flush any pending progress, consulting the caller's control.
    #[cfg(feature = "admission")]
    #[doc(hidden)]
    fn admission_finish(&mut self) -> core::result::Result<(), crate::admission::AdmissionError>;

    /// Consult the caller BEFORE any byte is scanned.
    ///
    /// Distinct from `admission_finish`: an empty or malformed input must still
    /// observe a caller that already asked to stop, and that must happen before
    /// the parser touches the first byte.
    #[cfg(feature = "admission")]
    #[doc(hidden)]
    fn admission_enter(&mut self) -> core::result::Result<(), crate::admission::AdmissionError>;

    /// Assumes the previous byte was a quotation mark. Parses a JSON-escaped
    /// string until the next quotation mark using the given scratch space if
    /// necessary. The scratch space is initially empty.
    #[doc(hidden)]
    fn parse_str<'s>(&'s mut self, scratch: &'s mut Vec<u8>) -> Result<Reference<'de, 's, str>>;

    /// Assumes the previous byte was a quotation mark. Parses a JSON-escaped
    /// string until the next quotation mark using the given scratch space if
    /// necessary. The scratch space is initially empty.
    ///
    /// This function returns the raw bytes in the string with escape sequences
    /// expanded but without performing unicode validation.
    #[doc(hidden)]
    fn parse_str_raw<'s>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
    ) -> Result<Reference<'de, 's, [u8]>>;

    /// Assumes the previous byte was a quotation mark. Parses a JSON-escaped
    /// string until the next quotation mark but discards the data.
    #[doc(hidden)]
    fn ignore_str(&mut self) -> Result<()>;

    /// Assumes the previous byte was a hex escape sequence ('\u') in a string.
    /// Parses next hexadecimal sequence.
    #[doc(hidden)]
    fn decode_hex_escape(&mut self) -> Result<u16>;

    /// Switch raw buffering mode on.
    ///
    /// This is used when deserializing `RawValue`.
    #[cfg(feature = "raw_value")]
    #[doc(hidden)]
    fn begin_raw_buffering(&mut self);

    /// Switch raw buffering mode off and provides the raw buffered data to the
    /// given visitor.
    #[cfg(feature = "raw_value")]
    #[doc(hidden)]
    fn end_raw_buffering<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>;

    /// Whether StreamDeserializer::next needs to check the failed flag. True
    /// for IoRead, false for StrRead and SliceRead which can track failure by
    /// truncating their input slice to avoid the extra check on every next
    /// call.
    #[doc(hidden)]
    const should_early_return_if_failed: bool;

    /// Mark a persistent failure of StreamDeserializer, either by setting the
    /// flag or by truncating the input data.
    #[doc(hidden)]
    fn set_failed(&mut self, failed: &mut bool);
}

pub struct Position {
    pub line: usize,
    pub column: usize,
}

pub enum Reference<'b, 'c, T>
where
    T: ?Sized + 'static,
{
    Borrowed(&'b T),
    Copied(&'c T),
}

impl<'b, 'c, T> Deref for Reference<'b, 'c, T>
where
    T: ?Sized + 'static,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match *self {
            Reference::Borrowed(b) => b,
            Reference::Copied(c) => c,
        }
    }
}

/// JSON input source that reads from a std::io input stream.
#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub struct IoRead<R>
where
    R: io::Read,
{
    iter: LineColIterator<io::Bytes<R>>,
    /// Temporary storage of peeked byte.
    ch: Option<u8>,
    #[cfg(feature = "raw_value")]
    raw_buffer: Option<Vec<u8>>,
    #[cfg(feature = "raw_value")]
    raw_buffering_start_position: Position,
}

/// JSON input source that reads from a slice of bytes.
//
// This is more efficient than other iterators because peek() can be read-only
// and we can compute line/col position only if an error happens.
pub struct SliceRead<'a> {
    slice: &'a [u8],
    /// Index of the *next* byte that will be returned by next() or peek().
    index: usize,
    /// Admission policy for the string scratch buffer.
    ///
    /// `None` on every default path: the ordinary `Vec<u8>` scratch is used and no
    /// behaviour changes. `Some` only for a deserializer built by
    /// `from_slice_with_admission`, where scratch growth is admitted first and
    /// errors are constructed inline.
    #[cfg(feature = "admission")]
    admission: Option<&'a crate::admission::AdmissionContext<'a>>,
    /// Bounded progress accounting (admission mode only).
    #[cfg(feature = "admission")]
    progress: Option<crate::admission::Progress<'a, 'a>>,
    #[cfg(feature = "raw_value")]
    raw_buffering_start_index: usize,
}

/// JSON input source that reads from a UTF-8 string.
//
// Able to elide UTF-8 checks by assuming that the input is valid UTF-8.
pub struct StrRead<'a> {
    delegate: SliceRead<'a>,
    #[cfg(feature = "raw_value")]
    data: &'a str,
}

// Prevent users from implementing the Read trait.
mod private {
    pub trait Sealed {}
}

//////////////////////////////////////////////////////////////////////////////

#[cfg(feature = "std")]
impl<R> IoRead<R>
where
    R: io::Read,
{
    /// Create a JSON input source to read from a std::io input stream.
    ///
    /// When reading from a source against which short reads are not efficient, such
    /// as a [`File`], you will want to apply your own buffering because serde_json
    /// will not buffer the input. See [`std::io::BufReader`].
    ///
    /// [`File`]: std::fs::File
    pub fn new(reader: R) -> Self {
        IoRead {
            iter: LineColIterator::new(reader.bytes()),
            ch: None,
            #[cfg(feature = "raw_value")]
            raw_buffer: None,
            #[cfg(feature = "raw_value")]
            raw_buffering_start_position: Position { line: 1, column: 0 },
        }
    }
}

#[cfg(feature = "std")]
impl<R> private::Sealed for IoRead<R> where R: io::Read {}

#[cfg(feature = "std")]
impl<R> IoRead<R>
where
    R: io::Read,
{
    fn parse_str_bytes<'s, T, F>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
        validate: bool,
        result: F,
    ) -> Result<T>
    where
        T: 's,
        F: FnOnce(&'s Self, &'s [u8]) -> Result<T>,
    {
        loop {
            let ch = tri!(next_or_eof(self));
            if !is_escape(ch, true) {
                scratch.push(ch);
                continue;
            }
            match ch {
                b'"' => {
                    return result(self, scratch);
                }
                b'\\' => {
                    tri!(parse_escape(self, validate, scratch));
                }
                _ => {
                    if validate {
                        return error(self, ErrorCode::ControlCharacterWhileParsingString);
                    }
                    scratch.push(ch);
                }
            }
        }
    }
}

#[cfg(feature = "std")]
impl<'de, R> Read<'de> for IoRead<R>
where
    R: io::Read,
{
    #[cfg(feature = "admission")]
    fn admission_grow(
        &self,
        _additional: usize,
    ) -> core::result::Result<(), crate::admission::AdmissionError> {
        Ok(())
    }

    #[cfg(feature = "admission")]
    fn admission_finish(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        Ok(())
    }

    #[cfg(feature = "admission")]
    fn admission_enter(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        Ok(())
    }

    #[inline]
    fn next(&mut self) -> Result<Option<u8>> {
        match self.ch.take() {
            Some(ch) => {
                #[cfg(feature = "raw_value")]
                {
                    if let Some(buf) = &mut self.raw_buffer {
                        buf.push(ch);
                    }
                }
                Ok(Some(ch))
            }
            None => match self.iter.next() {
                Some(Err(err)) => Err(Error::io(err)),
                Some(Ok(ch)) => {
                    #[cfg(feature = "raw_value")]
                    {
                        if let Some(buf) = &mut self.raw_buffer {
                            buf.push(ch);
                        }
                    }
                    Ok(Some(ch))
                }
                None => Ok(None),
            },
        }
    }

    #[inline]
    fn peek(&mut self) -> Result<Option<u8>> {
        match self.ch {
            Some(ch) => Ok(Some(ch)),
            None => match self.iter.next() {
                Some(Err(err)) => Err(Error::io(err)),
                Some(Ok(ch)) => {
                    self.ch = Some(ch);
                    Ok(self.ch)
                }
                None => Ok(None),
            },
        }
    }

    #[cfg(not(feature = "raw_value"))]
    #[inline]
    fn discard(&mut self) {
        self.ch = None;
    }

    #[cfg(feature = "raw_value")]
    fn discard(&mut self) {
        if let Some(ch) = self.ch.take() {
            if let Some(buf) = &mut self.raw_buffer {
                buf.push(ch);
            }
        }
    }

    fn position(&self) -> Position {
        Position {
            line: self.iter.line(),
            column: self.iter.col(),
        }
    }

    fn peek_position(&self) -> Position {
        // The LineColIterator updates its position during peek() so it has the
        // right one here.
        self.position()
    }

    fn byte_offset(&self) -> usize {
        match self.ch {
            Some(_) => self.iter.byte_offset() - 1,
            None => self.iter.byte_offset(),
        }
    }

    fn parse_str<'s>(&'s mut self, scratch: &'s mut Vec<u8>) -> Result<Reference<'de, 's, str>> {
        self.parse_str_bytes(scratch, true, as_str)
            .map(Reference::Copied)
    }

    fn parse_str_raw<'s>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
    ) -> Result<Reference<'de, 's, [u8]>> {
        self.parse_str_bytes(scratch, false, |_, bytes| Ok(bytes))
            .map(Reference::Copied)
    }

    fn ignore_str(&mut self) -> Result<()> {
        loop {
            let ch = tri!(next_or_eof(self));
            if !is_escape(ch, true) {
                continue;
            }
            match ch {
                b'"' => {
                    return Ok(());
                }
                b'\\' => {
                    tri!(ignore_escape(self));
                }
                _ => {
                    return error(self, ErrorCode::ControlCharacterWhileParsingString);
                }
            }
        }
    }

    fn decode_hex_escape(&mut self) -> Result<u16> {
        let a = tri!(next_or_eof(self));
        let b = tri!(next_or_eof(self));
        let c = tri!(next_or_eof(self));
        let d = tri!(next_or_eof(self));
        match decode_four_hex_digits(a, b, c, d) {
            Some(val) => Ok(val),
            None => error(self, ErrorCode::InvalidEscape),
        }
    }

    #[cfg(feature = "raw_value")]
    fn begin_raw_buffering(&mut self) {
        let position = self.position();
        self.raw_buffering_start_position = Position {
            line: position.line,
            column: position.column - usize::from(self.ch.is_some()),
        };
        self.raw_buffer = Some(Vec::new());
    }

    #[cfg(feature = "raw_value")]
    fn end_raw_buffering<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        let raw = self.raw_buffer.take().unwrap();
        let raw = match String::from_utf8(raw) {
            Ok(raw) => raw,
            Err(err) => {
                let prefix = &err.as_bytes()[..=err.utf8_error().valid_up_to()];
                let line;
                let column;
                if let Some(last_newline) = memchr::memrchr(b'\n', prefix) {
                    line = self.raw_buffering_start_position.line
                        + memchr::memchr_iter(b'\n', &prefix[..last_newline]).count()
                        + 1;
                    column = prefix.len() - last_newline - 1;
                } else {
                    line = self.raw_buffering_start_position.line;
                    column = self.raw_buffering_start_position.column + prefix.len();
                }
                return Err(Error::syntax(
                    ErrorCode::InvalidUnicodeCodePoint,
                    line,
                    column,
                ));
            }
        };
        visitor.visit_map(OwnedRawDeserializer {
            raw_value: Some(raw),
        })
    }

    const should_early_return_if_failed: bool = true;

    #[inline]
    #[cold]
    fn set_failed(&mut self, failed: &mut bool) {
        *failed = true;
    }
}

//////////////////////////////////////////////////////////////////////////////

impl<'a> SliceRead<'a> {
    /// Create a JSON input source to read from a slice of bytes.
    pub fn new(slice: &'a [u8]) -> Self {
        SliceRead {
            slice,
            index: 0,
            #[cfg(feature = "admission")]
            admission: None,
            #[cfg(feature = "admission")]
            progress: None,
            #[cfg(feature = "raw_value")]
            raw_buffering_start_index: 0,
        }
    }

    /// Reader that admits its scratch through `admission`.
    #[cfg(feature = "admission")]
    pub(crate) fn new_with_admission(
        slice: &'a [u8],
        admission: &'a crate::admission::AdmissionContext<'a>,
    ) -> Self {
        SliceRead {
            slice,
            index: 0,
            admission: Some(admission),
            progress: Some(crate::admission::Progress::new(admission)),
            #[cfg(feature = "raw_value")]
            raw_buffering_start_index: 0,
        }
    }

    /// Account `consumed` bytes of lexical progress.
    ///
    /// A no-op when no policy is attached (default builds), so the ordinary path
    /// is unchanged.
    #[cfg(feature = "admission")]
    #[inline]
    fn progressed(&mut self, consumed: usize) -> Result<()> {
        match self.progress.as_mut() {
            Some(progress) => progress
                .advance(consumed)
                .map_err(|error| Error::syntax_inline(ErrorCode::Admission(error), 0, 0)),
            None => Ok(()),
        }
    }

    fn position_of_index(&self, i: usize) -> Position {
        let start_of_line = match memchr::memrchr(b'\n', &self.slice[..i]) {
            Some(position) => position + 1,
            None => 0,
        };
        Position {
            line: 1 + memchr::memchr_iter(b'\n', &self.slice[..start_of_line]).count(),
            column: i - start_of_line,
        }
    }

    /// Admission mode uses the SAME escape predicate as upstream, attending each
    /// actual byte. Ordinary mode retains upstream's bulk/SWAR implementation.
    fn skip_to_escape(&mut self, forbid_control_characters: bool) -> Result<()> {
        #[cfg(feature = "admission")]
        if self.progress.is_some() {
            self.admission_enter()
                .map_err(|e| Error::syntax_inline(ErrorCode::Admission(e), 0, 0))?;
            while self.index < self.slice.len()
                && !is_escape(self.slice[self.index], forbid_control_characters)
            {
                self.index += 1;
                self.progressed(1)?;
            }
            return Ok(());
        }
        self.skip_to_escape_upstream(forbid_control_characters);
        Ok(())
    }

    fn skip_to_escape_upstream(&mut self, forbid_control_characters: bool) {
        // Immediately bail-out on empty strings and consecutive escapes (e.g. \u041b\u0435)
        if self.index == self.slice.len()
            || is_escape(self.slice[self.index], forbid_control_characters)
        {
            return;
        }
        self.index += 1;

        let rest = &self.slice[self.index..];

        if !forbid_control_characters {
            self.index += memchr::memchr2(b'"', b'\\', rest).unwrap_or(rest.len());
            return;
        }

        // We wish to find the first byte in range 0x00..=0x1F or " or \. Ideally, we'd use
        // something akin to memchr3, but the memchr crate does not support this at the moment.
        // Therefore, we use a variation on Mycroft's algorithm [1] to provide performance better
        // than a naive loop. It runs faster than equivalent two-pass memchr2+SWAR code on
        // benchmarks and it's cross-platform, so probably the right fit.
        // [1]: https://groups.google.com/forum/#!original/comp.lang.c/2HtQXvg7iKc/xOJeipH6KLMJ

        #[cfg(fast_arithmetic = "64")]
        type Chunk = u64;
        #[cfg(fast_arithmetic = "32")]
        type Chunk = u32;

        const STEP: usize = mem::size_of::<Chunk>();
        const ONE_BYTES: Chunk = Chunk::MAX / 255; // 0x0101...01

        for chunk in rest.chunks_exact(STEP) {
            let chars = Chunk::from_le_bytes(chunk.try_into().unwrap());
            let contains_ctrl = chars.wrapping_sub(ONE_BYTES * 0x20) & !chars;
            let chars_quote = chars ^ (ONE_BYTES * Chunk::from(b'"'));
            let contains_quote = chars_quote.wrapping_sub(ONE_BYTES) & !chars_quote;
            let chars_backslash = chars ^ (ONE_BYTES * Chunk::from(b'\\'));
            let contains_backslash = chars_backslash.wrapping_sub(ONE_BYTES) & !chars_backslash;
            let masked = (contains_ctrl | contains_quote | contains_backslash) & (ONE_BYTES << 7);
            if masked != 0 {
                // SAFETY: chunk is in-bounds for slice
                self.index = unsafe { chunk.as_ptr().offset_from(self.slice.as_ptr()) } as usize
                    + masked.trailing_zeros() as usize / 8;
                return;
            }
        }

        self.index += rest.len() / STEP * STEP;
        self.skip_to_escape_slow();
    }

    #[cold]
    #[inline(never)]
    fn skip_to_escape_slow(&mut self) {
        while self.index < self.slice.len() && !is_escape(self.slice[self.index], true) {
            self.index += 1;
        }
    }

    /// Scan one JSON string, appending decoded bytes to `scratch`.
    ///
    /// Returns [`StrScan`], which names either a slice of the **input** (the
    /// escaped-free fast path) or the populated scratch. Expressing the result as
    /// a range rather than a borrow is what lets the caller pass a scratch
    /// adapter that borrows a policy, without tying that policy's lifetime to the
    /// returned string.
    fn scan_str_bytes<S: ScratchSink>(
        &mut self,
        scratch: &mut S,
        validate: bool,
    ) -> Result<StrScan> {
        // Index of the first byte not yet copied into the scratch space.
        let mut start = self.index;

        loop {
            tri!(self.skip_to_escape(validate));
            if self.index == self.slice.len() {
                return error(self, ErrorCode::EofWhileParsingString);
            }
            match self.slice[self.index] {
                b'"' => {
                    if scratch.is_empty() {
                        let borrowed = start..self.index;
                        self.index += 1;
                        // The closing quote is a consumed byte; count it so the
                        // cadence reflects real input, not only scanned runs.
                        #[cfg(feature = "admission")]
                        self.progressed(1)?;
                        return Ok(StrScan::Borrowed(borrowed));
                    } else {
                        tri!(scratch.push_bytes(&self.slice[start..self.index]));
                        self.index += 1;
                        #[cfg(feature = "admission")]
                        self.progressed(1)?;
                        return Ok(StrScan::Copied);
                    }
                }
                b'\\' => {
                    tri!(scratch.push_bytes(&self.slice[start..self.index]));
                    self.index += 1;
                    // The backslash itself is consumed here; the escape body is
                    // counted by `parse_escape` -> `decode_hex_escape`.
                    #[cfg(feature = "admission")]
                    self.progressed(1)?;
                    tri!(parse_escape(self, validate, scratch));
                    start = self.index;
                }
                _ => {
                    self.index += 1;
                    #[cfg(feature = "admission")]
                    self.progressed(1)?;
                    return error(self, ErrorCode::ControlCharacterWhileParsingString);
                }
            }
        }
    }
}

impl<'a> private::Sealed for SliceRead<'a> {}

impl<'a> Read<'a> for SliceRead<'a> {
    #[inline]
    fn next(&mut self) -> Result<Option<u8>> {
        #[cfg(feature = "admission")]
        self.admission_enter()
            .map_err(|e| Error::syntax_inline(ErrorCode::Admission(e), 0, 0))?;
        // `Ok(self.slice.get(self.index).map(|ch| { self.index += 1; *ch }))`
        // is about 10% slower.
        Ok(if self.index < self.slice.len() {
            let ch = self.slice[self.index];
            self.index += 1;
            #[cfg(feature = "admission")]
            self.progressed(1)?;
            Some(ch)
        } else {
            None
        })
    }

    #[inline]
    fn peek(&mut self) -> Result<Option<u8>> {
        #[cfg(feature = "admission")]
        self.admission_enter()
            .map_err(|e| Error::syntax_inline(ErrorCode::Admission(e), 0, 0))?;
        // `Ok(self.slice.get(self.index).map(|ch| *ch))` is about 10% slower
        // for some reason.
        Ok(if self.index < self.slice.len() {
            Some(self.slice[self.index])
        } else {
            None
        })
    }

    #[inline]
    fn discard(&mut self) {
        self.index += 1;
        #[cfg(feature = "admission")]
        {
            // `discard` returns `()` and cannot report. The refusal is LATCHED and
            // surfaced at the next fallible progress site, so a one-shot policy
            // refusal is not lost.
            if let Some(progress) = self.progress.as_mut() {
                progress.advance_or_latch(1);
            }
        }
    }

    fn position(&self) -> Position {
        self.position_of_index(self.index)
    }

    fn peek_position(&self) -> Position {
        // Cap it at slice.len() just in case the most recent call was next()
        // and it returned the last byte.
        self.position_of_index(cmp::min(self.slice.len(), self.index + 1))
    }

    fn byte_offset(&self) -> usize {
        self.index
    }

    #[cfg(feature = "admission")]
    fn admission_grow(
        &self,
        additional: usize,
    ) -> core::result::Result<(), crate::admission::AdmissionError> {
        match self.admission {
            Some(context) => context.grow(additional),
            None => Ok(()),
        }
    }

    #[cfg(feature = "admission")]
    fn admission_finish(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        match self.progress.as_mut() {
            Some(progress) => progress.finish(),
            None => Ok(()),
        }
    }

    #[cfg(feature = "admission")]
    fn admission_enter(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        match self.progress.as_mut() {
            Some(progress) => progress.enter(),
            None => Ok(()),
        }
    }

    fn parse_str<'s>(&'s mut self, scratch: &'s mut Vec<u8>) -> Result<Reference<'a, 's, str>> {
        #[cfg(feature = "admission")]
        {
            // Take the context reference by copying it out FIRST, so the sink is
            // not tied to `&mut self`. `AdmissionContext::grow` needs only `&self`.
            let policy = self.admission;
            if let Some(policy) = policy {
                let scan = {
                    let mut sink = AdmissionScratch::new(scratch, policy);
                    self.scan_str_bytes(&mut sink, true)
                }?;
                return match scan {
                    StrScan::Borrowed(range) => {
                        as_str(self, &self.slice[range]).map(Reference::Borrowed)
                    }
                    StrScan::Copied => as_str(self, scratch).map(Reference::Copied),
                };
            }
        }
        let scan = self.scan_str_bytes(scratch, true)?;
        match scan {
            StrScan::Borrowed(range) => as_str(self, &self.slice[range]).map(Reference::Borrowed),
            StrScan::Copied => as_str(self, scratch).map(Reference::Copied),
        }
    }

    fn parse_str_raw<'s>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
    ) -> Result<Reference<'a, 's, [u8]>> {
        // Same admission sink as the string path: a byte-string decode must not
        // grow an unadmitted `Vec` just because it takes the raw variant. One
        // grammar, one scratch policy.
        #[cfg(feature = "admission")]
        {
            let policy = self.admission;
            if let Some(policy) = policy {
                let scan = {
                    let mut sink = AdmissionScratch::new(scratch, policy);
                    self.scan_str_bytes(&mut sink, false)
                }?;
                return Ok(match scan {
                    StrScan::Borrowed(range) => Reference::Borrowed(&self.slice[range]),
                    StrScan::Copied => Reference::Copied(scratch),
                });
            }
        }
        let scan = self.scan_str_bytes(scratch, false)?;
        Ok(match scan {
            StrScan::Borrowed(range) => Reference::Borrowed(&self.slice[range]),
            StrScan::Copied => Reference::Copied(scratch),
        })
    }

    fn ignore_str(&mut self) -> Result<()> {
        loop {
            tri!(self.skip_to_escape(true));
            if self.index == self.slice.len() {
                return error(self, ErrorCode::EofWhileParsingString);
            }
            match self.slice[self.index] {
                b'"' => {
                    self.index += 1;
                    #[cfg(feature = "admission")]
                    self.progressed(1)?;
                    return Ok(());
                }
                b'\\' => {
                    self.index += 1;
                    #[cfg(feature = "admission")]
                    self.progressed(1)?;
                    tri!(ignore_escape(self));
                }
                _ => {
                    return error(self, ErrorCode::ControlCharacterWhileParsingString);
                }
            }
        }
    }

    #[inline]
    fn decode_hex_escape(&mut self) -> Result<u16> {
        match self.slice[self.index..] {
            [a, b, c, d, ..] => {
                #[cfg(not(feature = "admission"))]
                {
                    self.index += 4;
                }
                #[cfg(feature = "admission")]
                for _ in 0..4 {
                    self.index += 1;
                    self.progressed(1)?;
                }
                match decode_four_hex_digits(a, b, c, d) {
                    Some(val) => Ok(val),
                    None => error(self, ErrorCode::InvalidEscape),
                }
            }
            _ => {
                #[cfg(not(feature = "admission"))]
                {
                    self.index = self.slice.len();
                }
                #[cfg(feature = "admission")]
                while self.index < self.slice.len() {
                    self.index += 1;
                    self.progressed(1)?;
                }
                error(self, ErrorCode::EofWhileParsingString)
            }
        }
    }

    #[cfg(feature = "raw_value")]
    fn begin_raw_buffering(&mut self) {
        self.raw_buffering_start_index = self.index;
    }

    #[cfg(feature = "raw_value")]
    fn end_raw_buffering<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'a>,
    {
        let raw = &self.slice[self.raw_buffering_start_index..self.index];
        let raw = match str::from_utf8(raw) {
            Ok(raw) => raw,
            Err(err) => {
                let at = self.raw_buffering_start_index + err.valid_up_to() + 1;
                let position = self.position_of_index(at);
                return Err(Error::syntax(
                    ErrorCode::InvalidUnicodeCodePoint,
                    position.line,
                    position.column,
                ));
            }
        };
        visitor.visit_map(BorrowedRawDeserializer {
            raw_value: Some(raw),
        })
    }

    const should_early_return_if_failed: bool = false;

    #[inline]
    #[cold]
    fn set_failed(&mut self, _failed: &mut bool) {
        self.slice = &self.slice[..self.index];
    }
}

//////////////////////////////////////////////////////////////////////////////

impl<'a> StrRead<'a> {
    /// Create a JSON input source to read from a UTF-8 string.
    pub fn new(s: &'a str) -> Self {
        StrRead {
            delegate: SliceRead::new(s.as_bytes()),
            #[cfg(feature = "raw_value")]
            data: s,
        }
    }
}

impl<'a> private::Sealed for StrRead<'a> {}

impl<'a> Read<'a> for StrRead<'a> {
    #[inline]
    fn next(&mut self) -> Result<Option<u8>> {
        self.delegate.next()
    }

    #[inline]
    fn peek(&mut self) -> Result<Option<u8>> {
        self.delegate.peek()
    }

    #[inline]
    fn discard(&mut self) {
        self.delegate.discard();
    }

    fn position(&self) -> Position {
        self.delegate.position()
    }

    fn peek_position(&self) -> Position {
        self.delegate.peek_position()
    }

    fn byte_offset(&self) -> usize {
        self.delegate.byte_offset()
    }

    #[cfg(feature = "admission")]
    fn admission_grow(
        &self,
        additional: usize,
    ) -> core::result::Result<(), crate::admission::AdmissionError> {
        self.delegate.admission_grow(additional)
    }

    #[cfg(feature = "admission")]
    fn admission_finish(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        self.delegate.admission_finish()
    }

    #[cfg(feature = "admission")]
    fn admission_enter(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        self.delegate.admission_enter()
    }

    fn parse_str<'s>(&'s mut self, scratch: &'s mut Vec<u8>) -> Result<Reference<'a, 's, str>> {
        // The deserialization input came in as &str with a UTF-8 guarantee, and
        // the \u-escapes are checked along the way, so no re-validation is needed.
        let scan = self.delegate.scan_str_bytes(scratch, true)?;
        Ok(match scan {
            StrScan::Borrowed(range) => Reference::Borrowed(unsafe {
                str::from_utf8_unchecked(&self.delegate.slice[range])
            }),
            StrScan::Copied => Reference::Copied(unsafe { str::from_utf8_unchecked(scratch) }),
        })
    }

    fn parse_str_raw<'s>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
    ) -> Result<Reference<'a, 's, [u8]>> {
        self.delegate.parse_str_raw(scratch)
    }

    fn ignore_str(&mut self) -> Result<()> {
        self.delegate.ignore_str()
    }

    fn decode_hex_escape(&mut self) -> Result<u16> {
        self.delegate.decode_hex_escape()
    }

    #[cfg(feature = "raw_value")]
    fn begin_raw_buffering(&mut self) {
        self.delegate.begin_raw_buffering();
    }

    #[cfg(feature = "raw_value")]
    fn end_raw_buffering<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'a>,
    {
        let raw = &self.data[self.delegate.raw_buffering_start_index..self.delegate.index];
        visitor.visit_map(BorrowedRawDeserializer {
            raw_value: Some(raw),
        })
    }

    const should_early_return_if_failed: bool = false;

    #[inline]
    #[cold]
    fn set_failed(&mut self, failed: &mut bool) {
        self.delegate.set_failed(failed);
    }
}

//////////////////////////////////////////////////////////////////////////////

impl<'de, R> private::Sealed for &mut R where R: Read<'de> {}

impl<'de, R> Read<'de> for &mut R
where
    R: Read<'de>,
{
    #[cfg(feature = "admission")]
    fn admission_grow(
        &self,
        additional: usize,
    ) -> core::result::Result<(), crate::admission::AdmissionError> {
        (**self).admission_grow(additional)
    }

    #[cfg(feature = "admission")]
    fn admission_finish(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        (**self).admission_finish()
    }

    #[cfg(feature = "admission")]
    fn admission_enter(&mut self) -> core::result::Result<(), crate::admission::AdmissionError> {
        (**self).admission_enter()
    }

    fn next(&mut self) -> Result<Option<u8>> {
        R::next(self)
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        R::peek(self)
    }

    fn discard(&mut self) {
        R::discard(self);
    }

    fn position(&self) -> Position {
        R::position(self)
    }

    fn peek_position(&self) -> Position {
        R::peek_position(self)
    }

    fn byte_offset(&self) -> usize {
        R::byte_offset(self)
    }

    fn parse_str<'s>(&'s mut self, scratch: &'s mut Vec<u8>) -> Result<Reference<'de, 's, str>> {
        R::parse_str(self, scratch)
    }

    fn parse_str_raw<'s>(
        &'s mut self,
        scratch: &'s mut Vec<u8>,
    ) -> Result<Reference<'de, 's, [u8]>> {
        R::parse_str_raw(self, scratch)
    }

    fn ignore_str(&mut self) -> Result<()> {
        R::ignore_str(self)
    }

    fn decode_hex_escape(&mut self) -> Result<u16> {
        R::decode_hex_escape(self)
    }

    #[cfg(feature = "raw_value")]
    fn begin_raw_buffering(&mut self) {
        R::begin_raw_buffering(self);
    }

    #[cfg(feature = "raw_value")]
    fn end_raw_buffering<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        R::end_raw_buffering(self, visitor)
    }

    const should_early_return_if_failed: bool = R::should_early_return_if_failed;

    fn set_failed(&mut self, failed: &mut bool) {
        R::set_failed(self, failed);
    }
}

//////////////////////////////////////////////////////////////////////////////

/// Marker for whether StreamDeserializer can implement FusedIterator.
pub trait Fused: private::Sealed {}
impl<'a> Fused for SliceRead<'a> {}
impl<'a> Fused for StrRead<'a> {}

fn is_escape(ch: u8, including_control_characters: bool) -> bool {
    ch == b'"' || ch == b'\\' || (including_control_characters && ch < 0x20)
}

fn next_or_eof<'de, R>(read: &mut R) -> Result<u8>
where
    R: ?Sized + Read<'de>,
{
    match tri!(read.next()) {
        Some(b) => Ok(b),
        None => error(read, ErrorCode::EofWhileParsingString),
    }
}

fn peek_or_eof<'de, R>(read: &mut R) -> Result<u8>
where
    R: ?Sized + Read<'de>,
{
    match tri!(read.peek()) {
        Some(b) => Ok(b),
        None => error(read, ErrorCode::EofWhileParsingString),
    }
}

fn error<'de, R, T>(read: &R, reason: ErrorCode) -> Result<T>
where
    R: ?Sized + Read<'de>,
{
    let position = read.position();
    Err(Error::syntax(reason, position.line, position.column))
}

fn as_str<'de, 's, R: Read<'de>>(read: &R, slice: &'s [u8]) -> Result<&'s str> {
    match str::from_utf8(slice) {
        Ok(s) => Ok(s),
        Err(err) => {
            // Point at the first invalid UTF-8 byte. `read` is positioned at
            // the closing quote of the string whose content is `slice`. If the
            // JSON string contained escape sequences, the column position is
            // approximate.
            let position = read.position();
            Err(Error::syntax(
                ErrorCode::InvalidUnicodeCodePoint,
                position.line,
                position
                    .column
                    .saturating_sub(slice.len() - err.valid_up_to()),
            ))
        }
    }
}

/// Parses a JSON escape sequence and appends it into the scratch space. Assumes
/// the previous byte read was a backslash.
fn parse_escape<'de, R: Read<'de>, S: ScratchSink>(
    read: &mut R,
    validate: bool,
    scratch: &mut S,
) -> Result<()> {
    let ch = tri!(next_or_eof(read));

    match ch {
        b'"' => scratch.push_byte(b'"'),
        b'\\' => scratch.push_byte(b'\\'),
        b'/' => scratch.push_byte(b'/'),
        b'b' => scratch.push_byte(b'\x08'),
        b'f' => scratch.push_byte(b'\x0c'),
        b'n' => scratch.push_byte(b'\n'),
        b'r' => scratch.push_byte(b'\r'),
        b't' => scratch.push_byte(b'\t'),
        b'u' => return parse_unicode_escape(read, validate, scratch),
        _ => return error(read, ErrorCode::InvalidEscape),
    }
}

/// Parses a JSON \u escape and appends it into the scratch space. Assumes `\u`
/// has just been read.
#[cold]
fn parse_unicode_escape<'de, R: Read<'de>, S: ScratchSink>(
    read: &mut R,
    validate: bool,
    scratch: &mut S,
) -> Result<()> {
    let mut n = tri!(read.decode_hex_escape());

    // Non-BMP characters are encoded as a sequence of two hex escapes,
    // representing UTF-16 surrogates. If deserializing a utf-8 string the
    // surrogates are required to be paired, whereas deserializing a byte string
    // accepts lone surrogates.
    if validate && n >= 0xDC00 && n <= 0xDFFF {
        // XXX: This is actually a trailing surrogate.
        return error(read, ErrorCode::LoneLeadingSurrogateInHexEscape);
    }

    loop {
        if n < 0xD800 || n > 0xDBFF {
            // Every u16 outside of the surrogate ranges is guaranteed to be a
            // legal char.
            tri!(push_wtf8_codepoint(n as u32, scratch));
            return Ok(());
        }

        // n is a leading surrogate, we now expect a trailing surrogate.
        let n1 = n;

        if tri!(peek_or_eof(read)) == b'\\' {
            read.discard();
        } else {
            return if validate {
                read.discard();
                error(read, ErrorCode::UnexpectedEndOfHexEscape)
            } else {
                tri!(push_wtf8_codepoint(n1 as u32, scratch));
                Ok(())
            };
        }

        if tri!(peek_or_eof(read)) == b'u' {
            read.discard();
        } else {
            return if validate {
                read.discard();
                error(read, ErrorCode::UnexpectedEndOfHexEscape)
            } else {
                tri!(push_wtf8_codepoint(n1 as u32, scratch));
                // The \ prior to this byte started an escape sequence, so we
                // need to parse that now. This recursive call does not blow the
                // stack on malicious input because the escape is not \u, so it
                // will be handled by one of the easy nonrecursive cases.
                parse_escape(read, validate, scratch)
            };
        }

        let n2 = tri!(read.decode_hex_escape());

        if n2 < 0xDC00 || n2 > 0xDFFF {
            if validate {
                return error(read, ErrorCode::LoneLeadingSurrogateInHexEscape);
            }
            tri!(push_wtf8_codepoint(n1 as u32, scratch));
            // If n2 is a leading surrogate, we need to restart.
            n = n2;
            continue;
        }

        // This value is in range U+10000..=U+10FFFF, which is always a valid
        // codepoint.
        let n = ((((n1 - 0xD800) as u32) << 10) | (n2 - 0xDC00) as u32) + 0x1_0000;
        tri!(push_wtf8_codepoint(n, scratch));
        return Ok(());
    }
}

/// Adds a WTF-8 codepoint to the end of the buffer. This is a more efficient
/// implementation of String::push. The codepoint may be a surrogate.
#[inline]
fn push_wtf8_codepoint<S: ScratchSink>(n: u32, scratch: &mut S) -> Result<()> {
    if n < 0x80 {
        return scratch.push_byte(n as u8);
    }

    // Encode into a fixed stack buffer first, then append. This replaces the
    // historical `reserve` + raw-pointer write with safe indexing: the length is
    // known before any growth, so no `set_len` and no `unsafe` is needed, and the
    // produced bytes are identical for every accepted code point.
    let mut buf = [0u8; 4];
    let encoded_len = match n {
        0..=0x7F => unreachable!(),
        0x80..=0x7FF => {
            buf[0] = ((n >> 6) & 0b0001_1111) as u8 | 0b1100_0000;
            2
        }
        0x800..=0xFFFF => {
            buf[0] = ((n >> 12) & 0b0000_1111) as u8 | 0b1110_0000;
            buf[1] = ((n >> 6) & 0b0011_1111) as u8 | 0b1000_0000;
            3
        }
        0x1_0000..=0x10_FFFF => {
            buf[0] = ((n >> 18) & 0b0000_0111) as u8 | 0b1111_0000;
            buf[1] = ((n >> 12) & 0b0011_1111) as u8 | 0b1000_0000;
            buf[2] = ((n >> 6) & 0b0011_1111) as u8 | 0b1000_0000;
            4
        }
        0x11_0000.. => unreachable!(),
    };
    buf[encoded_len - 1] = (n & 0b0011_1111) as u8 | 0b1000_0000;
    scratch.push_bytes(&buf[..encoded_len])
}

/// Parses a JSON escape sequence and discards the value. Assumes the previous
/// byte read was a backslash.
fn ignore_escape<'de, R>(read: &mut R) -> Result<()>
where
    R: ?Sized + Read<'de>,
{
    let ch = tri!(next_or_eof(read));

    match ch {
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
        b'u' => {
            // At this point we don't care if the codepoint is valid. We just
            // want to consume it. We don't actually know what is valid or not
            // at this point, because that depends on if this string will
            // ultimately be parsed into a string or a byte buffer in the "real"
            // parse.

            tri!(read.decode_hex_escape());
        }
        _ => {
            return error(read, ErrorCode::InvalidEscape);
        }
    }

    Ok(())
}

const fn decode_hex_val_slow(val: u8) -> Option<u8> {
    match val {
        b'0'..=b'9' => Some(val - b'0'),
        b'A'..=b'F' => Some(val - b'A' + 10),
        b'a'..=b'f' => Some(val - b'a' + 10),
        _ => None,
    }
}

const fn build_hex_table(shift: usize) -> [i16; 256] {
    let mut table = [0; 256];
    let mut ch = 0;
    while ch < 256 {
        table[ch] = match decode_hex_val_slow(ch as u8) {
            Some(val) => (val as i16) << shift,
            None => -1,
        };
        ch += 1;
    }
    table
}

static HEX0: [i16; 256] = build_hex_table(0);
static HEX1: [i16; 256] = build_hex_table(4);

fn decode_four_hex_digits(a: u8, b: u8, c: u8, d: u8) -> Option<u16> {
    let a = HEX1[a as usize] as i32;
    let b = HEX0[b as usize] as i32;
    let c = HEX1[c as usize] as i32;
    let d = HEX0[d as usize] as i32;

    let codepoint = ((a | b) << 8) | c | d;

    // A single sign bit check.
    if codepoint >= 0 {
        Some(codepoint as u16)
    } else {
        None
    }
}
