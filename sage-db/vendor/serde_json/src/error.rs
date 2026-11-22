//! When serializing or deserializing JSON goes wrong.

use crate::io;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use core::fmt::{self, Debug, Display};
use core::result;
use core::str::FromStr;
use serde::{de, ser};
#[cfg(feature = "std")]
use std::error;
#[cfg(feature = "std")]
use std::io::ErrorKind;

/// This type represents all possible errors that can occur when serializing or
/// deserializing JSON data.
pub struct Error {
    storage: Storage,
}

/// Private storage for [`Error`].
///
/// The default arm is exactly the historical representation (a boxed
/// `ErrorImpl`), so default builds behave identically. The `FallibleInline` arm
/// exists only for the opt-in `admission` mode: it carries the same three
/// fields inline so a syntax or unwind error can be **constructed without
/// allocating**, which is what makes a fallible decode path possible at all.
///
/// No public API exposes this layout, so widening it is not a public breaking
/// change; `size_of::<Error>()` grows by one discriminant.
enum Storage {
    Ordinary(Box<ErrorImpl>),
    #[cfg(feature = "admission")]
    FallibleInline {
        code: ErrorCode,
        line: usize,
        column: usize,
    },
}

impl Error {
    /// The admission failure this error carries, if it is one.
    ///
    /// Returns the **same** `AdmissionError` the seam produced — caller refusal,
    /// extent overflow, allocator failure, or an internal borrow conflict — so a
    /// caller can distinguish them and recover the counts, instead of reading a
    /// flattened code. Allocation-free: the payload is `Copy` and stored inline.
    #[cfg(feature = "admission")]
    #[must_use]
    pub fn admission_error(&self) -> Option<crate::admission::AdmissionError> {
        match self.code_at() {
            ErrorCode::Admission(error) => Some(*error),
            _ => None,
        }
    }

    /// The recorded line, whichever arm holds it.
    #[inline]
    fn line_at(&self) -> usize {
        match &self.storage {
            Storage::Ordinary(err) => err.line,
            #[cfg(feature = "admission")]
            Storage::FallibleInline { line, .. } => *line,
        }
    }

    /// The recorded column, whichever arm holds it.
    #[inline]
    fn column_at(&self) -> usize {
        match &self.storage {
            Storage::Ordinary(err) => err.column,
            #[cfg(feature = "admission")]
            Storage::FallibleInline { column, .. } => *column,
        }
    }

    #[inline]
    fn code_at(&self) -> &ErrorCode {
        match &self.storage {
            Storage::Ordinary(err) => &err.code,
            #[cfg(feature = "admission")]
            Storage::FallibleInline { code, .. } => code,
        }
    }
}

/// Alias for a `Result` with the error type `serde_json::Error`.
pub type Result<T> = result::Result<T, Error>;

impl Error {
    /// One-based line number at which the error was detected.
    ///
    /// Characters in the first line of the input (before the first newline
    /// character) are in line 1.
    pub fn line(&self) -> usize {
        self.line_at()
    }

    /// One-based column number at which the error was detected.
    ///
    /// The first character in the input and any characters immediately
    /// following a newline character are in column 1.
    ///
    /// Note that errors may occur in column 0, for example if a read from an
    /// I/O stream fails immediately following a previously read newline
    /// character.
    pub fn column(&self) -> usize {
        self.column_at()
    }

    /// Categorizes the cause of this error.
    ///
    /// - `Category::Io` - failure to read or write bytes on an I/O stream
    /// - `Category::Syntax` - input that is not syntactically valid JSON
    /// - `Category::Data` - input data that is semantically incorrect
    /// - `Category::Eof` - unexpected end of the input data
    pub fn classify(&self) -> Category {
        match *self.code_at() {
            ErrorCode::Message(_) => Category::Data,
            #[cfg(feature = "admission")]
            ErrorCode::Data(_) => Category::Data,
            #[cfg(feature = "admission")]
            ErrorCode::Admission(_) => Category::Data,
            ErrorCode::Io(_) => Category::Io,
            ErrorCode::EofWhileParsingList
            | ErrorCode::EofWhileParsingObject
            | ErrorCode::EofWhileParsingString
            | ErrorCode::EofWhileParsingValue => Category::Eof,
            ErrorCode::ExpectedColon
            | ErrorCode::ExpectedListCommaOrEnd
            | ErrorCode::ExpectedObjectCommaOrEnd
            | ErrorCode::ExpectedSomeIdent
            | ErrorCode::ExpectedSomeValue
            | ErrorCode::ExpectedDoubleQuote
            | ErrorCode::InvalidEscape
            | ErrorCode::InvalidNumber
            | ErrorCode::NumberOutOfRange
            | ErrorCode::InvalidUnicodeCodePoint
            | ErrorCode::ControlCharacterWhileParsingString
            | ErrorCode::KeyMustBeAString
            | ErrorCode::ExpectedNumericKey
            | ErrorCode::FloatKeyMustBeFinite
            | ErrorCode::LoneLeadingSurrogateInHexEscape
            | ErrorCode::TrailingComma
            | ErrorCode::TrailingCharacters
            | ErrorCode::UnexpectedEndOfHexEscape
            | ErrorCode::RecursionLimitExceeded => Category::Syntax,
        }
    }

    /// Returns true if this error was caused by a failure to read or write
    /// bytes on an I/O stream.
    pub fn is_io(&self) -> bool {
        self.classify() == Category::Io
    }

    /// Returns true if this error was caused by input that was not
    /// syntactically valid JSON.
    pub fn is_syntax(&self) -> bool {
        self.classify() == Category::Syntax
    }

    /// Returns true if this error was caused by input data that was
    /// semantically incorrect.
    ///
    /// For example, JSON containing a number is semantically incorrect when the
    /// type being deserialized into holds a String.
    pub fn is_data(&self) -> bool {
        self.classify() == Category::Data
    }

    /// Returns true if this error was caused by prematurely reaching the end of
    /// the input data.
    ///
    /// Callers that process streaming input may be interested in retrying the
    /// deserialization once more data is available.
    pub fn is_eof(&self) -> bool {
        self.classify() == Category::Eof
    }

    /// The kind reported by the underlying standard library I/O error, if this
    /// error was caused by a failure to read or write bytes on an I/O stream.
    ///
    /// # Example
    ///
    /// ```
    /// use serde_json::Value;
    /// use std::io::{self, ErrorKind, Read};
    /// use std::process;
    ///
    /// struct ReaderThatWillTimeOut<'a>(&'a [u8]);
    ///
    /// impl<'a> Read for ReaderThatWillTimeOut<'a> {
    ///     fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    ///         if self.0.is_empty() {
    ///             Err(io::Error::new(ErrorKind::TimedOut, "timed out"))
    ///         } else {
    ///             self.0.read(buf)
    ///         }
    ///     }
    /// }
    ///
    /// fn main() {
    ///     let reader = ReaderThatWillTimeOut(br#" {"k": "#);
    ///
    ///     let _: Value = match serde_json::from_reader(reader) {
    ///         Ok(value) => value,
    ///         Err(error) => {
    ///             if error.io_error_kind() == Some(ErrorKind::TimedOut) {
    ///                 // Maybe this application needs to retry certain kinds of errors.
    ///
    ///                 # return;
    ///             } else {
    ///                 eprintln!("error: {}", error);
    ///                 process::exit(1);
    ///             }
    ///         }
    ///     };
    /// }
    /// ```
    #[cfg(feature = "std")]
    pub fn io_error_kind(&self) -> Option<ErrorKind> {
        if let ErrorCode::Io(io_error) = self.code_at() {
            Some(io_error.kind())
        } else {
            None
        }
    }
}

/// Categorizes the cause of a `serde_json::Error`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Category {
    /// The error was caused by a failure to read or write bytes on an I/O
    /// stream.
    Io,

    /// The error was caused by input that was not syntactically valid JSON.
    Syntax,

    /// The error was caused by input data that was semantically incorrect.
    ///
    /// For example, JSON containing a number is semantically incorrect when the
    /// type being deserialized into holds a String.
    Data,

    /// The error was caused by prematurely reaching the end of the input data.
    ///
    /// Callers that process streaming input may be interested in retrying the
    /// deserialization once more data is available.
    Eof,
}

#[cfg(feature = "std")]
#[allow(clippy::fallible_impl_from)]
impl From<Error> for io::Error {
    /// Convert a `serde_json::Error` into an `io::Error`.
    ///
    /// JSON syntax and data errors are turned into `InvalidData` I/O errors.
    /// EOF errors are turned into `UnexpectedEof` I/O errors.
    ///
    /// ```
    /// use std::io;
    ///
    /// enum MyError {
    ///     Io(io::Error),
    ///     Json(serde_json::Error),
    /// }
    ///
    /// impl From<serde_json::Error> for MyError {
    ///     fn from(err: serde_json::Error) -> MyError {
    ///         use serde_json::error::Category;
    ///         match err.classify() {
    ///             Category::Io => {
    ///                 MyError::Io(err.into())
    ///             }
    ///             Category::Syntax | Category::Data | Category::Eof => {
    ///                 MyError::Json(err)
    ///             }
    ///         }
    ///     }
    /// }
    /// ```
    fn from(j: Error) -> Self {
        let category = j.classify();
        if category == Category::Io {
            if let ErrorCode::Io(err) = j.into_code() {
                return err;
            }
            unreachable!("classify() reported Io, so the code is Io");
        }
        match category {
            Category::Io => unreachable!(),
            Category::Syntax | Category::Data => io::Error::new(ErrorKind::InvalidData, j),
            Category::Eof => io::Error::new(ErrorKind::UnexpectedEof, j),
        }
    }
}

struct ErrorImpl {
    code: ErrorCode,
    line: usize,
    column: usize,
}

/// A classified data refusal with no message.
///
/// Used by the opt-in admission mode so a seed can refuse without allocating.
/// Rendered by [`Display`] as a fixed phrase per variant;
/// the precise classification is what the caller branches on.
#[cfg(feature = "admission")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataKind {
    /// A field name this message does not have.
    UnknownField,
    /// A field appeared more than once.
    DuplicateField,
    /// A value did not match the declared type.
    WrongType,
    /// A required field was absent.
    MissingField,
    /// The value was well-formed JSON but rejected by a declared constraint.
    Constraint,
}

#[cfg(feature = "admission")]
impl core::fmt::Display for DataKind {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(match self {
            Self::UnknownField => "unknown field",
            Self::DuplicateField => "duplicate field",
            Self::WrongType => "wrong type for field",
            Self::MissingField => "missing field",
            Self::Constraint => "declared constraint violated",
        })
    }
}

pub(crate) enum ErrorCode {
    /// Catchall for syntax error messages
    Message(Box<str>),
    /// A classified, message-free data refusal (admission mode only).
    #[cfg(feature = "admission")]
    Data(DataKind),
    /// An admission failure, carrying the real class and counts.
    ///
    /// Payload-preserving on purpose: the caller must be able to distinguish "my
    /// policy said no" from "the extent was unrepresentable" from "the allocator
    /// failed after admission", and must recover the same `AdmissionError` the
    /// seam produced rather than a flattened code.
    #[cfg(feature = "admission")]
    Admission(crate::admission::AdmissionError),

    /// Some I/O error occurred while serializing or deserializing.
    Io(io::Error),

    /// EOF while parsing a list.
    EofWhileParsingList,

    /// EOF while parsing an object.
    EofWhileParsingObject,

    /// EOF while parsing a string.
    EofWhileParsingString,

    /// EOF while parsing a JSON value.
    EofWhileParsingValue,

    /// Expected this character to be a `':'`.
    ExpectedColon,

    /// Expected this character to be either a `','` or a `']'`.
    ExpectedListCommaOrEnd,

    /// Expected this character to be either a `','` or a `'}'`.
    ExpectedObjectCommaOrEnd,

    /// Expected to parse either a `true`, `false`, or a `null`.
    ExpectedSomeIdent,

    /// Expected this character to start a JSON value.
    ExpectedSomeValue,

    /// Expected this character to be a `"`.
    ExpectedDoubleQuote,

    /// Invalid hex escape code.
    InvalidEscape,

    /// Invalid number.
    InvalidNumber,

    /// Number is bigger than the maximum value of its type.
    NumberOutOfRange,

    /// Invalid unicode code point.
    InvalidUnicodeCodePoint,

    /// Control character found while parsing a string.
    ControlCharacterWhileParsingString,

    /// Object key is not a string.
    KeyMustBeAString,

    /// Contents of key were supposed to be a number.
    ExpectedNumericKey,

    /// Object key is a non-finite float value.
    FloatKeyMustBeFinite,

    /// Lone leading surrogate in hex escape.
    LoneLeadingSurrogateInHexEscape,

    /// JSON has a comma after the last value in an array or map.
    TrailingComma,

    /// JSON has non-whitespace trailing characters after the value.
    TrailingCharacters,

    /// Unexpected end of hex escape.
    UnexpectedEndOfHexEscape,

    /// Encountered nesting of JSON maps and arrays more than 128 layers deep.
    RecursionLimitExceeded,
}

impl Error {
    #[cold]
    pub(crate) fn syntax(code: ErrorCode, line: usize, column: usize) -> Self {
        // ADMISSION MODE: parser/reader syntax errors are constructed inline.
        //
        // This is the single choke point every funnel and every direct site goes
        // through (`Deserializer::error`, `Deserializer::peek_error`, `read::error`
        // and the direct `Error::syntax` calls), so routing it here is what makes
        // the claim "no parser syntax error on this path allocates" true rather
        // than aspirational. It covers *every* `ErrorCode` reachable from parsing,
        // because the alternative — an allow-list of codes — is a list to forget to
        // extend.
        //
        // `Message` codes are not constructed here (they come through
        // `make_error`), so the boxed `String` case is unaffected.
        #[cfg(feature = "admission")]
        {
            return Error {
                storage: Storage::FallibleInline { code, line, column },
            };
        }
        #[cfg(not(feature = "admission"))]
        {
            Error {
                storage: Storage::Ordinary(Box::new(ErrorImpl { code, line, column })),
            }
        }
    }

    /// Allocation-free syntax error for the fallible admission mode.
    ///
    /// Identical in meaning to [`Self::syntax`], but the three fields are stored
    /// inline so no `Box` (and, for syntax codes, no `Box<str>`) is allocated.
    /// Only codes reachable from parsing are routed here; `Message` codes keep
    /// the ordinary arm.
    #[cfg(feature = "admission")]
    #[cold]
    pub(crate) fn syntax_inline(code: ErrorCode, line: usize, column: usize) -> Self {
        Error {
            storage: Storage::FallibleInline { code, line, column },
        }
    }

    /// Consume the error and hand back its code.
    fn into_code(self) -> ErrorCode {
        match self.storage {
            Storage::Ordinary(err) => err.code,
            #[cfg(feature = "admission")]
            Storage::FallibleInline { code, .. } => code,
        }
    }

    // Not public API. Should be pub(crate).
    //
    // Update `eager_json` crate when this function changes.
    #[doc(hidden)]
    #[cold]
    pub fn io(error: io::Error) -> Self {
        Error {
            storage: Storage::Ordinary(Box::new(ErrorImpl {
                code: ErrorCode::Io(error),
                line: 0,
                column: 0,
            })),
        }
    }

    #[cold]
    pub(crate) fn fix_position<F>(self, f: F) -> Self
    where
        F: FnOnce(ErrorCode) -> Error,
    {
        if self.line_at() == 0 {
            match self.storage {
                Storage::Ordinary(err) => f(err.code),
                #[cfg(feature = "admission")]
                Storage::FallibleInline { code, .. } => f(code),
            }
        } else {
            self
        }
    }
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ErrorCode::Message(msg) => f.write_str(msg),
            #[cfg(feature = "admission")]
            ErrorCode::Data(kind) => core::fmt::Display::fmt(kind, f),
            #[cfg(feature = "admission")]
            ErrorCode::Admission(error) => core::fmt::Display::fmt(error, f),
            ErrorCode::Io(err) => Display::fmt(err, f),
            ErrorCode::EofWhileParsingList => f.write_str("EOF while parsing a list"),
            ErrorCode::EofWhileParsingObject => f.write_str("EOF while parsing an object"),
            ErrorCode::EofWhileParsingString => f.write_str("EOF while parsing a string"),
            ErrorCode::EofWhileParsingValue => f.write_str("EOF while parsing a value"),
            ErrorCode::ExpectedColon => f.write_str("expected `:`"),
            ErrorCode::ExpectedListCommaOrEnd => f.write_str("expected `,` or `]`"),
            ErrorCode::ExpectedObjectCommaOrEnd => f.write_str("expected `,` or `}`"),
            ErrorCode::ExpectedSomeIdent => f.write_str("expected ident"),
            ErrorCode::ExpectedSomeValue => f.write_str("expected value"),
            ErrorCode::ExpectedDoubleQuote => f.write_str("expected `\"`"),
            ErrorCode::InvalidEscape => f.write_str("invalid escape"),
            ErrorCode::InvalidNumber => f.write_str("invalid number"),
            ErrorCode::NumberOutOfRange => f.write_str("number out of range"),
            ErrorCode::InvalidUnicodeCodePoint => f.write_str("invalid unicode code point"),
            ErrorCode::ControlCharacterWhileParsingString => {
                f.write_str("control character (\\u0000-\\u001F) found while parsing a string")
            }
            ErrorCode::KeyMustBeAString => f.write_str("key must be a string"),
            ErrorCode::ExpectedNumericKey => {
                f.write_str("invalid value: expected key to be a number in quotes")
            }
            ErrorCode::FloatKeyMustBeFinite => {
                f.write_str("float key must be finite (got NaN or +/-inf)")
            }
            ErrorCode::LoneLeadingSurrogateInHexEscape => {
                f.write_str("lone leading surrogate in hex escape")
            }
            ErrorCode::TrailingComma => f.write_str("trailing comma"),
            ErrorCode::TrailingCharacters => f.write_str("trailing characters"),
            ErrorCode::UnexpectedEndOfHexEscape => f.write_str("unexpected end of hex escape"),
            ErrorCode::RecursionLimitExceeded => f.write_str("recursion limit exceeded"),
        }
    }
}

impl serde::de::StdError for Error {
    #[cfg(feature = "std")]
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self.code_at() {
            ErrorCode::Io(err) => err.source(),
            _ => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.line_at() == 0 {
            Display::fmt(self.code_at(), f)
        } else {
            write!(
                f,
                "{} at line {} column {}",
                self.code_at(),
                self.line_at(),
                self.column_at()
            )
        }
    }
}

impl Display for ErrorImpl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.line == 0 {
            Display::fmt(&self.code, f)
        } else {
            write!(
                f,
                "{} at line {} column {}",
                self.code, self.line, self.column
            )
        }
    }
}

// Remove two layers of verbosity from the debug representation. Humans often
// end up seeing this representation because it is what unwrap() shows.
impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Error({:?}, line: {}, column: {})",
            self.code_at().to_string(),
            self.line_at(),
            self.column_at()
        )
    }
}

impl Error {
    /// An allocation-free data error, used by the admission path.
    ///
    /// `Error::custom` must format a `Display` into an owned string, which
    /// allocates. A classified data refusal needs no text, so it takes this
    /// route instead: the kind is stored inline and rendered on demand.
    #[cfg(feature = "admission")]
    #[cold]
    pub(crate) fn data(kind: DataKind) -> Error {
        Error {
            storage: Storage::FallibleInline {
                code: ErrorCode::Data(kind),
                line: 0,
                column: 0,
            },
        }
    }
}

impl de::Error for Error {
    #[cold]
    fn custom<T: Display>(msg: T) -> Error {
        #[cfg(feature = "admission")]
        {
            // ADMISSION MODE: an allocation-free, classified data error.
            //
            // A visitor's `A::Error` is generic, so a seed cannot choose the error
            // type; `E::custom` is the only construction path available to it. If
            // that path formats a message (as it must without this branch), every
            // refusal allocates — which is the defect this feature exists to
            // remove. So under the feature, `custom` builds the inline `Data` arm
            // and discards the message.
            //
            // DISCLOSED TRADE-OFF: this changes the *custom diagnostic text* of
            // ordinary APIs when the feature is on. Cargo features are additive
            // and unify across a dependency graph, so an unrelated crate enabling
            // `admission` changes what `Error::to_string()` prints for a data
            // error in every crate of that build. Classification is unchanged:
            // it is still `Category::Data`.
            // Recover the classification WITHOUT formatting: `carried_kind`
            // inspects the message object via a downcast-free marker. `DataKind`
            // itself implements `Display` writing its own name, and
            // `AdmissionRefusal` carries a kind it can hand back; anything else
            // (an ordinary caller's message) becomes `Constraint`, which is the
            // honest "declared constraint violated" class for a refused value.
            let kind = crate::admission::carried_kind(&msg);
            return Error {
                storage: Storage::FallibleInline {
                    code: ErrorCode::Data(kind),
                    line: 0,
                    column: 0,
                },
            };
        }
        #[cfg(not(feature = "admission"))]
        {
            make_error(msg.to_string())
        }
    }

    #[cold]
    fn invalid_type(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self {
        Error::custom(format_args!(
            "invalid type: {}, expected {}",
            JsonUnexpected(unexp),
            exp,
        ))
    }

    #[cold]
    fn invalid_value(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self {
        Error::custom(format_args!(
            "invalid value: {}, expected {}",
            JsonUnexpected(unexp),
            exp,
        ))
    }
}

impl ser::Error for Error {
    #[cold]
    fn custom<T: Display>(msg: T) -> Error {
        make_error(msg.to_string())
    }
}

struct JsonUnexpected<'a>(de::Unexpected<'a>);

impl<'a> Display for JsonUnexpected<'a> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            de::Unexpected::Unit => formatter.write_str("null"),
            de::Unexpected::Float(value) => write!(
                formatter,
                "floating point `{}`",
                zmij::Buffer::new().format(value),
            ),
            unexp => Display::fmt(&unexp, formatter),
        }
    }
}

// Parse our own error message that looks like "{} at line {} column {}" to work
// around erased-serde round-tripping the error through de::Error::custom.
fn make_error(mut msg: String) -> Error {
    let (line, column) = parse_line_col(&mut msg).unwrap_or((0, 0));
    Error {
        storage: Storage::Ordinary(Box::new(ErrorImpl {
            code: ErrorCode::Message(msg.into_boxed_str()),
            line,
            column,
        })),
    }
}

fn parse_line_col(msg: &mut String) -> Option<(usize, usize)> {
    let start_of_suffix = match msg.rfind(" at line ") {
        Some(index) => index,
        None => return None,
    };

    // Find start and end of line number.
    let start_of_line = start_of_suffix + " at line ".len();
    let mut end_of_line = start_of_line;
    while starts_with_digit(&msg[end_of_line..]) {
        end_of_line += 1;
    }

    if !msg[end_of_line..].starts_with(" column ") {
        return None;
    }

    // Find start and end of column number.
    let start_of_column = end_of_line + " column ".len();
    let mut end_of_column = start_of_column;
    while starts_with_digit(&msg[end_of_column..]) {
        end_of_column += 1;
    }

    if end_of_column < msg.len() {
        return None;
    }

    // Parse numbers.
    let line = match usize::from_str(&msg[start_of_line..end_of_line]) {
        Ok(line) => line,
        Err(_) => return None,
    };
    let column = match usize::from_str(&msg[start_of_column..end_of_column]) {
        Ok(column) => column,
        Err(_) => return None,
    };

    msg.truncate(start_of_suffix);
    Some((line, column))
}

fn starts_with_digit(slice: &str) -> bool {
    match slice.as_bytes().first() {
        None => false,
        Some(&byte) => byte >= b'0' && byte <= b'9',
    }
}
