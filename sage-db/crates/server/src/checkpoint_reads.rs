//! Authorized session-state reads by concept and session number.

use std::{
    io::{self, Write},
    net::TcpStream,
};

use sage_db_kernel::{
    auth::Grants,
    session::{SessionError, SessionRead, read_session_checkpoint},
};
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::{
    fold::FoldedRecord,
    fs::FileSystem,
    handle::Store,
    lease::AttemptLedger,
    view::{RecordReadError, RecordScanError, ViewBasis},
};
use sage_db_types::{framing::Refusal, identity::KbUuid, ledger::SharedLedger, persist::RecordKey};
use serde::Serialize;

use crate::{
    activation::{
        Format, JSON, TEXT, active_slug, decode_component, format, parameter, parts, render_error,
    },
    http::{self, Deadline, Failure, Head, Route},
    operator_read::Decimal,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Checkpoint<'a> {
    concept_id: &'a str,
    checkpoint_id: Decimal<u128>,
    revision: Decimal<u64>,
    kb_uuid: Decimal<KbUuid>,
    session: u32,
    content: &'a str,
}

pub(crate) fn serve(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    basis: ViewBasis,
    head: &Head<'_>,
    socket: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    let (path, query) = parts(head);
    let legacy = matches!(head.route, Route::LegacyCheckpoint);
    let prefix = if legacy {
        "/api/checkpoints/"
    } else {
        "/api/knowledge/checkpoints/"
    };
    let raw = path.strip_prefix(prefix).ok_or_else(Failure::internal)?;
    let mut slug_buffer = [0; 64];
    let slug = decode_component(raw, &mut slug_buffer, false)?;
    if legacy {
        let mut bytes = slug.bytes();
        if !bytes
            .next()
            .is_some_and(|value| value.is_ascii_lowercase() || value.is_ascii_digit())
            || !bytes
                .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == b'-')
        {
            return Err(Failure::bad("Invalid checkpoint concept id"));
        }
    }
    let missing = || {
        Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Session checkpoint not found",
        )
    };
    let wanted = parameter(query, "session")
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|number| {
                    *number > 0
                        && !value.starts_with('0')
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                })
                .ok_or_else(|| Failure::bad("session must be a positive decimal integer"))
        })
        .transpose()?;
    let (term, observed) = active_slug(store, grants, slug, ledger)?.ok_or_else(missing)?;
    if observed != basis {
        return Err(Failure::internal());
    }
    let mut selected: Option<(u32, RecordKey)> = None;
    let scanned = store
        .scan_current_records(ledger, |record, _| {
            if let FoldedRecord::SessionCheckpoint(checkpoint) = record
                && let sage_db_types::persist::CheckpointLocation::Term {
                    concept,
                    session_no,
                } = checkpoint.location
                && concept == term
                && wanted.is_none_or(|number| session_no == number)
            {
                let candidate = (session_no, checkpoint.key);
                if selected.is_none_or(|previous| candidate > previous) {
                    selected = Some(candidate);
                }
            }
            Ok::<(), Failure>(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(error) => error,
            RecordScanError::Read(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
            RecordScanError::Read(_) => Failure::internal(),
        })?;
    if scanned != basis {
        return Err(Failure::internal());
    }
    let (session, key) = selected.ok_or_else(missing)?;
    let read = read_session_checkpoint(store, grants, key, http::MAX_BODY as u64, ledger).map_err(
        |error| match error {
            SessionError::Command(error) => crate::runtime::kernel_error(error),
            _ => Failure::internal(),
        },
    )?;
    if read.basis() != basis {
        return Err(Failure::internal());
    }
    let SessionRead::Found { record, content } = read else {
        return Err(Failure::internal());
    };
    let FoldedRecord::SessionCheckpoint(checkpoint) = record.record() else {
        return Err(Failure::internal());
    };
    if checkpoint.key != key
        || checkpoint.location
            != (sage_db_types::persist::CheckpointLocation::Term {
                concept: term,
                session_no: session,
            })
    {
        return Err(Failure::internal());
    }
    if legacy {
        sage_db_protocol::ingress::validate_legacy_checkpoint(
            content.as_bytes(),
            slug,
            session,
            http::MAX_BODY as u64,
            ledger,
        )
        .map_err(|error| {
            use sage_db_protocol::ingress::IngressClass;
            if matches!(error.0.class, IngressClass::Syntax | IngressClass::Data) {
                Failure::new(
                    "422 Unprocessable Content",
                    ErrorCode::Unsupported,
                    "Checkpoint is available only as native content",
                )
            } else {
                crate::runtime::ingress_error(error)
            }
        })?;
        let text = format(head) == Format::Text;
        let extent = u64::try_from(content.len()).map_err(|_| Failure::internal())?;
        ledger
            .try_charge_entries(extent)
            .map_err(|error| Failure::refused(Refusal::Budget(error)))?;
        return http::respond_with(
            socket,
            "200 OK",
            if text { TEXT } else { JSON },
            deadline,
            ledger,
            |out| {
                if text {
                    let document = std::str::from_utf8(content.as_bytes())
                        .map_err(|_| io::ErrorKind::InvalidData)?;
                    out.write_all(document.trim().as_bytes())?;
                } else {
                    compact_json(content.as_bytes(), out)?;
                }
                out.write_all(b"\n")
            },
        )
        .map_err(render_error);
    }
    let result = Checkpoint {
        concept_id: slug,
        checkpoint_id: Decimal(key.id()),
        session,
        kb_uuid: Decimal(basis.kb_uuid),
        revision: Decimal(checkpoint.revision.get()),
        content: std::str::from_utf8(content.as_bytes()).map_err(|_| Failure::internal())?,
    };
    http::respond_with(socket, "200 OK", JSON, deadline, ledger, |out| {
        serde_json::to_writer(&mut *out, &result)?;
        out.write_all(b"\n")
    })
    .map_err(render_error)
}

// The schema and lexical grammar were already validated by the admitted parser.
// Preserve the exact string bytes while removing only JSON whitespace outside literals.
fn compact_json(document: &[u8], out: &mut dyn Write) -> io::Result<()> {
    let mut in_string = false;
    let mut escaped = false;
    for block in document.chunks(1024) {
        let mut first = 0;
        for (index, &byte) in block.iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
            } else if byte == b'"' {
                in_string = true;
            } else if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                out.write_all(&block[first..index])?;
                first = index + 1;
            }
        }
        // A fully skipped block still visits the bounded writer's checkpoint.
        if first == block.len() {
            let _ = out.write(&[])?;
        } else {
            out.write_all(&block[first..])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_projection_preserves_escaped_quotes_and_whitespace_across_blocks() {
        let long = "x".repeat(1017);
        let input = format!("{{ \"a\": \"{long} \\\"foo bar\", \"b\": [] }}");
        let result = format!("{{\"a\":\"{long} \\\"foo bar\",\"b\":[]}}");
        let mut out = Vec::new();
        compact_json(input.as_bytes(), &mut out).unwrap();
        assert_eq!(out, result.as_bytes());
    }
}
