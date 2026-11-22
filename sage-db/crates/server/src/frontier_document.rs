//! Private binary framing for the exact legacy Frontier document.

use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::claims::RequestMeta;
use sage_db_kernel::frontier_document::{
    FrontierDocumentError, read_frontier_document, replace_frontier_document,
};
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::view::KbMetadataReadError;
use sage_db_types::framing::Limits;
use sage_db_types::identity::RecordRevision;

use crate::http::{self, Deadline, Failure, Head, Route};

const MAX_DOCUMENT: usize = 16 * 1024 * 1024;
const WRITE_PREFIX: usize = 16 + 8 + 32;
pub(crate) const MAX_REQUEST: usize = MAX_DOCUMENT + WRITE_PREFIX;
const READ_PREFIX: usize = 16 + 8 + 1;

pub(crate) fn serve(
    store: &mut Store<impl FileSystem>,
    grants: Grants<'_>,
    random: &mut File,
    stream: &TcpStream,
    head: &Head<'_>,
    deadline: &Deadline,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), Failure> {
    if matches!(head.route, Route::ReadFrontierDocument) {
        let read =
            read_frontier_document(store, &grants, MAX_DOCUMENT as u64, ledger).map_err(refusal)?;
        let mut prefix = [0; READ_PREFIX];
        prefix[..16].copy_from_slice(read.basis.kb_uuid.as_bytes());
        prefix[16..24].copy_from_slice(&read.revision.get().to_le_bytes());
        prefix[24] = u8::from(read.raw.is_some());
        let bytes = read.raw.as_ref().map_or(&[][..], |blob| blob.as_bytes());
        http::respond_octets(stream, "200 OK", &[&prefix, bytes], deadline);
        return Ok(());
    }

    // The caller must hold the actual KB's WriteAux before the server admits the large frame.
    grants
        .authorize(store.kb_uuid(), Capability::WriteAux, ledger)
        .map_err(crate::runtime::auth_error)?;
    let bytes = http::read_body(stream, head, deadline, ledger)?;
    let (prefix, document) = bytes
        .split_at_checked(WRITE_PREFIX)
        .ok_or_else(|| Failure::bad("incomplete Frontier replacement frame"))?;
    if &prefix[..16] != store.kb_uuid().as_bytes() {
        return Err(Failure::unauthorized());
    }
    let version: [u8; 8] = prefix[16..24].try_into().map_err(|_| Failure::internal())?;
    let expected = RecordRevision::new(u64::from_le_bytes(version))
        .ok_or_else(|| Failure::bad("Frontier replacement revision must be positive"))?;
    let request_id = &prefix[24..];
    if !request_id
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(Failure::bad(
            "Frontier replacement request identity is invalid",
        ));
    }
    let request_id = std::str::from_utf8(request_id).map_err(|_| Failure::internal())?;
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
    match replace_frontier_document(
        store,
        &grants,
        meta,
        expected,
        document,
        &Limits::V1,
        ledger,
    )
    .map_err(refusal)?
    {
        HandleOutcome::Committed { .. } | HandleOutcome::AlreadyCommitted { .. } => {
            http::respond_octets(stream, "204 No Content", &[], deadline);
            Ok(())
        }
        HandleOutcome::Unknown => Err(Failure::unavailable(
            "Frontier replacement outcome is unknown",
        )),
        HandleOutcome::NotCommitted { .. } => Err(Failure::internal()),
    }
}

fn refusal(error: FrontierDocumentError) -> Failure {
    match error {
        FrontierDocumentError::Command(error) => crate::runtime::kernel_error(error),
        FrontierDocumentError::Metadata(KbMetadataReadError::Incomplete(_)) => {
            Failure::unavailable("Frontier metadata is not readable")
        }
        FrontierDocumentError::Metadata(KbMetadataReadError::Refused(refusal)) => {
            Failure::refused(refusal)
        }
        FrontierDocumentError::Metadata(_) => Failure::internal(),
        FrontierDocumentError::Stale { .. } => Failure::new(
            "412 Precondition Failed",
            ErrorCode::Conflict,
            "Frontier document revision changed",
        ),
    }
}
