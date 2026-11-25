//! Request lifetime and dispatch for the first network slice.

use std::fs::File;
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::time::{SystemTime, UNIX_EPOCH};

use sage_db_kernel::auth::{AuthError, Capability, Grants};
use sage_db_kernel::claims::{CommandError, InputError};
use sage_db_kernel::concept::ConceptTextError;
use sage_db_kernel::concept_intent::MAX_ATOM_UTF16_UNITS;
use sage_db_protocol::generated as wire;
use sage_db_protocol::ingress::{self, IngressError, IngressLimits};
use sage_db_protocol::validation::{ValidationError, ValidationLimits, validate_envelope};
use sage_db_server::commands::{self, CommandContext, CommandRefusal, ExecutionMeta};
use sage_db_server::credentials::{CredentialVerifier, VerifyError};
use sage_db_server::responses::{
    self, OutcomeResponse, ResponseError, ResponseLimits, WriteFamily,
};
use sage_db_store::commit::CommitRefusal;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleRefusal, Store};
use sage_db_store::lease::{AttemptLedger, LeaseLedger};
use sage_db_store::open::OutcomeLookup;
use sage_db_store::typed::MAX_ATOM_BYTES;
use sage_db_store::view::{BlobReadError, CommitReadError, ImportReadiness, RecordReadError};
use sage_db_types::budget::Budget;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::persist::IdSpace;
use serde::Serialize;
use wire::{CommitPayload, Envelope, ErrorCode, ReviewPayload, SubmitPayload};

use crate::http::{self, Deadline, Failure, Head, Route};

const INGRESS_LIMITS: IngressLimits =
    IngressLimits::new(http::MAX_BODY as u64, http::MAX_BODY as u64);
const RESPONSE_LIMITS: ResponseLimits = ResponseLimits {
    max_result_bytes: http::MAX_RESPONSE as u64,
    limits: Limits::V1,
    max_history_events: 10_000,
};
// Frontier revision pins its entry as well as every new concept reference.
const MAX_READ_SET: u64 = sage_db_kernel::frontier_intent::MAX_REFS as u64 + 1;
const REQUEST_BYTES: u64 = 128 * 1024 * 1024;
const REQUEST_ENTRIES: u64 = 10_000_000;

pub(crate) struct Service<'a, F: FileSystem> {
    store: Store<F>,
    verifier: CredentialVerifier<'a>,
    random: File,
    source_identity: &'static str,
}

impl<'a, F: FileSystem> Service<'a, F> {
    pub(crate) fn new(
        store: Store<F>,
        verifier: CredentialVerifier<'a>,
        random: File,
        source_identity: &'static str,
    ) -> Self {
        Self {
            store,
            verifier,
            random,
            source_identity,
        }
    }

    pub(crate) fn serve(mut self, listener: TcpListener) -> io::Result<()> {
        for accepted in listener.incoming() {
            match accepted {
                Ok(stream) => self.connection(&stream),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn connection(&mut self, stream: &TcpStream) {
        let deadline = Deadline::new();
        let mut headers = [0; http::MAX_HEAD];
        let head = match http::read_head(stream, &mut headers, &deadline) {
            Ok(head) => head,
            Err(error) => {
                http::reply_error(stream, None, error);
                return;
            }
        };
        let Ok(mut ledger) = AttemptLedger::open(REQUEST_BYTES, REQUEST_ENTRIES, &deadline) else {
            http::reply_error(stream, Some(head.route), Failure::internal());
            return;
        };
        let exchange = Exchange {
            stream,
            head,
            deadline: &deadline,
        };
        if let Err(error) = self.dispatch(&exchange, &mut ledger) {
            if exchange.head.route.legacy() {
                http::reply_legacy_error(
                    stream,
                    error,
                    error.message(),
                    crate::activation::wants_text(&exchange.head),
                );
            } else {
                http::reply_error(stream, Some(exchange.head.route), error);
            }
        }
    }

    fn dispatch(
        &mut self,
        exchange: &Exchange<'_>,
        ledger: &mut AttemptLedger<'_>,
    ) -> Result<(), Failure> {
        if matches!(exchange.head.route, Route::Health) {
            return crate::operator_read::health(
                &self.store,
                self.source_identity,
                exchange.stream,
                exchange.deadline,
                ledger,
            );
        }
        // Authenticate before decoding attacker-controlled content. The write adapter still
        // independently verifies its own authorization inputs before consulting the store.
        let grants = self
            .verifier
            .verify_bearer(exchange.head.bearer, self.store.kb_uuid(), ledger)
            .map_err(verify_error)?;
        match exchange.head.route {
            Route::ReadCompatCheckpoint | Route::WriteCompatCheckpoint => {
                return crate::compat_checkpoint::serve(
                    &mut self.store,
                    &grants,
                    &mut self.random,
                    exchange.stream,
                    &exchange.head,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::ReadFrontierDocument | Route::WriteFrontierDocument => {
                return crate::frontier_document::serve(
                    &mut self.store,
                    grants,
                    &mut self.random,
                    exchange.stream,
                    &exchange.head,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::Activate => {
                return crate::activation::serve(
                    &mut self.store,
                    &grants,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::Edges | Route::Path => {
                return crate::legacy_query::serve(
                    &mut self.store,
                    &grants,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::Manifest | Route::ManifestRebuild | Route::Search => {
                return crate::manifest_search::serve(
                    &self.store,
                    &grants,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::Graph | Route::GraphRebuild | Route::Viz => {
                return crate::graph_viz::serve(
                    &mut self.store,
                    &grants,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                );
            }
            Route::ConceptBody
            | Route::Catalog
            | Route::Meta
            | Route::Validate
            | Route::Revision
            | Route::Frontier
            | Route::NativeFrontier
            | Route::NativeCheckpoint
            | Route::LegacyCheckpoint
            | Route::Proposals
            | Route::KnowledgeProposals
            | Route::Proposal => {
                return crate::operator_read::serve(
                    &mut self.store,
                    &grants,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                );
            }
            _ => {}
        }
        if matches!(exchange.head.route, Route::CommitDirectConcept) {
            grants
                .authorize(self.store.kb_uuid(), Capability::WriteConcept, ledger)
                .map_err(auth_error)?;
        }
        if matches!(exchange.head.route, Route::SubmitLargeBodyProposal) {
            grants
                .authorize(self.store.kb_uuid(), Capability::Propose, ledger)
                .map_err(auth_error)?;
        }
        let body = http::read_body(exchange.stream, &exchange.head, exchange.deadline, ledger)?;
        match exchange.head.route {
            Route::ReadFrontierDocument
            | Route::WriteFrontierDocument
            | Route::ReadCompatCheckpoint
            | Route::WriteCompatCheckpoint
            | Route::Activate
            | Route::ConceptBody
            | Route::Edges
            | Route::Path
            | Route::Manifest
            | Route::ManifestRebuild
            | Route::Search
            | Route::Graph
            | Route::GraphRebuild
            | Route::Viz
            | Route::Health
            | Route::Catalog
            | Route::Meta
            | Route::Validate
            | Route::Revision
            | Route::Frontier
            | Route::NativeFrontier
            | Route::NativeCheckpoint
            | Route::LegacyCheckpoint
            | Route::Proposals
            | Route::KnowledgeProposals
            | Route::Proposal => {
                unreachable!("legacy projections have no request body")
            }
            Route::Handshake => {
                let request = ingress::decode_handshake_request(&body, INGRESS_LIMITS, ledger)
                    .map_err(ingress_error)?;
                let envelope = request.dto().envelope.as_ref();
                read_envelope(envelope, exchange.deadline, grants, ledger)?;
                let basis = self
                    .store
                    .commit_basis(IdSpace::TermId, ledger)
                    .map_err(basis_error)?;
                let mut capabilities = [""; 5];
                let mut count = 0;
                for capability in [
                    Capability::Read,
                    Capability::Propose,
                    Capability::Review,
                    Capability::WriteAux,
                    Capability::WriteConcept,
                ] {
                    if grants.capabilities().contains(capability) {
                        capabilities[count] = capability.as_str();
                        count += 1;
                    }
                }
                let reply = HandshakeReply {
                    profile: basis.rules.profile.as_str(),
                    source_identity: self.source_identity,
                    capabilities: &capabilities[..count],
                    limits: wire::Limits {
                        max_payload_bytes: http::MAX_BODY as u64,
                        max_refs: MAX_READ_SET,
                        max_atom_bytes: u64::from(MAX_ATOM_BYTES),
                        max_atom_utf16_units: Some(MAX_ATOM_UTF16_UNITS as u64),
                    },
                };
                exchange.send(&reply, ledger);
            }
            Route::ReadTermText | Route::ReadTermDocument | Route::ReadTermActivation => {
                let request = ingress::decode_read_term_text_request(&body, INGRESS_LIMITS, ledger)
                    .map_err(ingress_error)?;
                let dto = request.dto();
                read_envelope(dto.envelope.as_ref(), exchange.deadline, grants, ledger)?;
                crate::activation::serve_term_text(
                    &mut self.store,
                    &grants,
                    &dto.slug,
                    &exchange.head,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                )?;
            }
            Route::ReadGraphSnapshot => {
                let request =
                    ingress::decode_read_graph_snapshot_request(&body, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                read_envelope(
                    request.dto().envelope.as_ref(),
                    exchange.deadline,
                    grants,
                    ledger,
                )?;
                crate::db_graph::serve(
                    &mut self.store,
                    &grants,
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                )?;
            }
            Route::ReadProposalFiles => {
                let request =
                    ingress::decode_read_proposal_files_request(&body, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                read_envelope(
                    request.dto().envelope.as_ref(),
                    exchange.deadline,
                    grants,
                    ledger,
                )?;
                crate::proposal_files::serve(
                    &mut self.store,
                    &grants,
                    exchange.head.accept == Some(b"application/octet-stream"),
                    exchange.stream,
                    exchange.deadline,
                    ledger,
                )?;
            }
            Route::Submit => {
                let request =
                    ingress::decode_submit_proposal_request(&body, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                let dto = request.dto();
                let write = WriteRequest::Submit(dto.payload.as_ref().ok_or_else(missing_payload)?);
                let envelope = dto.envelope.as_ref().ok_or_else(missing_envelope)?;
                self.write(exchange, envelope, write, ledger)?;
            }
            Route::SubmitLargeBodyProposal => {
                let (metadata, proposed) = raw_document_parts(
                    &body,
                    sage_db_kernel::legacy_proposal::MAX_COMPAT_PROPOSED_BODY_BYTES,
                )?;
                let request =
                    ingress::decode_submit_proposal_request(metadata, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                let dto = request.dto();
                let envelope = dto.envelope.as_ref().ok_or_else(missing_envelope)?;
                let payload = dto.payload.as_ref().ok_or_else(missing_payload)?;
                self.write(
                    exchange,
                    envelope,
                    WriteRequest::SubmitLargeBody(payload, proposed),
                    ledger,
                )?;
            }
            Route::Review => {
                let request =
                    ingress::decode_review_proposal_request(&body, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                let dto = request.dto();
                let write = WriteRequest::Review(dto.payload.as_ref().ok_or_else(missing_payload)?);
                let envelope = dto.envelope.as_ref().ok_or_else(missing_envelope)?;
                self.write(exchange, envelope, write, ledger)?;
            }
            Route::Commit => {
                let request = ingress::decode_commit_command_request(&body, INGRESS_LIMITS, ledger)
                    .map_err(ingress_error)?;
                let dto = request.dto();
                let write = WriteRequest::Commit(dto.payload.as_ref().ok_or_else(missing_payload)?);
                let envelope = dto.envelope.as_ref().ok_or_else(missing_envelope)?;
                self.write(exchange, envelope, write, ledger)?;
            }
            Route::CommitDirectConcept => {
                let (metadata, document) =
                    raw_document_parts(&body, sage_db_kernel::term_create::MAX_DIRECT_BODY_BYTES)?;
                let request =
                    ingress::decode_commit_command_request(metadata, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                let dto = request.dto();
                let envelope = dto.envelope.as_ref().ok_or_else(missing_envelope)?;
                let payload = dto.payload.as_ref().ok_or_else(missing_payload)?;
                self.write(
                    exchange,
                    envelope,
                    WriteRequest::DirectConcept(payload, document),
                    ledger,
                )?;
            }
            Route::Outcome => {
                let request =
                    ingress::decode_get_request_outcome_request(&body, INGRESS_LIMITS, ledger)
                        .map_err(ingress_error)?;
                let envelope = read_envelope(
                    request.dto().envelope.as_ref(),
                    exchange.deadline,
                    grants,
                    ledger,
                )?;
                let scope = grants
                    .authorize(self.store.kb_uuid(), Capability::Read, ledger)
                    .map_err(auth_error)?;
                let lookup = match self.store.import_readiness(ledger) {
                    Ok(ImportReadiness::Ready { .. }) => {
                        self.store.outcome(scope.principal(), &envelope.request_id)
                    }
                    Ok(ImportReadiness::Incomplete { .. }) => {
                        return Err(Failure::unavailable("import is incomplete"));
                    }
                    Err(CommitReadError::Incomplete(reason)) => OutcomeLookup::Incomplete(reason),
                    Err(other) => return Err(basis_error(other)),
                };
                // This lookup accepts only a request id and therefore has no method-family
                // input. The persisted result body is authoritative. OriginalStatus is optional.
                let reply = responses::assemble_outcome(
                    &mut self.store,
                    lookup,
                    WriteFamily::Commit,
                    &RESPONSE_LIMITS,
                    ledger,
                )
                .map_err(response_error)?;
                let wire = match reply.response() {
                    OutcomeResponse::Found(value) => OutcomeReply::Found(value),
                    OutcomeResponse::Absent(value) => OutcomeReply::Absent(value),
                    OutcomeResponse::Incomplete(value) => OutcomeReply::Incomplete(value),
                };
                exchange.send(&wire, ledger);
            }
        }
        Ok(())
    }

    fn write(
        &mut self,
        exchange: &Exchange<'_>,
        envelope: &Envelope,
        write: WriteRequest<'_>,
        ledger: &mut dyn LeaseLedger,
    ) -> Result<(), Failure> {
        if envelope.deadline_ms != Some(0) {
            exchange.deadline.tighten(envelope.deadline_ms);
        }
        if u64::try_from(envelope.read_set.len()).map_or(true, |len| len > MAX_READ_SET) {
            return Err(Failure::bad("too many revision pairs"));
        }
        let mut tx_id = [0; 16];
        self.random
            .read_exact(&mut tx_id)
            .map_err(|_| Failure::internal())?;
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_micros()).ok())
            .ok_or_else(Failure::internal)?;
        let mut context = CommandContext {
            store: &mut self.store,
            verifier: &self.verifier,
            bearer: exchange.head.bearer,
            execution: ExecutionMeta { tx_id, created_at },
            limits: Limits::V1,
            validation: ValidationLimits::DEFAULT,
            max_existing_body_len: http::MAX_BODY as u64,
        };
        let (family, outcome) = match write {
            WriteRequest::Submit(payload) => (
                WriteFamily::Submit,
                commands::submit(&mut context, envelope, payload, ledger),
            ),
            WriteRequest::SubmitLargeBody(payload, body) => (
                WriteFamily::Submit,
                commands::submit_large_body(&mut context, envelope, payload, body, ledger),
            ),
            WriteRequest::Review(payload) => (
                WriteFamily::Review,
                commands::review_with_validation(
                    &mut context,
                    envelope,
                    payload,
                    ledger,
                    Budget::DEFAULT,
                    exchange.deadline,
                ),
            ),
            WriteRequest::Commit(payload) => (
                WriteFamily::Commit,
                commands::commit_with_validation(
                    &mut context,
                    envelope,
                    payload,
                    ledger,
                    Budget::DEFAULT,
                    exchange.deadline,
                ),
            ),
            WriteRequest::DirectConcept(payload, body) => (
                WriteFamily::Commit,
                commands::commit_direct_with_body(
                    &mut context,
                    envelope,
                    payload,
                    body,
                    ledger,
                    Budget::DEFAULT,
                    exchange.deadline,
                ),
            ),
        };
        let outcome = outcome.map_err(command_error)?;
        // Once the writer returned an outcome, a later assembly or socket refusal
        // must not be presented as a rejection of the original attempt. The client
        // can look up its original request identity with a fresh deadline.
        if let Ok(reply) = responses::assemble_write(
            &mut self.store,
            &outcome.outcome,
            family,
            &envelope.request_id,
            &RESPONSE_LIMITS,
            ledger,
        ) {
            exchange.send(
                &WriteReply {
                    outcome: reply.outcome(),
                },
                ledger,
            );
        }
        Ok(())
    }
}

struct Exchange<'a> {
    stream: &'a TcpStream,
    head: Head<'a>,
    deadline: &'a Deadline,
}

impl Exchange<'_> {
    fn send(&self, value: &impl Serialize, ledger: &mut dyn LeaseLedger) {
        let _ = http::respond(self.stream, value, self.deadline, ledger);
    }
}

// These borrowed projections have the envelope fields of the generated responses.
// They keep the existing assembled response owner and its credits live through send.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HandshakeReply<'a> {
    profile: &'static str,
    source_identity: &'static str,
    capabilities: &'a [&'static str],
    limits: wire::Limits,
}

#[derive(Serialize)]
struct WriteReply<'a> {
    outcome: &'a wire::WriteOutcome,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum OutcomeReply<'a> {
    Found(&'a wire::OutcomeFound),
    Absent(&'a wire::OutcomeAbsent),
    Incomplete(&'a wire::OutcomeIncomplete),
}

enum WriteRequest<'a> {
    Submit(&'a SubmitPayload),
    SubmitLargeBody(&'a SubmitPayload, &'a str),
    Review(&'a ReviewPayload),
    Commit(&'a CommitPayload),
    DirectConcept(&'a CommitPayload, &'a str),
}

fn raw_document_parts(body: &[u8], max_document: usize) -> Result<(&[u8], &str), Failure> {
    let (prefix, tail) = body
        .split_at_checked(http::DIRECT_CONCEPT_PREFIX)
        .ok_or_else(|| Failure::bad("incomplete document command frame"))?;
    let size = u32::from_le_bytes(prefix.try_into().map_err(|_| Failure::internal())?) as usize;
    if size > http::MAX_BODY {
        return Err(Failure::bad(
            "document command metadata exceeds its ceiling",
        ));
    }
    let (metadata, document) = tail
        .split_at_checked(size)
        .ok_or_else(|| Failure::bad("incomplete document command metadata"))?;
    if document.len() > max_document {
        return Err(Failure::bad("document command body exceeds its ceiling"));
    }
    let document = std::str::from_utf8(document)
        .map_err(|_| Failure::bad("document command body is not UTF-8"))?;
    Ok((metadata, document))
}

fn missing_payload() -> Failure {
    Failure::bad("payload is required")
}

fn missing_envelope() -> Failure {
    Failure::bad("envelope is required")
}

fn read_envelope<'a>(
    envelope: Option<&'a Envelope>,
    deadline: &Deadline,
    grants: Grants<'_>,
    ledger: &mut dyn LeaseLedger,
) -> Result<&'a Envelope, Failure> {
    let envelope = envelope.ok_or_else(missing_envelope)?;
    if envelope.deadline_ms != Some(0) {
        deadline.tighten(envelope.deadline_ms);
    }
    validate_envelope(ledger, &ValidationLimits::DEFAULT, envelope).map_err(validation_error)?;
    if envelope.expect_version.is_some() || !envelope.read_set.is_empty() {
        return Err(Failure::unsupported(
            "this method does not accept a read set or version expectation",
        ));
    }
    if crate::hex(&envelope.kb_uuid) != Some(*grants.kb().as_bytes()) {
        return Err(Failure::unauthorized());
    }
    Ok(envelope)
}

pub(crate) fn ingress_error(error: IngressError) -> Failure {
    if let Some(refusal) = error.0.refusal {
        return Failure::refused(refusal);
    }
    match error.0.class {
        ingress::IngressClass::BodyCeiling | ingress::IngressClass::OwnedCeiling => Failure::new(
            "413 Content Too Large",
            ErrorCode::Budget,
            "JSON admission ceiling exceeded",
        ),
        ingress::IngressClass::AllocatorFailed
        | ingress::IngressClass::ExtentOverflow
        | ingress::IngressClass::UnexpectedCapacity
        | ingress::IngressClass::BorrowConflict
        | ingress::IngressClass::Refused => Failure::internal(),
        _ => Failure::bad("invalid JSON request"),
    }
}

fn verify_error(error: VerifyError) -> Failure {
    match error {
        VerifyError::Refused(refusal) => Failure::refused(refusal),
        _ => Failure::unauthorized(),
    }
}

pub(crate) fn auth_error(error: AuthError) -> Failure {
    match error {
        AuthError::Refused(refusal) => Failure::refused(refusal),
        AuthError::MissingCapability { .. } => Failure::new(
            "403 Forbidden",
            ErrorCode::PermissionDenied,
            "capability not granted",
        ),
        AuthError::Unauthenticated | AuthError::WrongKb { .. } => Failure::unauthorized(),
    }
}

fn validation_error(error: ValidationError) -> Failure {
    match error {
        ValidationError::Refused(refusal) => Failure::refused(refusal),
        ValidationError::Malformed { .. } => Failure::bad("invalid protocol request"),
        ValidationError::Limit { .. } => Failure::new(
            "413 Content Too Large",
            ErrorCode::Budget,
            "protocol ceiling exceeded",
        ),
        ValidationError::InvalidConfig { .. } => Failure::internal(),
    }
}

fn basis_error(error: CommitReadError) -> Failure {
    match error {
        CommitReadError::Refused(refusal) => Failure::refused(refusal),
        CommitReadError::Incomplete(_) => Failure::unavailable("no definitive store basis"),
        _ => Failure::internal(),
    }
}

pub(crate) fn blob_error(error: BlobReadError) -> Failure {
    match error {
        BlobReadError::Refused(refusal) => Failure::refused(refusal),
        BlobReadError::Corrupt(_) => Failure::new(
            "503 Service Unavailable",
            ErrorCode::Corrupt,
            "committed blob is corrupt",
        ),
        BlobReadError::Incomplete(_) => Failure::unavailable("no definitive store basis"),
        BlobReadError::Ceiling { .. } => Failure::new(
            "413 Content Too Large",
            ErrorCode::Budget,
            "committed body exceeds its read ceiling",
        ),
        _ => Failure::internal(),
    }
}

fn response_error(error: ResponseError) -> Failure {
    match error {
        ResponseError::Refused(refusal) => Failure::refused(refusal),
        ResponseError::Blob(error) => blob_error(error),
        _ => Failure::internal(),
    }
}

fn conflict(code: ErrorCode, message: &'static str) -> Failure {
    Failure::new("409 Conflict", code, message)
}

fn idempotency_mismatch() -> Failure {
    conflict(
        ErrorCode::IdempotencyMismatch,
        "request id already has different content",
    )
}

fn command_error(error: CommandRefusal) -> Failure {
    match error {
        CommandRefusal::Unauthenticated(error) => verify_error(error),
        CommandRefusal::Refused(refusal) => Failure::refused(refusal),
        CommandRefusal::Invalid(error) => validation_error(error),
        CommandRefusal::KbMismatch { .. } => Failure::unauthorized(),
        CommandRefusal::UnsupportedExpectedVersion { .. }
        | CommandRefusal::UnsupportedShape { .. } => {
            Failure::unsupported("unsupported command shape or version expectation")
        }
        CommandRefusal::MalformedField { .. } => Failure::bad("invalid command field"),
        CommandRefusal::Concept(error) => match error {
            ConceptTextError::Command(error) => kernel_error(error),
            ConceptTextError::Input(_) => Failure::bad("concept text refused"),
            ConceptTextError::Internal(_) => Failure::internal(),
        },
        CommandRefusal::Claims(error) => kernel_error(error),
        CommandRefusal::DirectConcept(sage_db_kernel::term_create::CreateError::SlugExists) => {
            conflict(
                ErrorCode::ConceptSlugExists,
                "The concept slug already exists",
            )
        }
        CommandRefusal::TermCreate(error) | CommandRefusal::DirectConcept(error) => {
            use sage_db_kernel::term_create::CreateError;
            match error {
                CreateError::Command(error) => kernel_error(error),
                CreateError::Validation(sage_db_types::query::QueryError::Stopped(stop)) => {
                    Failure::refused(Refusal::Stopped(stop))
                }
                CreateError::Validation(sage_db_types::query::QueryError::BudgetExceeded(_)) => {
                    crate::activation::limit("candidate validation exceeded its work ceiling")
                }
                CreateError::InvalidCandidate(_) => Failure::new(
                    "422 Unprocessable Content",
                    ErrorCode::InvalidArgument,
                    "Term violates the committed graph rules",
                ),
                CreateError::SlugExists => Failure::new(
                    "409 Conflict",
                    ErrorCode::Conflict,
                    "The concept slug already exists",
                ),
                CreateError::Validation(_) | CreateError::StoredPayload => Failure::internal(),
                CreateError::IdExhausted(_) | CreateError::IdCollision(_) => Failure::new(
                    "409 Conflict",
                    ErrorCode::Conflict,
                    "A new record identity is unavailable",
                ),
                CreateError::Field(field) => Failure::bad(field),
                CreateError::Text(_) => Failure::bad("Term creation text or review note refused"),
            }
        }
        CommandRefusal::Relation(error) => match error {
            sage_db_kernel::relations::RelationAssertError::Command(error) => kernel_error(error),
            sage_db_kernel::relations::RelationAssertError::Validation(
                sage_db_types::QueryError::BudgetExceeded(_),
            ) => crate::activation::limit("candidate validation work exceeded its ceiling"),
            sage_db_kernel::relations::RelationAssertError::Validation(
                sage_db_types::QueryError::Stopped(stop),
            ) => Failure::refused(Refusal::Stopped(stop)),
            sage_db_kernel::relations::RelationAssertError::Input(
                sage_db_kernel::relations::RelationAssertInputError::InvalidCandidate { .. },
            ) => Failure::new(
                "422 Unprocessable Content",
                ErrorCode::InvalidArgument,
                "relation violates the committed graph rules",
            ),
            sage_db_kernel::relations::RelationAssertError::Input(
                sage_db_kernel::relations::RelationAssertInputError::StaleEndpoint { .. }
                | sage_db_kernel::relations::RelationAssertInputError::StaleAssertion(_),
            ) => conflict(ErrorCode::Conflict, "relation endpoint baseline changed"),
            sage_db_kernel::relations::RelationAssertError::Input(
                sage_db_kernel::relations::RelationAssertInputError::TargetSlugUnresolved,
            ) => Failure::new(
                "422 Unprocessable Content",
                ErrorCode::LegacyProposalTargetUnresolved,
                "legacy proposal target slug is still unresolved",
            ),
            sage_db_kernel::relations::RelationAssertError::Input(
                sage_db_kernel::relations::RelationAssertInputError::TargetSlugAlreadyExists,
            ) => conflict(
                ErrorCode::Conflict,
                "legacy proposal target slug already exists",
            ),
            sage_db_kernel::relations::RelationAssertError::Input(_) => {
                Failure::bad("relation assertion input refused")
            }
            sage_db_kernel::relations::RelationAssertError::Validation(_) => Failure::internal(),
        },
        CommandRefusal::Session(error) => match error {
            sage_db_kernel::session::SessionError::Command(error) => kernel_error(error),
            sage_db_kernel::session::SessionError::Input(_) => {
                Failure::bad("session checkpoint input refused")
            }
            sage_db_kernel::session::SessionError::Internal(_) => Failure::internal(),
        },
        CommandRefusal::Frontier(error) => match error {
            sage_db_kernel::frontier::FrontierError::Command(error) => kernel_error(error),
            sage_db_kernel::frontier::FrontierError::Input(_) => {
                Failure::bad("frontier input refused")
            }
            sage_db_kernel::frontier::FrontierError::Internal(_) => Failure::internal(),
        },
        _ => Failure::internal(),
    }
}

pub(crate) fn kernel_error(error: CommandError) -> Failure {
    match error {
        CommandError::Auth(error) => auth_error(error),
        CommandError::Refused(refusal) => Failure::refused(refusal),
        CommandError::IdempotencyMismatch => idempotency_mismatch(),
        CommandError::Input(InputError::LegacyProposalAtomMismatch) => conflict(
            ErrorCode::LegacyProposalAtomMismatch,
            "quoted atom no longer matches",
        ),
        CommandError::Input(InputError::LegacyProposalBodyMismatch { .. }) => conflict(
            ErrorCode::LegacyProposalBodyMismatch,
            "quoted body revision no longer matches",
        ),
        CommandError::Input(InputError::LegacyProposalAliasMoved) => {
            conflict(ErrorCode::Conflict, "effective proposal filename moved")
        }
        CommandError::Input(
            InputError::StaleBaseline { .. } | InputError::ProposalNotPending(_),
        ) => conflict(ErrorCode::Conflict, "proposal or record baseline changed"),
        CommandError::Input(_) => Failure::bad("command input refused"),
        CommandError::Fenced | CommandError::Incomplete(_) => {
            Failure::unavailable("store cannot accept another write")
        }
        CommandError::ImportInProgress => Failure::unavailable("import is incomplete"),
        CommandError::Basis(error) => basis_error(error),
        CommandError::Blob(error) => blob_error(error),
        CommandError::Record(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
        CommandError::Store(HandleRefusal::IdempotencyMismatch) => idempotency_mismatch(),
        CommandError::Store(HandleRefusal::ExpectVersionMismatch { .. }) => {
            conflict(ErrorCode::Conflict, "database version changed")
        }
        CommandError::Store(HandleRefusal::Fenced) => {
            Failure::unavailable("store cannot accept another write")
        }
        CommandError::Store(HandleRefusal::Derivation(CommitRefusal::Budget)) => Failure::new(
            "429 Too Many Requests",
            ErrorCode::Budget,
            "write budget was refused",
        ),
        CommandError::Store(HandleRefusal::Derivation(CommitRefusal::Stopped)) => {
            // This listener's only control stop is its request deadline.
            Failure::refused(Refusal::Stopped(
                sage_db_types::query::ControlStop::DeadlineExceeded,
            ))
        }
        CommandError::Store(HandleRefusal::Derivation(CommitRefusal::Refused(refusal))) => {
            Failure::refused(refusal)
        }
        _ => Failure::internal(),
    }
}
