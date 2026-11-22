//! Private raw transport for checkpoint state opaque to the DBMS.

use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::claims::RequestMeta;
use sage_db_kernel::session::{
    LegacyCheckpointWrite, SessionError, SessionInputError, SessionRead,
    read_legacy_checkpoint_slot, read_session_checkpoint, upsert_legacy_checkpoint,
};
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_types::framing::Limits;

use crate::http::{self, Deadline, Failure, Head, Route};

const MAX_DOCUMENT: usize = 256 * 1024;
const COMMON_PREFIX: usize = 16 + 8 + 2;
const WRITE_EXTRA: usize = 8 + 32;
pub(crate) const MAX_INSPECT: usize = COMMON_PREFIX + http::MAX_HEAD + 1;
pub(crate) const MAX_WRITE: usize = MAX_INSPECT - 1 + WRITE_EXTRA + MAX_DOCUMENT;

pub(crate) fn serve(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    random: &mut File,
    stream: &TcpStream,
    head: &Head<'_>,
    deadline: &Deadline,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), Failure> {
    let is_write = matches!(head.route, Route::WriteCompatCheckpoint);
    grants
        .authorize(
            store.kb_uuid(),
            if is_write {
                Capability::WriteAux
            } else {
                Capability::Read
            },
            ledger,
        )
        .map_err(crate::runtime::auth_error)?;
    let body = http::read_body(stream, head, deadline, ledger)?;
    let (prefix, rest) = body.split_at_checked(COMMON_PREFIX).ok_or_else(invalid)?;
    if &prefix[..16] != store.kb_uuid().as_bytes() {
        return Err(Failure::unauthorized());
    }
    let ordinal = take_ordinal(&prefix[16..24])?;
    let slug_len = usize::from(u16::from_le_bytes([prefix[24], prefix[25]]));
    if slug_len > http::MAX_HEAD {
        return Err(invalid());
    }
    let (slug, rest) = rest.split_at_checked(slug_len).ok_or_else(invalid)?;
    let slug = std::str::from_utf8(slug).map_err(|_| invalid())?;

    if !is_write {
        let [contents] = rest else {
            return Err(invalid());
        };
        if *contents > 1 {
            return Err(invalid());
        }
        let selected = read_legacy_checkpoint_slot(
            store,
            grants,
            slug,
            (ordinal != 0).then_some(ordinal),
            ledger,
        )
        .map_err(refusal)?;
        let mut prefix = [0; 32];
        prefix[..16].copy_from_slice(selected.basis.kb_uuid.as_bytes());
        prefix[16..24].copy_from_slice(&selected.basis.version.get().to_le_bytes());
        if let Some((session, key)) = selected.selected {
            prefix[24..32].copy_from_slice(&session.to_le_bytes());
            if *contents == 1 {
                let result =
                    read_session_checkpoint(store, grants, key, MAX_DOCUMENT as u64, ledger)
                        .map_err(refusal)?;
                if result.basis() != selected.basis {
                    return Err(Failure::internal());
                }
                let SessionRead::Found { content, .. } = result else {
                    return Err(Failure::internal());
                };
                http::respond_octets(stream, "200 OK", &[&prefix, content.as_bytes()], deadline);
                return Ok(());
            }
        }
        http::respond_octets(stream, "200 OK", &[&prefix], deadline);
        return Ok(());
    }

    let (write, content) = rest.split_at_checked(WRITE_EXTRA).ok_or_else(invalid)?;
    if content.len() > MAX_DOCUMENT {
        return Err(Failure::new(
            "413 Content Too Large",
            ErrorCode::Budget,
            "checkpoint document exceeds its ceiling",
        ));
    }
    let expected_version = take_ordinal(&write[..8])?;
    let request = &write[8..];
    if !request
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid());
    }
    let request_id = std::str::from_utf8(request).map_err(|_| invalid())?;
    let mut tx_id = [0; 16];
    random
        .read_exact(&mut tx_id)
        .map_err(|_| Failure::internal())?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_micros()).ok())
        .ok_or_else(Failure::internal)?;
    let meta = RequestMeta {
        request_id,
        tx_id,
        created_at,
    };
    match upsert_legacy_checkpoint(
        store,
        grants,
        meta,
        LegacyCheckpointWrite {
            slug,
            session_no: ordinal,
            content,
            expected_version,
        },
        &Limits::V1,
        ledger,
    )
    .map_err(refusal)?
    {
        HandleOutcome::Committed { .. } | HandleOutcome::AlreadyCommitted { .. } => {
            http::respond_octets(stream, "204 No Content", &[], deadline);
            Ok(())
        }
        HandleOutcome::Unknown => Err(Failure::unavailable("checkpoint commit outcome is unknown")),
        HandleOutcome::NotCommitted { .. } => Err(Failure::internal()),
    }
}

fn invalid() -> Failure {
    Failure::bad("invalid checkpoint private frame")
}

fn take_ordinal(bytes: &[u8]) -> Result<u64, Failure> {
    let bytes = bytes.try_into().map_err(|_| invalid())?;
    Ok(u64::from_le_bytes(bytes))
}

fn refusal(error: SessionError) -> Failure {
    match error {
        SessionError::Command(error) => crate::runtime::kernel_error(error),
        SessionError::Input(SessionInputError::LegacyVersionChanged { .. }) => Failure::new(
            "412 Precondition Failed",
            ErrorCode::Conflict,
            "checkpoint basis changed",
        ),
        SessionError::Input(SessionInputError::InvalidLegacyLocation) => invalid(),
        _ => Failure::internal(),
    }
}
