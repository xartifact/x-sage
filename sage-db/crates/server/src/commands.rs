//! The authenticated command application adapter.
//!
//! # What this is
//!
//! One layer between generated protocol DTOs and the **existing** kernel command set. It does
//! three things and nothing else:
//!
//! 1. **Authenticates** a bearer token through
//!    [`CredentialVerifier::verify_bearer`](crate::credentials::CredentialVerifier::verify_bearer)
//!    against the actual [`Store`]'s KB, on the caller's own ledger. The returned
//!    [`Grants`] is the only source of principal;
//! 2. **Validates** the protocol envelope and payload through the protocol crate's paired entry
//!    points, on that **same** ledger, and cross-checks the envelope's KB against the target
//!    store;
//! 3. **Dispatches** to the canonical kernel command for the selected operation.
//!
//! # What this is deliberately not
//!
//! No HTTP, RPC, queue, response assembly, or protocol codec of its own. The kernel owns
//! request framing, request digests, idempotency and outcome replay; this adapter never
//! recomputes a digest or replays an outcome. A [`HandleOutcome`] is returned **unchanged** — a
//! future response assembler reads the canonical result blob from it.
//!
//! # The eight commands
//!
//! | protocol | kernel |
//! |---|---|
//! | `SubmitPayload.claim_edit` | [`claims::submit_claim_edit`] |
//! | `SubmitPayload.concept_text` | [`concept::submit_concept_text`] |
//! | `ReviewPayload.claim_edit` | [`claims::review_claim_edit`] |
//! | `ReviewPayload.concept_text` | [`concept::review_concept_text`] |
//! | `CommitPayload.create_session_checkpoint` | [`session::create_session_checkpoint`] |
//! | `CommitPayload.revise_session_checkpoint` | [`session::revise_session_checkpoint`] |
//! | `CommitPayload.create_frontier` | [`frontier::create_frontier`] |
//! | `CommitPayload.revise_frontier` | [`frontier::revise_frontier`] |
//!
//! # Three rules this file exists to enforce
//!
//! * **The principal comes only from verified credentials.** `declared_author` in a payload is
//!   attribution and stays attribution; nothing in the request can set the audit principal.
//! * **`expect_version` is refused, never ignored.** No kernel command in this set carries a
//!   whole-KB version expectation, so honouring one would silently change the request's
//!   identity. It is refused explicitly, before any mutation.
//! * **Review dispatches on the wire family, before any store read.** Distinct families hash
//!   different operation kinds, so the family must be known before the kernel's outcome-first
//!   lookup runs. A wrong family against a stored proposal is the kernel's own typed refusal.
//!
//! # Deadline
//!
//! The envelope's `deadline_ms` is **not** turned into a timer here. This library has no clock
//! and no cancellation source; the outer transport must tighten the caller's deadline and
//! supply the actual control hook through [`sage_db_types::query::QueryControl`]. Nothing in
//! this module claims to enforce a deadline.

use crate::credentials::{CredentialVerifier, VerifyError};
use sage_db_kernel::auth::Grants;
use sage_db_kernel::claims::{self, RequestMeta};
use sage_db_kernel::concept;
use sage_db_kernel::concept_intent::{self, ConceptTextEdit};
use sage_db_kernel::frontier;
use sage_db_kernel::frontier_intent::FrontierState;
use sage_db_kernel::intent::{ClaimEdit, ReviewDecision};
use sage_db_kernel::legacy_proposal::{
    BodyObservation, LegacyDirectory, LegacyProposal, LegacyProposalFile, LegacyReview,
};
use sage_db_kernel::session;
use sage_db_kernel::session_intent::SessionState;
use sage_db_kernel::{relations, term_create};
use sage_db_protocol::generated::{
    CommitPayload, ConceptText, Decision, Envelope, FrontierState as WireFrontierState,
    RecordKey as WireKey, ReviewPayload, SessionCheckpointState, SubmitPayload, commit_payload,
    review_payload, submit_payload,
};
use sage_db_protocol::validation::{
    ValidatedRequest, ValidationError, ValidationLimits, validate_commit_request,
    validate_review_request, validate_submit_request,
};
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_types::budget::Budget;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::persist::{RecordKey, RecordKind, RevPair};
use sage_db_types::query::QueryControl;

/// The transaction identity the **server executor** supplies.
///
/// `tx_id` and `created_at` are transaction-header facts, not caller statements: they never
/// appear in the request payload, and a client cannot choose either. `request_id` is *not* here
/// because it belongs to the authenticated envelope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecutionMeta {
    /// This transaction's 16-byte identity, which must equal the `tid` in its file name.
    pub tx_id: [u8; 16],
    /// Commit time, UTC microseconds, from the executor's clock.
    pub created_at: i64,
}

/// Which write command a request selected.
///
/// Derived from the payload's oneof **before** any store access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteCommand {
    /// A claim-text edit submission.
    SubmitClaimEdit,
    /// A concept-text edit submission.
    SubmitConceptText,
    /// A claim-edit review.
    ReviewClaimEdit,
    /// A concept-text review.
    ReviewConceptText,
    /// An authored relation submission.
    SubmitRelationAssert,
    /// A relation replacement pinning an existing support.
    SubmitRelationReplace,
    /// A compatibility edge proposal.
    SubmitLegacyRelation,
    /// A compatibility atom or body proposal.
    SubmitLegacyConcept,
    /// Review of an authored relation submission.
    ReviewRelationAssert,
    /// Merge the effective compatibility edge filename.
    MergeLegacyRelation,
    /// Merge the effective compatibility atom or body filename.
    MergeLegacyConcept,
    /// Submit a new Term for review.
    SubmitTermCreate,
    /// Decide a pending Term creation.
    ReviewTermCreate,
    /// Atomically create a Term and its initial assertions.
    DirectConceptCreate,
    /// Create a session checkpoint.
    CreateSessionCheckpoint,
    /// Revise a session checkpoint.
    ReviseSessionCheckpoint,
    /// Create a frontier entry.
    CreateFrontier,
    /// Revise a frontier entry.
    ReviseFrontier,
}

/// Why a command was refused instead of returning a kernel outcome.
///
/// Each variant keeps the family that produced it, so a caller can tell an authentication
/// refusal from a protocol-syntax refusal from a kernel verdict. Nothing is flattened into a
/// generic "invalid argument", and no version is ever invented.
#[derive(Debug)]
pub enum CommandRefusal {
    /// The bearer token was not accepted. Opaque by construction: the auth layer yields one
    /// class for an unbound KB, a wrong token, an empty token and a revoked binding alike.
    Unauthenticated(VerifyError),
    /// Authentication could not be *attempted*: the caller's own ledger refused budget, or a
    /// cancellation/deadline stop arrived.
    ///
    /// Distinct from [`Self::Unauthenticated`] on purpose. Reporting a genuine budget or stop
    /// refusal as an authentication failure would tell the caller to fix their credentials
    /// when the real answer is "no capacity" or "cancelled", and would hide a real
    /// [`sage_db_types::ledger::BudgetError`] behind an opaque class.
    Refused(Refusal),
    /// The protocol envelope or payload failed its borrowed syntax gate.
    Invalid(ValidationError),
    /// The envelope named a different KB than the store this call targets.
    KbMismatch {
        /// The KB the authenticated store serves.
        expected: KbUuid,
        /// The KB the envelope declared.
        declared: KbUuid,
    },
    /// The envelope carried a whole-KB version expectation, which these commands cannot honour.
    ///
    /// The pinned value is carried as a scalar, not a copy of the request's string: a refusal
    /// path must not allocate, and the caller already holds the exact text it sent.
    UnsupportedExpectedVersion {
        /// The version the caller pinned, once validated as a canonical decimal.
        expected: u64,
    },
    /// A protocol value could not be converted into its canonical form.
    MalformedField {
        /// The field, as the proto names it.
        field: &'static str,
    },
    /// The payload's oneof names no operation this adapter implements.
    UnsupportedShape {
        /// Which message carried the empty or unknown selection.
        payload: &'static str,
    },
    /// The canonical read-set allocation did not report the admitted extent.
    ReadSetExtent,
    /// Initial relation slots exceed their exact admitted extent.
    InitialRelationExtent,
    /// The kernel refused with a typed claim verdict.
    Claims(claims::CommandError),
    /// The kernel refused with a typed concept-text verdict.
    Concept(concept::ConceptTextError),
    /// The kernel refused with a typed relation-assertion verdict.
    Relation(relations::RelationAssertError),
    /// Term creation proposal verdict.
    TermCreate(term_create::CreateError),
    /// Refusal from the direct concept transaction.
    DirectConcept(term_create::CreateError),
    /// The kernel refused with a typed session verdict.
    Session(session::SessionError),
    /// The kernel refused with a typed frontier verdict.
    Frontier(frontier::FrontierError),
}

impl From<ValidationError> for CommandRefusal {
    fn from(error: ValidationError) -> Self {
        Self::Invalid(error)
    }
}

impl From<VerifyError> for CommandRefusal {
    fn from(error: VerifyError) -> Self {
        Self::Unauthenticated(error)
    }
}

/// The result of one accepted command.
///
/// The outcome is the kernel's own value, untouched: a commit, an already-committed retry, or a
/// genuine ambiguity. No local success enum re-describes it, because a second description is a
/// second authority for what a commit means.
#[derive(Debug)]
pub struct CommandOutcome {
    /// Which command ran.
    pub command: WriteCommand,
    /// The kernel's exact result.
    pub outcome: HandleOutcome,
}

/// Everything one request needs besides its own bytes.
pub struct CommandContext<'a, F: FileSystem> {
    /// The store this request targets. Writes are serialised by this exclusive borrow.
    pub store: &'a mut Store<F>,
    /// The deployment's credential table.
    pub verifier: &'a CredentialVerifier<'a>,
    /// The bearer token exactly as presented; never logged, never stored.
    pub bearer: &'a [u8],
    /// Transaction identity from the server executor.
    pub execution: ExecutionMeta,
    /// The kernel's own canonical bounds.
    pub limits: Limits,
    /// The protocol's borrowed-syntax ceilings.
    pub validation: ValidationLimits,
    /// The executor's policy ceiling for re-reading an unchanged committed body.
    ///
    /// Passed to concept review after idempotency inside the kernel, and never part of the
    /// request's identity.
    pub max_existing_body_len: u64,
}

/// Dispatch one submission.
///
/// # Errors
///
/// [`CommandRefusal`] for an authentication failure, a protocol-syntax failure, a KB mismatch,
/// an unsupported version expectation, a field conversion failure, an unsupported oneof shape,
/// or a typed kernel refusal. A genuine `Unknown` is **not** an error: it returns inside
/// [`CommandOutcome`].
pub fn submit<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &SubmitPayload,
    ledger: &mut dyn LeaseLedger,
) -> Result<CommandOutcome, CommandRefusal> {
    submit_core(context, envelope, payload, None, ledger)
}

/// Submit an authenticated legacy body edit with UTF-8 supplied separately from typed metadata.
///
/// # Errors
/// Malformed authorization, envelope, body or proposal fields, or the canonical writer refusal.
pub fn submit_large_body<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &SubmitPayload,
    body: &str,
    ledger: &mut dyn LeaseLedger,
) -> Result<CommandOutcome, CommandRefusal> {
    submit_core(context, envelope, payload, Some(body), ledger)
}

fn submit_core<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &SubmitPayload,
    raw_body: Option<&str>,
    ledger: &mut dyn LeaseLedger,
) -> Result<CommandOutcome, CommandRefusal> {
    let kb = context.store.kb_uuid();
    let grants = authenticate(context.verifier, context.bearer, kb, ledger)?;
    if raw_body.is_some() && !matches!(payload.kind, Some(submit_payload::Kind::LegacyConcept(_))) {
        return Err(CommandRefusal::MalformedField {
            field: "legacy_concept.edit",
        });
    }
    if !matches!(
        payload.kind,
        Some(
            submit_payload::Kind::LegacyRelation(_)
                | submit_payload::Kind::LegacyConcept(_)
                | submit_payload::Kind::LegacyRelationBySlug(_)
        )
    ) {
        refuse_expected_version(envelope)?;
    }

    let validated = validate_submit_request(ledger, &context.validation, envelope, payload)?;
    let envelope = validated.envelope().message();
    check_envelope_kb(kb, envelope)?;

    let read_set = read_set_of(envelope, ledger)?;
    let meta = request_meta(envelope, context.execution);
    let family = payload
        .kind
        .as_ref()
        .ok_or(CommandRefusal::UnsupportedShape {
            payload: "SubmitPayload",
        })?;

    let (command, outcome) = match family {
        submit_payload::Kind::ClaimEdit(edit) => {
            let subject = proto_key("claim_edit.subject", edit.subject.as_ref())?;
            let claim = proto_key("claim_edit.claim", edit.claim.as_ref())?;
            (
                WriteCommand::SubmitClaimEdit,
                claims::submit_claim_edit(
                    context.store,
                    &grants,
                    meta,
                    ClaimEdit {
                        subject,
                        claim,
                        // Borrowed, never copied: the request bytes stay the caller's.
                        text: &edit.text,
                        declared_author: &edit.declared_author,
                    },
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Claims)?,
            )
        }
        submit_payload::Kind::RelationAssert(edge) => (
            WriteCommand::SubmitRelationAssert,
            relations::submit_relation_assert(
                context.store,
                &grants,
                meta,
                relation_draft(edge)?,
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Relation)?,
        ),
        submit_payload::Kind::RelationReplace(edge) => (
            WriteCommand::SubmitRelationReplace,
            relations::submit_relation_replace(
                context.store,
                &grants,
                meta,
                relation_draft(edge)?,
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Relation)?,
        ),
        submit_payload::Kind::LegacyRelation(legacy) => {
            let edge = legacy.edge.as_ref().ok_or(CommandRefusal::MalformedField {
                field: "legacy_relation.edge",
            })?;
            let metadata = LegacyProposal {
                file: LegacyProposalFile::Edge {
                    filename: &legacy.filename,
                },
                observed_version: required_expect_version(envelope)?,
            };
            (
                WriteCommand::SubmitLegacyRelation,
                relations::submit_legacy_relation_assert(
                    context.store,
                    &grants,
                    meta,
                    relation_draft(edge)?,
                    metadata,
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Relation)?,
            )
        }
        submit_payload::Kind::LegacyRelationBySlug(edge) => (
            WriteCommand::SubmitLegacyRelation,
            relations::submit_legacy_relation_by_slug(
                context.store,
                &grants,
                meta,
                relations::RelationAssert {
                    from: proto_key("legacy_relation_by_slug.from", edge.from.as_ref())?,
                    target: edge.target_slug.as_str(),
                    relation: narrow(
                        "legacy_relation_by_slug.relation",
                        edge.relation.ok_or(CommandRefusal::MalformedField {
                            field: "legacy_relation_by_slug.relation",
                        })?,
                    )?,
                    confidence: narrow(
                        "legacy_relation_by_slug.confidence",
                        edge.confidence.ok_or(CommandRefusal::MalformedField {
                            field: "legacy_relation_by_slug.confidence",
                        })?,
                    )?,
                    declaration_order: edge.declaration_order,
                    declared_author: &edge.declared_author,
                    reason: &edge.reason,
                },
                LegacyProposal {
                    file: LegacyProposalFile::Edge {
                        filename: &edge.filename,
                    },
                    observed_version: required_expect_version(envelope)?,
                },
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Relation)?,
        ),
        submit_payload::Kind::LegacyConcept(legacy) => {
            let edit = legacy.edit.as_ref().ok_or(CommandRefusal::MalformedField {
                field: "legacy_concept.edit",
            })?;
            let file = match (
                legacy.current_atom.as_deref(),
                legacy.public_revision.as_deref(),
            ) {
                (Some(current_atom), None) => LegacyProposalFile::Atom {
                    filename: &legacy.filename,
                    current_atom,
                },
                (None, Some(hash)) => LegacyProposalFile::Body {
                    filename: &legacy.filename,
                    expected_revision: public_digest(hash)?,
                },
                _ => {
                    return Err(CommandRefusal::MalformedField {
                        field: "legacy_concept.baseline",
                    });
                }
            };
            let mut draft = concept_draft(edit)?;
            if let Some(body) = raw_body {
                if !matches!(file, LegacyProposalFile::Body { .. })
                    || draft.body != Some("")
                    || draft.atom.is_some()
                {
                    return Err(CommandRefusal::MalformedField {
                        field: "legacy_concept.edit.body",
                    });
                }
                draft.body = Some(body);
            }
            (
                WriteCommand::SubmitLegacyConcept,
                concept::submit_legacy_concept_text(
                    context.store,
                    &grants,
                    meta,
                    draft,
                    edit.reason.as_deref().unwrap_or_default(),
                    LegacyProposal {
                        file,
                        observed_version: required_expect_version(envelope)?,
                    },
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Concept)?,
            )
        }
        submit_payload::Kind::TermCreate(term) => (
            WriteCommand::SubmitTermCreate,
            term_create::submit(
                context.store,
                &grants,
                meta,
                term_draft(term)?,
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::TermCreate)?,
        ),
        submit_payload::Kind::ConceptText(text) => (
            WriteCommand::SubmitConceptText,
            concept::submit_concept_text_with_reason(
                context.store,
                &grants,
                meta,
                concept_draft(text)?,
                text.reason.as_deref(),
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Concept)?,
        ),
    };

    Ok(CommandOutcome { command, outcome })
}

/// Dispatch one review decision.
///
/// The wire family is read **first**, before the store is touched, because distinct families
/// hash different operation kinds and the kernel's outcome-first lookup depends on knowing
/// which operation this is.
///
/// # Errors
///
/// As [`submit`], plus the requirement that a review envelope carry **exactly one** read pair
/// naming the proposal.
pub fn review<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &ReviewPayload,
    ledger: &mut dyn LeaseLedger,
) -> Result<CommandOutcome, CommandRefusal> {
    review_core(context, envelope, payload, ledger, None)
}

/// Dispatch a review with the explicit budget and stop control required by relation validation.
///
/// # Errors
///
/// Preserves the usual review refusals and the relation validator's budget and stop refusals.
pub fn review_with_validation<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &ReviewPayload,
    ledger: &mut dyn LeaseLedger,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<CommandOutcome, CommandRefusal> {
    review_core(context, envelope, payload, ledger, Some((budget, control)))
}

fn review_core<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &ReviewPayload,
    ledger: &mut dyn LeaseLedger,
    graph_validation: Option<(Budget, &dyn QueryControl)>,
) -> Result<CommandOutcome, CommandRefusal> {
    let kb = context.store.kb_uuid();
    let grants = authenticate(context.verifier, context.bearer, kb, ledger)?;
    if !matches!(
        payload.family,
        Some(review_payload::Family::LegacyRelation(_) | review_payload::Family::LegacyConcept(_))
    ) {
        refuse_expected_version(envelope)?;
    }

    // A wire fact, read before any stored record is consulted.
    let family = payload
        .family
        .as_ref()
        .ok_or(CommandRefusal::UnsupportedShape {
            payload: "ReviewPayload",
        })?;
    let (body, command, compatibility) = match family {
        review_payload::Family::ClaimEdit(body) => (body, WriteCommand::ReviewClaimEdit, None),
        review_payload::Family::ConceptText(body) => (body, WriteCommand::ReviewConceptText, None),
        review_payload::Family::RelationAssert(body) => {
            (body, WriteCommand::ReviewRelationAssert, None)
        }
        review_payload::Family::TermCreate(body) => (body, WriteCommand::ReviewTermCreate, None),
        review_payload::Family::LegacyRelation(review) => (
            review
                .action
                .as_ref()
                .ok_or(CommandRefusal::MalformedField {
                    field: "legacy_relation_review.action",
                })?,
            WriteCommand::MergeLegacyRelation,
            Some(review),
        ),
        review_payload::Family::LegacyConcept(review) => (
            review
                .action
                .as_ref()
                .ok_or(CommandRefusal::MalformedField {
                    field: "legacy_concept_review.action",
                })?,
            WriteCommand::MergeLegacyConcept,
            Some(review),
        ),
    };

    let validated = validate_review_request(ledger, &context.validation, envelope, payload)?;
    let envelope = validated.envelope().message();
    check_envelope_kb(kb, envelope)?;

    let read_set = read_set_of(envelope, ledger)?;
    let (proposal, revision) = single_proposal_pair(&read_set)?;
    if proto_key("review.proposal", body.proposal.as_ref())? != proposal {
        return Err(CommandRefusal::MalformedField {
            field: "review.proposal",
        });
    }
    let meta = request_meta(envelope, context.execution);

    let outcome = match command {
        WriteCommand::ReviewClaimEdit => claims::review_claim_edit(
            context.store,
            &grants,
            meta,
            proposal,
            revision,
            proto_decision(body.decision)?,
            &body.note,
            &context.limits,
            ledger,
        )
        .map_err(CommandRefusal::Claims)?,
        WriteCommand::ReviewConceptText => concept::review_concept_text(
            context.store,
            &grants,
            meta,
            proposal,
            revision,
            proto_decision_wire(body.decision)?,
            &body.note,
            // After idempotency inside the kernel, and not part of request identity.
            context.max_existing_body_len,
            &context.limits,
            ledger,
        )
        .map_err(CommandRefusal::Concept)?,
        WriteCommand::ReviewTermCreate => {
            let (budget, control) = graph_validation.ok_or(CommandRefusal::UnsupportedShape {
                payload: "Term creation review requires validation budget and control",
            })?;
            term_create::review(
                context.store,
                &grants,
                meta,
                proposal,
                revision,
                proto_decision_wire(body.decision)?,
                &body.note,
                budget,
                control,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::TermCreate)?
        }
        WriteCommand::ReviewRelationAssert => {
            let (budget, control) = graph_validation.ok_or(CommandRefusal::UnsupportedShape {
                payload: "relation review requires validation budget and control",
            })?;
            let decision = match proto_decision(body.decision)? {
                ReviewDecision::Approve => relations::DECISION_APPROVE,
                ReviewDecision::Reject => relations::DECISION_REJECT,
            };
            relations::review_relation_assert(
                context.store,
                &grants,
                meta,
                proposal,
                revision,
                decision,
                &body.note,
                budget,
                control,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Relation)?
        }
        WriteCommand::MergeLegacyRelation => {
            let review = compatibility.ok_or(CommandRefusal::MalformedField {
                field: "legacy_relation_review",
            })?;
            if review.public_revision.is_some() {
                return Err(CommandRefusal::MalformedField {
                    field: "legacy_relation_review.public_revision",
                });
            }
            let (budget, control) = graph_validation.ok_or(CommandRefusal::UnsupportedShape {
                payload: "legacy relation merge requires validation budget and control",
            })?;
            relations::merge_legacy_relation_assert(
                context.store,
                &grants,
                meta,
                read_set[0],
                LegacyReview {
                    directory: LegacyDirectory::Edges,
                    filename: &review.filename,
                    observed_version: required_expect_version(envelope)?,
                },
                &body.note,
                budget,
                control,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Relation)?
        }
        WriteCommand::MergeLegacyConcept => {
            let review = compatibility.ok_or(CommandRefusal::MalformedField {
                field: "legacy_concept_review",
            })?;
            let observation = review
                .public_revision
                .as_deref()
                .map(public_digest)
                .transpose()?
                .map(|public_revision| BodyObservation { public_revision });
            let directory = if observation.is_some() {
                LegacyDirectory::Bodies
            } else {
                LegacyDirectory::Atoms
            };
            concept::merge_legacy_concept_text(
                context.store,
                &grants,
                meta,
                read_set[0],
                LegacyReview {
                    directory,
                    filename: &review.filename,
                    observed_version: required_expect_version(envelope)?,
                },
                observation,
                &body.note,
                context.max_existing_body_len,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Concept)?
        }
        // The family match above binds only review commands.
        _ => {
            return Err(CommandRefusal::UnsupportedShape {
                payload: "ReviewPayload",
            });
        }
    };

    Ok(CommandOutcome { command, outcome })
}

/// Dispatch one commit command.
///
/// # Errors
///
/// As [`submit`], with the per-family key and state conversions.
pub fn commit<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &CommitPayload,
    ledger: &mut dyn LeaseLedger,
) -> Result<CommandOutcome, CommandRefusal> {
    commit_core(context, envelope, payload, ledger, None, None)
}

/// Dispatch a commit with the budget and stop control needed by direct concept creation.
///
/// # Errors
/// Preserves ordinary commit refusals and candidate graph validation refusals.
pub fn commit_with_validation<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &CommitPayload,
    ledger: &mut dyn LeaseLedger,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<CommandOutcome, CommandRefusal> {
    commit_core(
        context,
        envelope,
        payload,
        ledger,
        Some((budget, control)),
        None,
    )
}

/// Commit a direct concept from typed metadata and its bounded private raw body.
///
/// # Errors
/// The same authenticated, versioned creation command refusals as typed JSON.
pub fn commit_direct_with_body<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &CommitPayload,
    body: &str,
    ledger: &mut dyn LeaseLedger,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<CommandOutcome, CommandRefusal> {
    commit_core(
        context,
        envelope,
        payload,
        ledger,
        Some((budget, control)),
        Some(body),
    )
}

fn commit_core<F: FileSystem>(
    context: &mut CommandContext<'_, F>,
    envelope: &Envelope,
    payload: &CommitPayload,
    ledger: &mut dyn LeaseLedger,
    graph_validation: Option<(Budget, &dyn QueryControl)>,
    direct_body: Option<&str>,
) -> Result<CommandOutcome, CommandRefusal> {
    let kb = context.store.kb_uuid();
    let grants = authenticate(context.verifier, context.bearer, kb, ledger)?;
    if !matches!(
        payload.kind,
        Some(commit_payload::Kind::DirectConceptCreate(_))
    ) {
        if direct_body.is_some() {
            return Err(CommandRefusal::UnsupportedShape {
                payload: "direct concept creation",
            });
        }
        refuse_expected_version(envelope)?;
    }
    let validated = validate_commit_request(ledger, &context.validation, envelope, payload)?;
    let envelope = validated.envelope().message();
    check_envelope_kb(kb, envelope)?;

    let read_set = read_set_of(envelope, ledger)?;
    let meta = request_meta(envelope, context.execution);
    let family = payload
        .kind
        .as_ref()
        .ok_or(CommandRefusal::UnsupportedShape {
            payload: "CommitPayload",
        })?;
    let (command, outcome) = match family {
        commit_payload::Kind::DirectConceptCreate(request) => {
            let (budget, control) = graph_validation.ok_or(CommandRefusal::UnsupportedShape {
                payload: "direct concept creation requires validation budget and control",
            })?;
            let term = request
                .term
                .as_ref()
                .ok_or(CommandRefusal::UnsupportedShape {
                    payload: "DirectConceptCreate.term",
                })?;
            let relations = initial_relations(&request.assertions, ledger)?;
            let mut draft = term_draft(term)?;
            if let Some(body) = direct_body {
                if !draft.body.is_empty() {
                    return Err(CommandRefusal::UnsupportedShape {
                        payload: "direct concept raw body conflicts with JSON body",
                    });
                }
                draft.body = body;
            }
            (
                WriteCommand::DirectConceptCreate,
                term_create::create_direct(
                    context.store,
                    &grants,
                    meta,
                    term_create::DirectCreate {
                        draft,
                        initial_relations: &relations,
                        read_set: &read_set,
                        expect_version: envelope
                            .expect_version
                            .as_deref()
                            .map(parse_expect_version)
                            .transpose()?,
                    },
                    budget,
                    control,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::DirectConcept)?,
            )
        }
        commit_payload::Kind::CreateSessionCheckpoint(state) => (
            WriteCommand::CreateSessionCheckpoint,
            session::create_session_checkpoint(
                context.store,
                &grants,
                meta,
                session_state("create_session_checkpoint", state)?,
                &read_set,
                &context.limits,
                ledger,
            )
            .map_err(CommandRefusal::Session)?,
        ),
        commit_payload::Kind::ReviseSessionCheckpoint(revise) => {
            let checkpoint = proto_key(
                "revise_session_checkpoint.checkpoint",
                revise.checkpoint.as_ref(),
            )?;
            let state = revise
                .state
                .as_ref()
                .ok_or(CommandRefusal::UnsupportedShape {
                    payload: "ReviseSessionCheckpoint.state",
                })?;
            (
                WriteCommand::ReviseSessionCheckpoint,
                session::revise_session_checkpoint(
                    context.store,
                    &grants,
                    meta,
                    checkpoint,
                    session_state("revise_session_checkpoint.state", state)?,
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Session)?,
            )
        }
        commit_payload::Kind::CreateFrontier(state) => {
            let refs = frontier_refs(state, ledger)?;
            (
                WriteCommand::CreateFrontier,
                frontier::create_frontier(
                    context.store,
                    &grants,
                    meta,
                    FrontierState {
                        concept_refs: &refs,
                        text: &state.text,
                        priority: narrow("create_frontier.priority", state.priority)?,
                        status: narrow("create_frontier.status", state.status)?,
                    },
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Frontier)?,
            )
        }
        commit_payload::Kind::ReviseFrontier(revise) => {
            let key = proto_key("revise_frontier.frontier", revise.frontier.as_ref())?;
            let state = revise
                .state
                .as_ref()
                .ok_or(CommandRefusal::UnsupportedShape {
                    payload: "ReviseFrontier.state",
                })?;
            // The state is COMPLETE: forward every new ref, and never a ref from the old
            // record. The kernel compares the whole list against the read set.
            let refs = frontier_refs(state, ledger)?;
            (
                WriteCommand::ReviseFrontier,
                frontier::revise_frontier(
                    context.store,
                    &grants,
                    meta,
                    key,
                    FrontierState {
                        concept_refs: &refs,
                        text: &state.text,
                        priority: narrow("revise_frontier.priority", state.priority)?,
                        status: narrow("revise_frontier.status", state.status)?,
                    },
                    &read_set,
                    &context.limits,
                    ledger,
                )
                .map_err(CommandRefusal::Frontier)?,
            )
        }
    };

    Ok(CommandOutcome { command, outcome })
}

/// The protocol-validated envelope and payload of whichever command was dispatched.
///
/// Exposed so a caller can prove a refusal happened *before* any mutation rather than trusting
/// a log line. It carries no authority of its own.
pub type ValidatedCommand<'a, P> = ValidatedRequest<'a, P>;

// ---------------------------------------------------------------------------
// Authentication and envelope checks
// ---------------------------------------------------------------------------

/// Verify the bearer against the actual store's KB on the caller's ledger.
///
/// The returned grants are the ONLY source of principal for the audit fields; nothing in the
/// request body can influence them.
fn authenticate<'a>(
    verifier: &'a CredentialVerifier<'a>,
    bearer: &[u8],
    kb: KbUuid,
    ledger: &mut dyn LeaseLedger,
) -> Result<Grants<'a>, CommandRefusal> {
    verifier
        .verify_bearer(bearer, kb, ledger)
        .map_err(|error| match error {
            // The ledger's own verdict travels unchanged.
            VerifyError::Refused(refusal) => CommandRefusal::Refused(refusal),
            // Everything else is the one opaque authentication class.
            other => CommandRefusal::Unauthenticated(other),
        })
}

/// Refuse a whole-KB version expectation, before any mutation.
///
/// These kernel commands carry no whole-KB expectation. Interpreting a pin as "no expectation"
/// would silently change the request's identity; honouring it would need a mechanism the
/// kernel does not have for these operations.
fn refuse_expected_version(envelope: &Envelope) -> Result<(), CommandRefusal> {
    match envelope.expect_version.as_ref() {
        None => Ok(()),
        // Refused, never interpreted. The value is parsed into a scalar so the refusal path
        // allocates nothing; the caller still holds the exact decimal it sent.
        Some(expected) => Err(CommandRefusal::UnsupportedExpectedVersion {
            expected: parse_expect_version(expected)?,
        }),
    }
}

/// Parse a validated decimal version into a scalar.
///
/// The protocol gate has already refused a non-canonical or out-of-range field, so a failure
/// here is a conversion defect rather than caller input — reported as a malformed field, never
/// papered over with a default.
fn parse_expect_version(text: &str) -> Result<u64, CommandRefusal> {
    text.parse::<u64>()
        .map_err(|_| CommandRefusal::MalformedField {
            field: "envelope.expect_version",
        })
}

/// The envelope must name the KB this store actually serves.
fn check_envelope_kb(store_kb: KbUuid, envelope: &Envelope) -> Result<(), CommandRefusal> {
    let declared = parse_kb_uuid(&envelope.kb_uuid).ok_or(CommandRefusal::MalformedField {
        field: "envelope.kb_uuid",
    })?;
    if declared != store_kb {
        return Err(CommandRefusal::KbMismatch {
            expected: store_kb,
            declared,
        });
    }
    Ok(())
}

/// Parse the canonical 32-character lowercase hex form.
///
/// The protocol validator has already enforced the shape, so a failure here is a conversion
/// defect rather than caller input; both are reported as a malformed field, never guessed.
fn parse_kb_uuid(text: &str) -> Option<KbUuid> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut parsed = [0u8; 16];
    for (at, slot) in parsed.iter_mut().enumerate() {
        let pair = text.get(at * 2..at * 2 + 2)?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(KbUuid::from_bytes(parsed))
}

/// Convert the envelope's read set into canonical pairs, preserving order exactly.
///
/// No sorting, de-duplication or repair: a read set that disagrees with what the operation
/// requires is the kernel's caller error to report. The vector is the one allocation, reserved
/// exactly once at the measured element size.
fn read_set_of(
    envelope: &Envelope,
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<RevPair>, CommandRefusal> {
    let slots = envelope.read_set.len();
    let bytes = slots
        .checked_mul(core::mem::size_of::<RevPair>())
        .ok_or(CommandRefusal::ReadSetExtent)?;
    let extent = u64::try_from(bytes).map_err(|_| CommandRefusal::ReadSetExtent)?;
    ledger
        .try_charge(extent)
        .map_err(|error| CommandRefusal::Refused(Refusal::Budget(error)))?;
    let mut pairs: Vec<RevPair> = Vec::new();
    pairs
        .try_reserve_exact(slots)
        .map_err(|_| CommandRefusal::Refused(Refusal::Allocation { requested: bytes }))?;
    if pairs.capacity() != slots {
        return Err(CommandRefusal::ReadSetExtent);
    }
    for pair in &envelope.read_set {
        ledger
            .try_charge_entries(1)
            .map_err(|error| CommandRefusal::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| CommandRefusal::Refused(Refusal::Stopped(stop)))?;
        let key = proto_key("envelope.read_set.key", pair.key.as_ref())?;
        pairs.push(RevPair {
            key,
            revision: parse_revision("envelope.read_set.revision", &pair.revision)?,
        });
    }
    Ok(pairs)
}

/// The one proposal pair an envelope must carry for a review.
///
/// These reviews take exactly one proposal revision. Accepting extra pairs and checking only
/// the first would tell the caller their other dependencies were verified when they were not.
fn single_proposal_pair(
    read_set: &[RevPair],
) -> Result<(RecordKey, RecordRevision), CommandRefusal> {
    match read_set {
        [only] if only.key.kind() == RecordKind::Proposal => Ok((only.key, only.revision)),
        [_] => Err(CommandRefusal::MalformedField {
            field: "envelope.read_set.key",
        }),
        _ => Err(CommandRefusal::MalformedField {
            field: "envelope.read_set",
        }),
    }
}

/// Build the kernel's request metadata from the authenticated envelope and the executor.
fn request_meta(envelope: &Envelope, execution: ExecutionMeta) -> RequestMeta<'_> {
    RequestMeta {
        request_id: &envelope.request_id,
        tx_id: execution.tx_id,
        created_at: execution.created_at,
    }
}

// ---------------------------------------------------------------------------
// Field conversions
// ---------------------------------------------------------------------------

/// Convert one protocol key into a canonical [`RecordKey`].
///
/// The kind must be in the closed registry and the id must fit the bound that kind declares —
/// checked by Types itself, never restated here.
fn proto_key(field: &'static str, key: Option<&WireKey>) -> Result<RecordKey, CommandRefusal> {
    let key = key.ok_or(CommandRefusal::MalformedField { field })?;
    let kind_u16 = u16::try_from(key.kind).map_err(|_| CommandRefusal::MalformedField { field })?;
    let kind = RecordKind::from_u16(kind_u16).ok_or(CommandRefusal::MalformedField { field })?;
    let id: u128 = key
        .id
        .parse()
        .map_err(|_| CommandRefusal::MalformedField { field })?;
    let canonical = RecordKey::new(kind, id);
    if !canonical.id_is_in_range() {
        return Err(CommandRefusal::MalformedField { field });
    }
    Ok(canonical)
}

/// Convert one canonical decimal revision.
fn parse_revision(field: &'static str, text: &str) -> Result<RecordRevision, CommandRefusal> {
    let value: u64 = text
        .parse()
        .map_err(|_| CommandRefusal::MalformedField { field })?;
    RecordRevision::new(value).ok_or(CommandRefusal::MalformedField { field })
}

/// Narrow a protocol `uint32` to the kernel's `u16` domain, refusing overflow rather than
/// truncating.
fn narrow(field: &'static str, value: u32) -> Result<u16, CommandRefusal> {
    u16::try_from(value).map_err(|_| CommandRefusal::MalformedField { field })
}

fn required_expect_version(envelope: &Envelope) -> Result<u64, CommandRefusal> {
    let field = "envelope.expect_version";
    let version = parse_expect_version(
        envelope
            .expect_version
            .as_deref()
            .ok_or(CommandRefusal::MalformedField { field })?,
    )?;
    if version == 0 {
        return Err(CommandRefusal::MalformedField { field });
    }
    Ok(version)
}

fn public_digest(text: &str) -> Result<[u8; 32], CommandRefusal> {
    let malformed = CommandRefusal::MalformedField {
        field: "legacy_proposal.public_revision",
    };
    if text.len() != 64 {
        return Err(malformed);
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let Some(pair) = text.get(i * 2..i * 2 + 2) else {
            return Err(malformed);
        };
        let Ok(value) = u8::from_str_radix(pair, 16) else {
            return Err(malformed);
        };
        *byte = value;
    }
    Ok(bytes)
}

fn relation_draft(
    edge: &sage_db_protocol::generated::RelationAssert,
) -> Result<relations::RelationAssert<'_>, CommandRefusal> {
    Ok(relations::RelationAssert {
        from: proto_key("relation_assert.from", edge.from.as_ref())?,
        target: proto_key("relation_assert.target", edge.target.as_ref())?,
        relation: narrow(
            "relation_assert.relation",
            edge.relation.ok_or(CommandRefusal::MalformedField {
                field: "relation_assert.relation",
            })?,
        )?,
        confidence: narrow(
            "relation_assert.confidence",
            edge.confidence.ok_or(CommandRefusal::MalformedField {
                field: "relation_assert.confidence",
            })?,
        )?,
        declaration_order: edge.declaration_order,
        declared_author: &edge.declared_author,
        reason: &edge.reason,
    })
}

fn concept_draft(text: &ConceptText) -> Result<ConceptTextEdit<'_>, CommandRefusal> {
    Ok(ConceptTextEdit {
        subject: proto_key("concept_text.subject", text.subject.as_ref())?,
        field_mask: concept_mask(text),
        atom: text.atom.as_deref(),
        body: text.body.as_deref(),
        declared_author: &text.declared_author,
    })
}

fn term_draft(
    term: &sage_db_protocol::generated::TermCreate,
) -> Result<term_create::Draft<'_>, CommandRefusal> {
    let missing = |field| CommandRefusal::MalformedField { field };
    let layer = term
        .layer
        .and_then(|value| u8::try_from(value).ok())
        .and_then(sage_db_types::layer::SemanticLayer::from_u8)
        .ok_or_else(|| missing("term_create.layer"))?;
    let confidence = term
        .confidence
        .and_then(|value| u16::try_from(value).ok())
        .and_then(sage_db_types::confidence::Confidence::from_u16)
        .ok_or_else(|| missing("term_create.confidence"))?;
    let source = term
        .source
        .and_then(|value| u16::try_from(value).ok())
        .and_then(sage_db_types::persist::TermSource::from_u16)
        .ok_or_else(|| missing("term_create.source"))?;
    Ok(term_create::Draft {
        slug: &term.slug,
        atom: &term.atom,
        body: &term.body,
        layer,
        domains: &term.domains,
        confidence,
        source,
        triggers: &term.triggers,
        declared_author: &term.declared_author,
        reason: &term.reason,
    })
}

fn initial_relations(
    assertions: &[sage_db_protocol::generated::InitialConceptAssertion],
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<term_create::InitialRelation>, CommandRefusal> {
    let slots = assertions.len();
    let bytes = slots
        .checked_mul(core::mem::size_of::<term_create::InitialRelation>())
        .ok_or(CommandRefusal::InitialRelationExtent)?;
    ledger
        .try_charge(u64::try_from(bytes).map_err(|_| CommandRefusal::InitialRelationExtent)?)
        .map_err(|error| CommandRefusal::Refused(Refusal::Budget(error)))?;
    let mut relations = Vec::new();
    relations
        .try_reserve_exact(slots)
        .map_err(|_| CommandRefusal::Refused(Refusal::Allocation { requested: bytes }))?;
    if relations.capacity() != slots {
        return Err(CommandRefusal::InitialRelationExtent);
    }
    for assertion in assertions {
        charge_entries(ledger, 1)?;
        ledger
            .checkpoint()
            .map_err(|stop| CommandRefusal::Refused(Refusal::Stopped(stop)))?;
        let target = proto_key(
            "direct_concept_create.assertion.target",
            assertion.target.as_ref(),
        )?;
        let relation = assertion
            .relation
            .and_then(|value| u8::try_from(value).ok())
            .and_then(sage_db_types::relation::RelationId::from_u8)
            .ok_or(CommandRefusal::MalformedField {
                field: "direct_concept_create.assertion.relation",
            })?;
        let confidence = assertion
            .confidence
            .and_then(|value| u16::try_from(value).ok())
            .and_then(sage_db_types::confidence::Confidence::from_u16)
            .ok_or(CommandRefusal::MalformedField {
                field: "direct_concept_create.assertion.confidence",
            })?;
        relations.push(term_create::InitialRelation {
            target,
            relation,
            confidence,
        });
    }
    Ok(relations)
}

/// Build the kernel's session state from the protocol's.
fn session_state<'a>(
    field: &'static str,
    state: &'a SessionCheckpointState,
) -> Result<SessionState<'a>, CommandRefusal> {
    Ok(SessionState {
        concept: proto_key("session_checkpoint.concept", state.concept.as_ref())
            .map_err(|_| CommandRefusal::MalformedField { field })?,
        session_no: state.session_no,
        content: &state.content,
    })
}

/// Which fields a concept-text edit names.
///
/// `None` and `Some("")` are different statements — "not edited" versus "becomes empty" — so
/// the mask is derived from presence, never from emptiness.
fn concept_mask(text: &ConceptText) -> u8 {
    let mut mask = 0u8;
    if text.atom.is_some() {
        mask |= concept_intent::MASK_ATOM;
    }
    if text.body.is_some() {
        mask |= concept_intent::MASK_BODY;
    }
    mask
}

/// The kernel's decision enum for claim review.
fn proto_decision(wire: i32) -> Result<ReviewDecision, CommandRefusal> {
    match Decision::try_from(wire) {
        Ok(Decision::Approve) => Ok(ReviewDecision::Approve),
        Ok(Decision::Reject) => Ok(ReviewDecision::Reject),
        // `UNSPECIFIED` and unknown values are refused, never defaulted to approve.
        _ => Err(CommandRefusal::MalformedField {
            field: "review.decision",
        }),
    }
}

/// The kernel's wire decision value for concept review.
fn proto_decision_wire(wire: i32) -> Result<u16, CommandRefusal> {
    match Decision::try_from(wire) {
        Ok(Decision::Approve) => Ok(concept_intent::DECISION_APPROVE),
        Ok(Decision::Reject) => Ok(concept_intent::DECISION_REJECT),
        _ => Err(CommandRefusal::MalformedField {
            field: "review.decision",
        }),
    }
}

/// Materialise a frontier entry's refs with an exact, pre-sized reservation.
///
/// The kernel takes a `&[RecordKey]`, so the conversion must own the list. The reservation is
/// exact and happens **before** any copy: no `collect()`, and no growth after admission.
fn frontier_refs(
    state: &WireFrontierState,
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<RecordKey>, CommandRefusal> {
    let declared = u64::try_from(state.concept_refs.len()).unwrap_or(u64::MAX);
    // Charge the copy before reserving it: the ledger decides, not the allocator.
    charge_entries(ledger, declared)?;
    let mut refs: Vec<RecordKey> = Vec::new();
    refs.try_reserve_exact(state.concept_refs.len())
        .map_err(|_| CommandRefusal::MalformedField {
            field: "frontier.concept_refs",
        })?;
    for (index, key) in state.concept_refs.iter().enumerate() {
        // The refs are Terms by definition; a non-Term ref is a caller error, not a guess.
        let key = proto_key("frontier.concept_refs", Some(key))?;
        if key.kind() != RecordKind::Term {
            let _ = index;
            return Err(CommandRefusal::MalformedField {
                field: "frontier.concept_refs",
            });
        }
        refs.push(key);
    }
    Ok(refs)
}

/// Charge `entries` to the caller's ledger before the matching copy.
fn charge_entries(ledger: &mut dyn LeaseLedger, entries: u64) -> Result<(), CommandRefusal> {
    use sage_db_types::framing::Refusal;
    ledger
        .try_charge_entries(entries)
        .map_err(|error| CommandRefusal::Invalid(ValidationError::Refused(Refusal::Budget(error))))
}
