//! Bounded single-request HTTP/1.1 framing for the loopback listener.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

use sage_db_protocol::generated::ErrorCode;
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::{ControlStop, QueryControl};
use serde::Serialize;

pub(crate) const MAX_BODY: usize = 256 * 1024;
pub(crate) const MAX_RESPONSE: usize = 128 * 1024;
// Complete graph snapshots and proposal lists grow with the knowledge base.
// Their larger, ledger-admitted buffers do not change single-record limits.
pub(crate) const MAX_BATCH_RESPONSE: usize = 512 * 1024;
// The named private direct-concept frame has separately bounded JSON metadata
// and raw UTF-8. Existing JSON methods retain MAX_BODY.
pub(crate) const DIRECT_CONCEPT_PREFIX: usize = 4;
const MAX_DIRECT_CONCEPT_REQUEST: usize =
    DIRECT_CONCEPT_PREFIX + MAX_BODY + sage_db_kernel::term_create::MAX_DIRECT_BODY_BYTES;
pub(crate) const MAX_HEAD: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) struct Deadline {
    start: Instant,
    until: Cell<Instant>,
}

impl Deadline {
    pub(crate) fn new() -> Self {
        let start = Instant::now();
        Self {
            start,
            until: Cell::new(start + REQUEST_TIMEOUT),
        }
    }

    pub(crate) fn tighten(&self, millis: Option<u64>) {
        if let Some(millis) = millis {
            let requested = Duration::from_millis(millis).min(REQUEST_TIMEOUT);
            self.until.set(self.until.get().min(self.start + requested));
        }
    }

    fn remaining(&self) -> Result<Duration, Failure> {
        let left = self.until.get().saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(Failure::stopped(ControlStop::DeadlineExceeded))
        } else {
            Ok(left)
        }
    }
}

impl QueryControl for Deadline {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        (Instant::now() < self.until.get())
            .then_some(())
            .ok_or(ControlStop::DeadlineExceeded)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Failure {
    status: &'static str,
    code: ErrorCode,
    message: &'static str,
    budget: Option<BudgetError>,
}

impl Failure {
    pub(crate) const fn new(status: &'static str, code: ErrorCode, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            budget: None,
        }
    }

    pub(crate) const fn message(self) -> &'static str {
        self.message
    }

    pub(crate) const fn bad(message: &'static str) -> Self {
        Self::new("400 Bad Request", ErrorCode::InvalidArgument, message)
    }

    pub(crate) const fn unauthorized() -> Self {
        Self::new(
            "401 Unauthorized",
            ErrorCode::Unauthenticated,
            "bearer credential required",
        )
    }

    pub(crate) const fn unsupported(message: &'static str) -> Self {
        Self::new("400 Bad Request", ErrorCode::Unsupported, message)
    }

    pub(crate) const fn unavailable(message: &'static str) -> Self {
        Self::new("503 Service Unavailable", ErrorCode::ReadOnly, message)
    }

    pub(crate) const fn internal() -> Self {
        Self::new(
            "500 Internal Server Error",
            ErrorCode::Unspecified,
            "service could not complete the request",
        )
    }

    pub(crate) fn refused(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Budget(budget) => Self {
                status: "429 Too Many Requests",
                code: ErrorCode::Budget,
                message: "request work exceeds its allowance",
                budget: Some(budget),
            },
            Refusal::Stopped(stop) => Self::stopped(stop),
            Refusal::Allocation { .. } => Self::internal(),
        }
    }

    fn stopped(stop: ControlStop) -> Self {
        match stop {
            ControlStop::Cancelled => Self::new(
                "408 Request Timeout",
                ErrorCode::Cancelled,
                "request cancelled",
            ),
            ControlStop::DeadlineExceeded => Self::new(
                "408 Request Timeout",
                ErrorCode::Deadline,
                "request deadline expired",
            ),
        }
    }

    fn socket(error: &io::Error) -> Self {
        if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            Self::stopped(ControlStop::DeadlineExceeded)
        } else {
            Self::bad("connection ended before the request")
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Route {
    Handshake,
    ReadTermText,
    ReadTermDocument,
    ReadTermActivation,
    ReadGraphSnapshot,
    ReadFrontierDocument,
    WriteFrontierDocument,
    ReadCompatCheckpoint,
    WriteCompatCheckpoint,
    ReadProposalFiles,
    Submit,
    SubmitLargeBodyProposal,
    Review,
    Commit,
    CommitDirectConcept,
    Outcome,
    Activate,
    ConceptBody,
    Edges,
    Path,
    Manifest,
    ManifestRebuild,
    Search,
    Graph,
    GraphRebuild,
    Viz,
    Health,
    Catalog,
    Meta,
    Validate,
    Revision,
    Frontier,
    NativeFrontier,
    NativeCheckpoint,
    LegacyCheckpoint,
    Proposals,
    KnowledgeProposals,
    Proposal,
}

impl Route {
    fn parse(path: &str, method: Option<&str>) -> Result<Self, Failure> {
        if path == "/db/v1/checkpoint-document" {
            return Ok(if method == Some("POST") {
                Self::ReadCompatCheckpoint
            } else {
                Self::WriteCompatCheckpoint
            });
        }
        if path == "/db/v1/frontier-document" {
            return Ok(if method == Some("GET") {
                Self::ReadFrontierDocument
            } else {
                Self::WriteFrontierDocument
            });
        }
        let location = path.split_once('?').map_or(path, |(path, _)| path);
        match location {
            "/api/path" => return Ok(Self::Path),
            "/api/manifest" if method == Some("PUT") => return Ok(Self::ManifestRebuild),
            "/api/manifest" => return Ok(Self::Manifest),
            "/api/search" => return Ok(Self::Search),
            "/api/graph" if method == Some("PUT") => return Ok(Self::GraphRebuild),
            "/api/graph" => return Ok(Self::Graph),
            "/api/viz" => return Ok(Self::Viz),
            "/api/health" => return Ok(Self::Health),
            "/api/knowledge-bases" => return Ok(Self::Catalog),
            "/api/meta" => return Ok(Self::Meta),
            "/api/validate" => return Ok(Self::Validate),
            "/api/revisions" => return Ok(Self::Revision),
            "/api/frontier" if method == Some("GET") => return Ok(Self::Frontier),
            "/api/knowledge/frontier" => return Ok(Self::NativeFrontier),
            "/api/proposals" if method == Some("GET") => return Ok(Self::Proposals),
            "/api/knowledge/proposals" if method == Some("GET") => {
                return Ok(Self::KnowledgeProposals);
            }
            _ => {}
        }
        if location
            .strip_prefix("/api/knowledge/checkpoints/")
            .is_some_and(|slug| !slug.is_empty() && !slug.contains('/'))
        {
            return Ok(Self::NativeCheckpoint);
        }
        if location
            .strip_prefix("/api/checkpoints/")
            .is_some_and(|slug| !slug.is_empty() && !slug.contains('/'))
        {
            return Ok(Self::LegacyCheckpoint);
        }
        if let Some(rest) = location.strip_prefix("/api/concepts/") {
            let (slug, route) = if let Some(slug) = rest.strip_suffix("/edges") {
                (slug, Self::Edges)
            } else if let Some(slug) = rest.strip_suffix("/body") {
                (slug, Self::ConceptBody)
            } else {
                (rest, Self::Activate)
            };
            if !slug.is_empty() && !slug.contains('/') {
                return Ok(route);
            }
        }
        if ["/api/proposals/", "/api/knowledge/proposals/"]
            .into_iter()
            .any(|prefix| {
                location
                    .strip_prefix(prefix)
                    .is_some_and(|id| id.starts_with("prp_") && !id.contains('/'))
            })
        {
            return Ok(Self::Proposal);
        }
        let name = path.strip_prefix("/sage.v1.SageCommandService/");
        match name {
            Some("Handshake") => Ok(Self::Handshake),
            Some("ReadTermText") => Ok(Self::ReadTermText),
            Some("ReadTermDocument") => Ok(Self::ReadTermDocument),
            Some("ReadTermActivation") => Ok(Self::ReadTermActivation),
            Some("ReadGraphSnapshot") => Ok(Self::ReadGraphSnapshot),
            Some("ReadProposalFiles") => Ok(Self::ReadProposalFiles),
            Some("SubmitProposal") => Ok(Self::Submit),
            Some("SubmitLargeBodyProposal") => Ok(Self::SubmitLargeBodyProposal),
            Some("ReviewProposal") => Ok(Self::Review),
            Some("CommitCommand") => Ok(Self::Commit),
            Some("CommitDirectConcept") => Ok(Self::CommitDirectConcept),
            Some("GetRequestOutcome") => Ok(Self::Outcome),
            _ => Err(Failure::new(
                "404 Not Found",
                ErrorCode::Unsupported,
                "method is unavailable",
            )),
        }
    }

    pub(crate) fn legacy(self) -> bool {
        matches!(
            self,
            Self::Activate
                | Self::ConceptBody
                | Self::Edges
                | Self::Path
                | Self::Manifest
                | Self::ManifestRebuild
                | Self::Search
                | Self::Graph
                | Self::GraphRebuild
                | Self::Viz
                | Self::Health
                | Self::Catalog
                | Self::Meta
                | Self::Validate
                | Self::Revision
                | Self::Frontier
                | Self::NativeFrontier
                | Self::NativeCheckpoint
                | Self::LegacyCheckpoint
                | Self::Proposals
                | Self::KnowledgeProposals
                | Self::Proposal
        )
    }

    fn write(self) -> bool {
        matches!(
            self,
            Self::Submit
                | Self::SubmitLargeBodyProposal
                | Self::Review
                | Self::Commit
                | Self::CommitDirectConcept
        )
    }
}

pub(crate) struct Head<'a> {
    pub(crate) route: Route,
    pub(crate) bearer: &'a [u8],
    pub(crate) target: &'a str,
    pub(crate) accept: Option<&'a [u8]>,
    len: usize,
    prefix: &'a [u8],
}

fn parse_head(bytes: &[u8], end: usize) -> Result<Head<'_>, Failure> {
    let raw = &bytes[..end];
    // Reject alternative line boundaries before passing the frame to the HTTP parser.
    if !raw
        .split_inclusive(|&b| b == b'\n')
        .all(|line| line.ends_with(b"\r\n"))
    {
        return Err(Failure::bad("incorrect header line ending"));
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if request.parse(raw).ok() != Some(httparse::Status::Complete(end))
        || request.version != Some(1)
    {
        return Err(Failure::bad("expected a complete HTTP/1.1 request"));
    }
    let target = request
        .path
        .ok_or_else(|| Failure::bad("request path required"))?;
    let route = Route::parse(target, request.method)?;
    let method = match route {
        Route::ManifestRebuild
        | Route::GraphRebuild
        | Route::WriteFrontierDocument
        | Route::WriteCompatCheckpoint => "PUT",
        Route::ReadFrontierDocument => "GET",
        route if route.legacy() => "GET",
        _ => "POST",
    };
    if request.method != Some(method) {
        return Err(Failure::new(
            "405 Method Not Allowed",
            ErrorCode::Unsupported,
            method,
        ));
    }
    let mut length = None;
    let mut auth = None;
    let mut content_type = None;
    let mut host = None;
    let mut accept = None;
    for header in request.headers {
        let name = header.name;
        if name.eq_ignore_ascii_case("transfer-encoding") || name.eq_ignore_ascii_case("expect") {
            return Err(Failure::bad("unsupported HTTP framing"));
        }
        let slot = if name.eq_ignore_ascii_case("content-length") {
            &mut length
        } else if name.eq_ignore_ascii_case("authorization") {
            &mut auth
        } else if name.eq_ignore_ascii_case("content-type") {
            &mut content_type
        } else if name.eq_ignore_ascii_case("host") {
            &mut host
        } else if name.eq_ignore_ascii_case("accept") {
            &mut accept
        } else {
            continue;
        };
        if slot.replace(header.value).is_some() {
            return Err(Failure::bad("duplicate framing header"));
        }
    }
    if host.is_none_or(<[u8]>::is_empty) {
        return Err(Failure::bad("Host is required"));
    }
    let bearer = auth
        .filter(|value| {
            value
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"Bearer "))
        })
        .and_then(|value| value.get(7..))
        .filter(|value| !value.is_empty());
    let bearer = match bearer {
        Some(bearer) => bearer,
        None if matches!(route, Route::Health) => &[],
        None => return Err(Failure::unauthorized()),
    };
    let len = if matches!(route, Route::ReadFrontierDocument)
        || (route.legacy() && !matches!(route, Route::GraphRebuild))
    {
        if length.is_some_and(|value| value != b"0") {
            return Err(Failure::bad("this route does not accept a request body"));
        }
        0
    } else if matches!(route, Route::GraphRebuild) && length.is_none_or(|value| value == b"0") {
        0
    } else {
        let (media, limit, expected) = match route {
            Route::WriteFrontierDocument => (
                "application/octet-stream",
                crate::frontier_document::MAX_REQUEST,
                "expected application/octet-stream",
            ),
            Route::ReadCompatCheckpoint => (
                "application/octet-stream",
                crate::compat_checkpoint::MAX_INSPECT,
                "expected application/octet-stream",
            ),
            Route::WriteCompatCheckpoint => (
                "application/octet-stream",
                crate::compat_checkpoint::MAX_WRITE,
                "expected application/octet-stream",
            ),
            Route::CommitDirectConcept => (
                "application/octet-stream",
                MAX_DIRECT_CONCEPT_REQUEST,
                "expected application/octet-stream",
            ),
            Route::SubmitLargeBodyProposal => (
                "application/octet-stream",
                DIRECT_CONCEPT_PREFIX
                    + MAX_BODY
                    + sage_db_kernel::legacy_proposal::MAX_COMPAT_PROPOSED_BODY_BYTES,
                "expected application/octet-stream",
            ),
            _ => ("application/json", MAX_BODY, "expected application/json"),
        };
        if !content_type.is_some_and(|value| value.eq_ignore_ascii_case(media.as_bytes())) {
            return Err(Failure::new(
                "415 Unsupported Media Type",
                ErrorCode::Unsupported,
                expected,
            ));
        }
        let Some(length) =
            length.filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_digit))
        else {
            return Err(Failure::bad("one numeric Content-Length is required"));
        };
        std::str::from_utf8(length)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&value| value <= limit)
            .ok_or_else(|| {
                Failure::new(
                    "413 Content Too Large",
                    ErrorCode::Budget,
                    "request body exceeds its ceiling",
                )
            })?
    };
    let prefix = &bytes[end..];
    if prefix.len() > len {
        return Err(Failure::bad("unexpected bytes after request"));
    }
    Ok(Head {
        route,
        bearer,
        target,
        accept,
        len,
        prefix,
    })
}

pub(crate) fn read_head<'a>(
    mut stream: &TcpStream,
    storage: &'a mut [u8; MAX_HEAD],
    deadline: &Deadline,
) -> Result<Head<'a>, Failure> {
    let mut used: usize = 0;
    while used < storage.len() {
        stream
            .set_read_timeout(Some(deadline.remaining()?))
            .map_err(|error| Failure::socket(&error))?;
        let read = stream
            .read(&mut storage[used..])
            .map_err(|error| Failure::socket(&error))?;
        if read == 0 {
            return Err(Failure::bad("incomplete request headers"));
        }
        let start = used.saturating_sub(3);
        used += read;
        if let Some(index) = storage[start..used]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            return parse_head(&storage[..used], start + index + 4);
        }
    }
    Err(Failure::new(
        "431 Request Header Fields Too Large",
        ErrorCode::InvalidArgument,
        "request headers exceed their ceiling",
    ))
}

pub(crate) fn read_body(
    mut stream: &TcpStream,
    head: &Head<'_>,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, Failure> {
    ledger
        .try_charge(head.len as u64)
        .map_err(|error| Failure::refused(Refusal::Budget(error)))?;
    let mut body = Vec::new();
    body.try_reserve_exact(head.len)
        .map_err(|_| Failure::internal())?;
    if body.capacity() != head.len {
        return Err(Failure::internal());
    }
    body.resize(head.len, 0);
    let mut used = head.prefix.len();
    body[..used].copy_from_slice(head.prefix);
    while used < body.len() {
        stream
            .set_read_timeout(Some(deadline.remaining()?))
            .map_err(|error| Failure::socket(&error))?;
        let read = stream
            .read(&mut body[used..])
            .map_err(|error| Failure::socket(&error))?;
        if read == 0 {
            return Err(Failure::bad("incomplete request body"));
        }
        used += read;
    }
    Ok(body)
}

struct Render<'out, 'ledger> {
    out: io::Cursor<&'out mut [u8]>,
    ledger: &'ledger mut dyn SharedLedger,
    prepaid: bool,
}

impl Write for Render<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.ledger
            .checkpoint()
            .map_err(|_| io::ErrorKind::TimedOut)?;
        self.ledger
            .try_charge_entries(1)
            .map_err(|_| io::ErrorKind::OutOfMemory)?;
        if !self.prepaid {
            self.ledger
                .try_charge(bytes.len() as u64)
                .map_err(|_| io::ErrorKind::OutOfMemory)?;
        }
        self.out.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn respond(
    stream: &TcpStream,
    value: &impl Serialize,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> io::Result<()> {
    respond_with(
        stream,
        "200 OK",
        "application/json",
        deadline,
        ledger,
        |out| serde_json::to_writer(out, value).map_err(io::Error::from),
    )
}

pub(crate) fn render<'a>(
    buffer: &'a mut [u8],
    ledger: &mut dyn SharedLedger,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<&'a [u8]> {
    render_in(buffer, ledger, false, write)
}

pub(crate) fn render_prepaid<'a>(
    buffer: &'a mut [u8],
    ledger: &mut dyn SharedLedger,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<&'a [u8]> {
    render_in(buffer, ledger, true, write)
}

pub(crate) fn batch_response_buffer(ledger: &mut dyn SharedLedger) -> Result<Vec<u8>, Failure> {
    ledger
        .checkpoint()
        .map_err(|stop| Failure::refused(Refusal::Stopped(stop)))?;
    let mut buffer = crate::activation::reserve(MAX_BATCH_RESPONSE, ledger)?;
    buffer.resize(MAX_BATCH_RESPONSE, 0);
    Ok(buffer)
}

pub(crate) fn respond_batch(
    stream: &TcpStream,
    value: &impl Serialize,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Failure> {
    let mut buffer = batch_response_buffer(ledger)?;
    respond_prepaid(
        stream,
        "application/json",
        &mut buffer,
        deadline,
        ledger,
        |out| serde_json::to_writer(out, value).map_err(io::Error::from),
    )
    .map_err(crate::activation::render_error)
}

fn render_in<'a>(
    buffer: &'a mut [u8],
    ledger: &mut dyn SharedLedger,
    prepaid: bool,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<&'a [u8]> {
    let mut writer = Render {
        out: io::Cursor::new(buffer),
        ledger,
        prepaid,
    };
    write(&mut writer)?;
    let used = writer.out.position() as usize;
    Ok(&writer.out.into_inner()[..used])
}

pub(crate) fn respond_with(
    stream: &TcpStream,
    status: &'static str,
    content_type: &'static str,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffer = [0; MAX_RESPONSE];
    let body = render(&mut buffer, ledger, write)?;
    send_rendered(stream, status, content_type, body, deadline);
    Ok(())
}

/// The caller already admitted the allocation backing this scratch buffer.
pub(crate) fn respond_prepaid(
    stream: &TcpStream,
    content_type: &'static str,
    buffer: &mut [u8],
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let body = render_prepaid(buffer, ledger, write)?;
    send_rendered(stream, "200 OK", content_type, body, deadline);
    Ok(())
}

fn send_rendered(
    stream: &TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    deadline: &Deadline,
) {
    // Once transmission starts, a failure closes the connection. Sending a
    // second status after a partially written response would corrupt framing.
    let _ = send(
        stream,
        status,
        content_type,
        body,
        deadline.until.get(),
        None,
    );
}

pub(crate) fn respond_octets(
    stream: &TcpStream,
    status: &'static str,
    parts: &[&[u8]],
    deadline: &Deadline,
) {
    let _ = send_parts(
        stream,
        status,
        "application/octet-stream",
        parts,
        deadline.until.get(),
        None,
    );
}

fn send(
    stream: &TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    until: Instant,
    allow: Option<&str>,
) -> io::Result<()> {
    send_parts(stream, status, content_type, &[body], until, allow)
}

fn send_parts(
    mut stream: &TcpStream,
    status: &str,
    content_type: &str,
    body: &[&[u8]],
    until: Instant,
    allow: Option<&str>,
) -> io::Result<()> {
    let body_len = body
        .iter()
        .try_fold(0usize, |len, part| len.checked_add(part.len()))
        .ok_or(io::ErrorKind::InvalidInput)?;
    let mut header = [0u8; 256];
    let mut out = io::Cursor::new(header.as_mut_slice());
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        body_len
    )?;
    match status {
        "401 Unauthorized" => out.write_all(b"WWW-Authenticate: Bearer\r\n")?,
        "405 Method Not Allowed" => write!(out, "Allow: {}\r\n", allow.unwrap_or("POST"))?,
        _ => {}
    }
    out.write_all(b"\r\n")?;
    let written = out.position() as usize;
    for mut slice in core::iter::once(&header[..written]).chain(body.iter().copied()) {
        while !slice.is_empty() {
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            stream.set_write_timeout(Some(remaining))?;
            let sent = stream.write(slice)?;
            if sent == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            slice = &slice[sent..];
        }
    }
    Ok(())
}

pub(crate) fn reply_error(stream: &TcpStream, route: Option<Route>, failure: Failure) {
    let mut buffer = [0u8; 512];
    let mut out = io::Cursor::new(buffer.as_mut_slice());
    let wrap = route.is_some_and(Route::write);
    let result = (|| -> io::Result<()> {
        if wrap {
            out.write_all(b"{\"outcome\":")?;
        }
        write!(
            out,
            "{{\"error\":{{\"code\":\"{}\",\"message\":\"{}\"",
            failure.code.as_str_name(),
            failure.message
        )?;
        if let Some(budget) = failure.budget {
            let unit = match budget.unit() {
                BudgetUnit::Bytes => "BUDGET_UNIT_BYTES",
                BudgetUnit::Entries => "BUDGET_UNIT_ENTRIES",
            };
            write!(
                out,
                ",\"budget\":{{\"unit\":\"{unit}\",\"requested\":\"{}\",\"remaining\":\"{}\"}}",
                budget.requested(),
                budget.remaining()
            )?;
        }
        out.write_all(if wrap { b"}}}" } else { b"}}" })
    })();
    let used = out.position() as usize;
    let until = Instant::now() + ERROR_TIMEOUT;
    if result.is_ok()
        && send(
            stream,
            failure.status,
            "application/json",
            &buffer[..used],
            until,
            (failure.status == "405 Method Not Allowed").then_some(failure.message),
        )
        .is_ok()
    {
        finish_error(stream, until);
    }
}

pub(crate) fn reply_legacy_error(stream: &TcpStream, failure: Failure, message: &str, text: bool) {
    let mut buffer = [0u8; 512];
    let mut out = io::Cursor::new(buffer.as_mut_slice());
    let (result, media) = if text {
        (writeln!(out, "{message}"), "text/plain; charset=utf-8")
    } else {
        (
            serde_json::to_writer(&mut out, &LegacyError { error: message })
                .map_err(io::Error::from),
            "application/json; charset=utf-8",
        )
    };
    let used = out.position() as usize;
    let until = Instant::now() + ERROR_TIMEOUT;
    if result.is_ok() && send(stream, failure.status, media, &buffer[..used], until, None).is_ok() {
        finish_error(stream, until);
    }
}

#[derive(Serialize)]
struct LegacyError<'a> {
    error: &'a str,
}

// A Linux close with unread peer data can reset the connection and erase the
// refusal at the client. Finish writing first, then discard a bounded prefix
// of the remaining input without parsing it or waiting past the error window.
fn finish_error(mut stream: &TcpStream, until: Instant) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let mut scratch = [0u8; 4096];
    let mut left = MAX_HEAD + MAX_BODY;
    while left != 0 {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
            return;
        }
        let chunk = left.min(scratch.len());
        match stream.read(&mut scratch[..chunk]) {
            Ok(0) | Err(_) => return,
            Ok(read) => left -= read,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, MAX_BODY, parse_head};

    #[test]
    fn batch_json_respects_both_the_old_and_the_batch_reply_extents() {
        use std::io::{ErrorKind, Write};

        use sage_db_store::lease::AttemptLedger;
        use sage_db_types::query::RunToCompletion;

        let payload = "x".repeat(super::MAX_RESPONSE);
        let mut ledger =
            AttemptLedger::open(super::MAX_BATCH_RESPONSE as u64, 16, &RunToCompletion).unwrap();
        let mut batch = super::batch_response_buffer(&mut ledger).ok().unwrap();
        let rendered = super::render_prepaid(&mut batch, &mut ledger, |out| {
            serde_json::to_writer(out, &payload).map_err(std::io::Error::from)
        })
        .unwrap();
        assert_eq!(rendered.len(), super::MAX_RESPONSE + 2);
        assert_eq!(rendered.first(), Some(&b'"'));
        assert_eq!(rendered.last(), Some(&b'"'));

        let mut ledger =
            AttemptLedger::open(super::MAX_BATCH_RESPONSE as u64, 16, &RunToCompletion).unwrap();
        let mut single = [0; super::MAX_RESPONSE];
        let write = |out: &mut dyn Write| {
            serde_json::to_writer(out, &payload).map_err(std::io::Error::from)
        };
        assert_eq!(
            super::render(&mut single, &mut ledger, write)
                .unwrap_err()
                .kind(),
            ErrorKind::WriteZero
        );
        let oversized = vec![b'x'; super::MAX_BATCH_RESPONSE + 1];
        assert_eq!(
            super::render_prepaid(&mut batch, &mut ledger, |out| out.write_all(&oversized))
                .unwrap_err()
                .kind(),
            ErrorKind::WriteZero
        );
        let mut short =
            AttemptLedger::open((super::MAX_BATCH_RESPONSE - 1) as u64, 16, &RunToCompletion)
                .unwrap();
        assert_eq!(
            super::batch_response_buffer(&mut short).err().unwrap().code,
            sage_db_protocol::generated::ErrorCode::Budget
        );
    }

    fn parse(raw: &[u8]) -> Result<super::Head<'_>, Failure> {
        let end = raw.windows(4).position(|part| part == b"\r\n\r\n").unwrap() + 4;
        parse_head(raw, end)
    }

    fn request(headers: &str, body: &str) -> String {
        format!(
            "POST /sage.v1.SageCommandService/Handshake HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer example\r\nContent-Type: application/json\r\n{headers}\r\n{body}"
        )
    }

    #[test]
    fn concept_body_is_a_read_route_with_one_slug() {
        let get = b"GET /api/concepts/risk/body?kb=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer example\r\n\r\n";
        assert!(matches!(
            parse(get).ok().unwrap().route,
            super::Route::ConceptBody
        ));
        let post = b"POST /api/concepts/risk/body HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer example\r\n\r\n";
        assert_eq!(parse(post).err().unwrap().status, "405 Method Not Allowed");
        let nested = b"GET /api/concepts/risk/extra/body HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer example\r\n\r\n";
        assert!(parse(nested).is_err());
    }

    #[test]
    fn frontier_binary_request_preserves_other_body_and_media_limits() {
        let length = MAX_BODY + 4096;
        let raw = format!(
            "PUT /db/v1/frontier-document HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer example\r\nContent-Type: application/octet-stream\r\nContent-Length: {length}\r\n\r\n"
        );
        let frontier = parse(raw.as_bytes()).ok().unwrap();
        assert!(matches!(
            frontier.route,
            super::Route::WriteFrontierDocument
        ));
        assert_eq!(frontier.len, length);

        let too_long = crate::frontier_document::MAX_REQUEST + 1;
        let oversized = raw.replace(&length.to_string(), &too_long.to_string());
        assert_eq!(
            parse(oversized.as_bytes()).err().unwrap().status,
            "413 Content Too Large"
        );
        let wrong_media = raw.replace("application/octet-stream", "application/json");
        assert_eq!(
            parse(wrong_media.as_bytes()).err().unwrap().status,
            "415 Unsupported Media Type"
        );

        let protected = request(&format!("Content-Length: {length}\r\n"), "");
        assert_eq!(
            parse(protected.as_bytes()).err().unwrap().status,
            "413 Content Too Large"
        );
        let without_auth = raw.replace("Authorization: Bearer example\r\n", "");
        assert_eq!(
            parse(without_auth.as_bytes()).err().unwrap().status,
            "401 Unauthorized"
        );
    }

    #[test]
    fn frontier_reply_writes_length_and_raw_bytes_beyond_json_response_limit() {
        use std::io::Read;
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let data = vec![0xa5; MAX_BODY + 1];
            super::respond_octets(
                &socket,
                "200 OK",
                &[&[0x42; 25], &data],
                &super::Deadline::new(),
            );
        });
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        let header_end = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let header = std::str::from_utf8(&response[..header_end]).unwrap();
        assert!(header.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(header.contains("Content-Type: application/octet-stream\r\n"));
        assert!(header.contains(&format!("Content-Length: {}\r\n", MAX_BODY + 26)));
        let body = &response[header_end..];
        assert_eq!(&body[..25], &[0x42; 25]);
        assert_eq!(body.len(), MAX_BODY + 26);
        assert!(body[25..].iter().all(|byte| *byte == 0xa5));
        server.join().unwrap();
    }

    #[test]
    fn header_carries_only_the_declared_prefix() {
        let raw = request("Content-Length: 4\r\n", "ab");
        let head = parse(raw.as_bytes()).ok().unwrap();
        assert_eq!(head.bearer, b"example");
        assert_eq!(head.len, 4);
        assert_eq!(head.prefix, b"ab");
    }

    #[test]
    fn refusal_reaches_client_when_the_body_is_still_unread() {
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::mpsc;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (head_read, wait_for_head) = mpsc::channel();
        let (body_sent, wait_for_body) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut storage = [0; super::MAX_HEAD];
            let head = super::read_head(&stream, &mut storage, &super::Deadline::new())
                .ok()
                .unwrap();
            assert!(head.prefix.is_empty());
            head_read.send(()).unwrap();
            wait_for_body.recv_timeout(Duration::from_secs(2)).unwrap();
            super::reply_error(&stream, Some(head.route), Failure::unauthorized());
        });

        client
            .write_all(request("Content-Length: 4\r\n", "").as_bytes())
            .unwrap();
        wait_for_head.recv_timeout(Duration::from_secs(2)).unwrap();
        client.write_all(b"body").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        body_sent.send(()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(response.contains("ERROR_CODE_UNAUTHENTICATED"));
        server.join().unwrap();
    }

    #[test]
    fn prepaid_render_uses_admitted_space_and_refuses_the_next_byte() {
        use sage_db_store::lease::AttemptLedger;
        use sage_db_types::ledger::SharedLedger;
        use sage_db_types::query::RunToCompletion;

        let mut ledger = AttemptLedger::open(4, 16, &RunToCompletion).unwrap();
        ledger.try_charge(4).unwrap();
        let mut scratch = [0; 4];
        let exact = super::render_in(&mut scratch, &mut ledger, true, |out| {
            out.write_all(b"1234")
        });
        assert_eq!(exact.unwrap(), b"1234");
        let excess = super::render_in(&mut scratch, &mut ledger, true, |out| {
            out.write_all(b"12345")
        });
        assert_eq!(excess.unwrap_err().kind(), std::io::ErrorKind::WriteZero);
    }

    #[test]
    fn framing_refuses_ambiguous_or_unbounded_requests() {
        for headers in [
            "Content-Length: 1\r\ncontent-length: 1\r\n",
            "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n",
            "Content-Length: 1\r\nExpect: 100-continue\r\n",
            "Content-Length: +1\r\n",
            "Content-Length: 1\r\nAuthorization: Bearer duplicate\r\n",
        ] {
            assert!(parse(request(headers, "").as_bytes()).is_err());
        }
        assert!(parse(request("Content-Length: 1\r\n", "too long").as_bytes()).is_err());
        let oversized = format!("Content-Length: {}\r\n", MAX_BODY + 1);
        assert!(parse(request(&oversized, "").as_bytes()).is_err());
    }
}
