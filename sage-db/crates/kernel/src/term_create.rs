//! Term creation by approved proposal or by a dedicated direct-creation grant.
mod direct;
pub use direct::{DirectCreate, InitialRelation, MAX_DIRECT_BODY_BYTES, create_direct};

use sage_db_infer::validate::Violation;
use sage_db_store::{
    fold::FoldedRecord,
    fs::FileSystem,
    handle::{HandleOutcome, Store},
    lease::LeaseLedger,
    typed::{self, EventInput},
    view::{CommitBasis, RecordScanError, ViewBasis},
};
use sage_db_types::{
    budget::Budget,
    confidence::Confidence,
    framing::Limits,
    identity::RecordRevision,
    layer::SemanticLayer,
    ledger::SharedLedger,
    persist::{
        ActorOrigin, CommonFields, Extensions, IdAllocationEvent, IdSpace, ProposalKind,
        ProposalRecord, ProposalStatus, RecordKey, RecordKind, RevPair, TermRecord, TermSource,
        Watermark,
    },
    prepared::{BlobDeclaration, BlobSource, PreparedTransaction},
    query::{QueryControl, QueryError},
    record::TermMeta,
};

use crate::{
    auth::{Capability, Grants},
    candidate::{self, Addition, CandidateError},
    claims::{
        CommandError, InputError, InternalDefect, RequestMeta, StoredField, admitted_bytes,
        admitted_string, admitted_vec, check_meta, ensure_import_ready, extent, folded_current,
        frame_and_digest, map_format, map_request, outcome_event, retry_outcome, revision_next,
        version_next,
    },
    concept_intent::{self, ConceptTextPayloadError},
};

/// Ordinary Term creation proposal submission.
pub const SUBMIT_TERM_CREATE: u16 = 12;
/// Ordinary Term creation proposal decision.
pub const REVIEW_TERM_CREATE: u16 = 13;
const VERSION: u16 = 1;

/// Authored Term metadata and content as supplied by the proposer.
#[derive(Debug)]
pub struct Draft<'a, T = &'a [String]> {
    /// New stable slug.
    pub slug: &'a str,
    /// New summary.
    pub atom: &'a str,
    /// Complete Markdown body.
    pub body: &'a str,
    /// New semantic layer.
    pub layer: SemanticLayer,
    /// Domain labels in authored order.
    pub domains: T,
    /// Authored confidence.
    pub confidence: Confidence,
    /// Knowledge source class.
    pub source: TermSource,
    /// Activation triggers in authored order.
    pub triggers: T,
    /// Author attribution.
    pub declared_author: &'a str,
    /// Why the concept is proposed.
    pub reason: &'a str,
}

/// A typed refusal to submit or review creation.
#[derive(Debug)]
pub enum CreateError {
    /// Shared authorization, budget, conflict, or store refusal.
    Command(CommandError),
    /// Invalid submitted field.
    Field(&'static str),
    /// Canonical text or review payload validation.
    Text(ConceptTextPayloadError),
    /// The stored proposal frame cannot be used for creation.
    StoredPayload,
    /// An active or retired Term already owns this slug.
    SlugExists,
    /// No new identifier can be allocated in this space.
    IdExhausted(IdSpace),
    /// Allocated key is already present.
    IdCollision(RecordKey),
    /// The graph evaluator did not finish.
    Validation(QueryError),
    /// Committed rules refuse the graph with the proposed Term.
    InvalidCandidate(Vec<Violation>),
}

impl From<CommandError> for CreateError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

impl From<ConceptTextPayloadError> for CreateError {
    fn from(error: ConceptTextPayloadError) -> Self {
        match error {
            ConceptTextPayloadError::Refused(refusal) => Self::Command(refusal.into()),
            error => Self::Text(error),
        }
    }
}

fn internal() -> CommandError {
    CommandError::Internal(InternalDefect::EncodeDecodeDivergence)
}

fn field(value: &str, max: u32, name: &'static str) -> Result<(), CreateError> {
    if u32::try_from(value.len()).is_ok_and(|size| size <= max)
        && !value.bytes().any(|byte| byte == 0)
    {
        Ok(())
    } else {
        Err(CreateError::Field(name))
    }
}

fn check_text<T>(draft: &Draft<'_, T>) -> Result<(), CreateError> {
    check_text_bounded(draft, u64::from(typed::MAX_PAYLOAD))
}

fn check_text_bounded<T>(draft: &Draft<'_, T>, max_body: u64) -> Result<(), CreateError> {
    field(draft.slug, typed::MAX_SLUG_BYTES, "slug")?;
    let mut slug = draft.slug.bytes();
    if !slug
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !slug.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(CreateError::Field("slug"));
    }
    concept_intent::check_atom(draft.atom)?;
    concept_intent::check_declared_author(draft.declared_author)?;
    field(draft.reason, typed::MAX_TEXT_BYTES, "reason")?;
    if draft.reason.trim().is_empty() {
        return Err(CreateError::Field("reason"));
    }
    if extent(draft.body.len())? > max_body || draft.body.trim().is_empty() {
        return Err(CreateError::Field("body"));
    }
    Ok(())
}

fn put_str(out: &mut Vec<u8>, value: &str) -> Result<(), CreateError> {
    let len = u32::try_from(value.len()).map_err(|_| CreateError::Field("extent"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn strings_extent<'a>(
    values: impl IntoIterator<Item = (&'a str, u32, &'static str)>,
    total: &mut usize,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CreateError> {
    for (value, limit, name) in values {
        ledger.try_charge_entries(1).map_err(CommandError::from)?;
        ledger.checkpoint().map_err(CommandError::from)?;
        field(value, limit, name)?;
        *total = total
            .checked_add(4)
            .and_then(|n| n.checked_add(value.len()))
            .ok_or(CreateError::Field("extent"))?;
    }
    Ok(())
}

fn list_count(count: usize, max: u32, name: &'static str) -> Result<u32, CreateError> {
    u32::try_from(count)
        .ok()
        .filter(|&n| n > 0 && n <= max)
        .ok_or(CreateError::Field(name))
}

fn encode(draft: &Draft<'_>, ledger: &mut dyn SharedLedger) -> Result<Vec<u8>, CreateError> {
    encode_payload(draft, None, None, ledger)
}

fn encode_payload(
    draft: &Draft<'_>,
    initial_relations: Option<&[InitialRelation]>,
    body_ref: Option<[u8; 32]>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CreateError> {
    ledger.checkpoint().map_err(CommandError::from)?;
    if body_ref.is_some() {
        check_text_bounded(draft, MAX_DIRECT_BODY_BYTES as u64)?;
    } else {
        check_text(draft)?;
    }
    let domains = list_count(draft.domains.len(), typed::MAX_DOMAINS, "domains")?;
    let triggers = list_count(draft.triggers.len(), typed::MAX_TRIGGERS, "triggers")?;
    // Five length prefixes, two list counts, a u8 layer, two u16 discriminants.
    let mut total = 33usize;
    let body_extent = if body_ref.is_some() {
        8 + 32 - 4
    } else {
        draft.body.len()
    };
    total = total
        .checked_add(body_extent)
        .ok_or(CreateError::Field("extent"))?;
    for value in [draft.slug, draft.atom, draft.declared_author, draft.reason] {
        total = total
            .checked_add(value.len())
            .ok_or(CreateError::Field("extent"))?;
    }
    strings_extent(
        draft
            .domains
            .iter()
            .map(|s| (s.as_str(), typed::MAX_TERM_DOMAIN_BYTES, "domains")),
        &mut total,
        ledger,
    )?;
    strings_extent(
        draft
            .triggers
            .iter()
            .map(|s| (s.as_str(), typed::MAX_TRIGGER_BYTES, "triggers")),
        &mut total,
        ledger,
    )?;
    if let Some(relations) = initial_relations {
        let relation_bytes = relations
            .len()
            .checked_mul(22)
            .and_then(|bytes| bytes.checked_add(4))
            .ok_or(CreateError::Field("relations"))?;
        total = total
            .checked_add(relation_bytes)
            .ok_or(CreateError::Field("relations"))?;
    }
    if extent(total)? > u64::from(typed::MAX_PAYLOAD) {
        return Err(CreateError::Field("payload"));
    }
    let mut out = admitted_vec(total, ledger)?;
    put_str(&mut out, draft.slug)?;
    put_str(&mut out, draft.atom)?;
    if let Some(digest) = body_ref {
        out.extend_from_slice(&extent(draft.body.len())?.to_le_bytes());
        out.extend_from_slice(&digest);
    } else {
        put_str(&mut out, draft.body)?;
    }
    out.push(draft.layer.get());
    out.extend_from_slice(&draft.confidence.as_u16().to_le_bytes());
    out.extend_from_slice(&(draft.source as u16).to_le_bytes());
    for (count, values) in [(domains, draft.domains), (triggers, draft.triggers)] {
        out.extend_from_slice(&count.to_le_bytes());
        for value in values {
            ledger.checkpoint().map_err(CommandError::from)?;
            put_str(&mut out, value)?;
        }
    }
    put_str(&mut out, draft.declared_author)?;
    put_str(&mut out, draft.reason)?;
    if let Some(relations) = initial_relations {
        let count = u32::try_from(relations.len()).map_err(|_| CreateError::Field("relations"))?;
        out.extend_from_slice(&count.to_le_bytes());
        for relation in relations {
            ledger.try_charge_entries(1).map_err(CommandError::from)?;
            ledger.checkpoint().map_err(CommandError::from)?;
            out.extend_from_slice(&relation.target.kind().as_u16().to_le_bytes());
            out.extend_from_slice(&relation.target.id().to_le_bytes());
            out.extend_from_slice(&(relation.relation as u16).to_le_bytes());
            out.extend_from_slice(&relation.confidence.as_u16().to_le_bytes());
        }
    }
    Ok(out)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], CreateError> {
        let (value, rest) = self
            .0
            .split_at_checked(size)
            .ok_or(CreateError::StoredPayload)?;
        self.0 = rest;
        Ok(value)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], CreateError> {
        self.take(N)?
            .try_into()
            .map_err(|_| CreateError::StoredPayload)
    }

    fn length(&mut self) -> Result<usize, CreateError> {
        usize::try_from(u32::from_le_bytes(self.fixed()?)).map_err(|_| CreateError::StoredPayload)
    }

    fn text(&mut self) -> Result<&'a str, CreateError> {
        let size = self.length()?;
        core::str::from_utf8(self.take(size)?).map_err(|_| CreateError::StoredPayload)
    }

    fn list(
        &mut self,
        max_count: u32,
        max: u32,
        name: &'static str,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Vec<String>, CreateError> {
        let count = self.length()?;
        list_count(count, max_count, name)?;
        if count > self.0.len() / 4 {
            return Err(CreateError::StoredPayload);
        }
        let mut values = admitted_vec(count, ledger)?;
        for _ in 0..count {
            let value = self.text()?;
            field(value, max, name)?;
            values.push(admitted_string(value, ledger)?);
        }
        Ok(values)
    }
}

fn decode<'a>(
    bytes: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Draft<'a, Vec<String>>, CreateError> {
    ledger.checkpoint().map_err(CommandError::from)?;
    if extent(bytes.len())? > u64::from(typed::MAX_PAYLOAD) {
        return Err(CreateError::StoredPayload);
    }
    let mut cursor = Cursor(bytes);
    let slug = cursor.text()?;
    let atom = cursor.text()?;
    let body = cursor.text()?;
    let layer =
        SemanticLayer::from_u8(cursor.fixed::<1>()?[0]).ok_or(CreateError::Field("layer"))?;
    let confidence = Confidence::from_u16(u16::from_le_bytes(cursor.fixed()?))
        .ok_or(CreateError::Field("confidence"))?;
    let source = TermSource::from_u16(u16::from_le_bytes(cursor.fixed()?))
        .ok_or(CreateError::Field("source"))?;
    let domains = cursor.list(
        typed::MAX_DOMAINS,
        typed::MAX_TERM_DOMAIN_BYTES,
        "domains",
        ledger,
    )?;
    let triggers = cursor.list(
        typed::MAX_TRIGGERS,
        typed::MAX_TRIGGER_BYTES,
        "triggers",
        ledger,
    )?;
    let draft = Draft {
        slug,
        atom,
        body,
        layer,
        domains,
        triggers,
        confidence,
        source,
        declared_author: cursor.text()?,
        reason: cursor.text()?,
    };
    if !cursor.0.is_empty() {
        return Err(CreateError::StoredPayload);
    }
    check_text(&draft)?;
    Ok(draft)
}

/// Read a hash-verified stored proposal frame and validate its metadata.
///
/// The record and bytes must come from the same publication.
///
/// # Errors
/// Refuses unsupported frames, divergent provenance, and caller ledger refusals.
pub fn decode_stored<'a>(
    record: &ProposalRecord,
    bytes: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Draft<'a, Vec<String>>, CreateError> {
    if record.kind != ProposalKind::TermCreate
        || record.subject.kind() != RecordKind::Term
        || !record.subject.id_is_in_range()
        || !record.base_revisions.is_empty()
    {
        return Err(CreateError::StoredPayload);
    }
    let frame = map_request(typed::decode_request(bytes, ledger))?;
    let [op] = frame.ops.as_slice() else {
        return Err(CreateError::StoredPayload);
    };
    if op.kind != SUBMIT_TERM_CREATE
        || op.version != VERSION
        || !frame.read_set.is_empty()
        || frame.expected_version.is_some()
    {
        return Err(CreateError::StoredPayload);
    }
    let draft = decode(op.payload, ledger)?;
    if draft.declared_author != record.declared_author {
        return Err(CreateError::StoredPayload);
    }
    Ok(draft)
}

fn slug_exists(
    store: &Store<impl FileSystem>,
    basis: ViewBasis,
    slug: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<bool, CommandError> {
    let mut exists = false;
    let observed = store
        .scan_identified_records(ledger, |row, work| {
            if let FoldedRecord::Term(term) = row {
                work.try_charge_entries(extent(slug.len().min(term.slug.len()))?)
                    .map_err(CommandError::from)?;
                work.checkpoint()?;
                exists |= term.slug == slug;
            }
            Ok::<(), CommandError>(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(error) => error,
            RecordScanError::Incomplete(reason) => CommandError::Incomplete(reason),
            RecordScanError::Read(error) => error.into(),
        })?;
    if observed != basis {
        return Err(CommandError::BasisDivergence);
    }
    Ok(exists)
}

struct Write<'a> {
    principal: &'a str,
    meta: RequestMeta<'a>,
    digest: [u8; 32],
}

fn audit(
    principal: &str,
    reason: &str,
    time: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<CommonFields, CommandError> {
    Ok(CommonFields {
        recorded_by: Some(admitted_string(principal, ledger)?),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some(admitted_string(reason, ledger)?),
        created_at: time,
        updated_at: None,
    })
}

fn blob(sha256: [u8; 32], bytes: Vec<u8>) -> Result<BlobDeclaration, CommandError> {
    Ok(BlobDeclaration {
        sha256,
        declared_len: extent(bytes.len())?,
        source: BlobSource::Provide(bytes),
    })
}

fn declare(
    blobs: &mut Vec<BlobDeclaration>,
    declaration: BlobDeclaration,
) -> Result<(), CommandError> {
    for existing in blobs.iter() {
        if existing.sha256 == declaration.sha256 {
            return if existing.declared_len == declaration.declared_len {
                Ok(())
            } else {
                Err(internal())
            };
        }
    }
    blobs.push(declaration);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn publish(
    store: &mut Store<impl FileSystem>,
    basis: CommitBasis,
    write: Write<'_>,
    proposal: &ProposalRecord,
    term: Option<(&TermRecord, Vec<u8>)>,
    intent: BlobDeclaration,
    counters: &[Watermark],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CreateError> {
    let version = version_next(store.head())?;
    let mut events = admitted_vec(2 + counters.len() + usize::from(term.is_some()), ledger)?;
    let mut blobs = admitted_vec(4, ledger)?;
    declare(
        &mut blobs,
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
    )?;
    declare(&mut blobs, intent)?;
    let mut affected = admitted_vec(1 + usize::from(term.is_some()), ledger)?;
    if let Some((term, bytes)) = term {
        affected.push(RevPair {
            key: term.key,
            revision: term.revision,
        });
        events.push(map_format(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Created, term),
            ledger,
        ))?);
        declare(&mut blobs, blob(term.body_ref, bytes)?)?;
    }
    affected.push(RevPair {
        key: proposal.key,
        revision: proposal.revision,
    });
    events.push(map_format(typed::prepare_event(
        EventInput::Proposal {
            decided: proposal.status.is_decided(),
            record: proposal,
        },
        ledger,
    ))?);
    for &allocation in counters {
        events.push(map_format(typed::prepare_event(
            EventInput::IdAllocation(&IdAllocationEvent {
                allocation,
                reason: None,
                extensions: Extensions::new(),
            }),
            ledger,
        ))?);
    }
    let result =
        typed::result_payload(version, &affected, counters, ledger).map_err(CommandError::from)?;
    let result_ref = map_format(typed::raw_hash(&result, ledger))?;
    let outcome = outcome_event(
        version,
        write.principal,
        write.meta.request_id,
        write.digest,
        result_ref,
        write.meta.created_at,
        ledger,
    )?;
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);
    declare(&mut blobs, blob(result_ref, result)?)?;
    let prepared = PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: write.meta.tx_id,
        kb_uuid: store.kb_uuid(),
        created_at: write.meta.created_at,
        principal_id: admitted_string(write.principal, ledger)?,
        request_id: admitted_string(write.meta.request_id, ledger)?,
        payload_digest: write.digest,
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, limits, ledger) {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        outcome => Ok(outcome),
    }
}

/// Reserve a Term id and publish a pending proposal without creating a Term.
///
/// # Errors
/// Refuses unauthorized, invalid, conflicting or resource-bounded requests.
pub fn submit(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    draft: Draft<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CreateError> {
    let authorized = grants
        .authorize(store.kb_uuid(), Capability::Propose, ledger)
        .map_err(CommandError::from)?;
    let principal = authorized.principal();
    check_meta(&meta, principal).map_err(CommandError::from)?;
    if !read_set.is_empty() {
        return Err(CreateError::Field("read_set"));
    }
    let payload = encode(&draft, ledger)?;
    let (frame, digest) =
        frame_and_digest(SUBMIT_TERM_CREATE, VERSION, &payload, read_set, ledger)?;
    if let Some(retry) = retry_outcome(store, principal, meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;
    let basis = store
        .commit_basis(IdSpace::ProposalId, ledger)
        .map_err(CommandError::from)?;
    let terms = store
        .commit_basis(IdSpace::TermId, ledger)
        .map_err(CommandError::from)?;
    if terms.basis != basis.basis {
        return Err(CommandError::BasisDivergence.into());
    }
    if slug_exists(store, basis.basis, draft.slug, ledger)? {
        return Err(CreateError::SlugExists);
    }
    let term_id = u32::try_from(terms.allocated_high_water)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(CreateError::IdExhausted(IdSpace::TermId))?;
    let proposal_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(CreateError::IdExhausted(IdSpace::ProposalId))?;
    let term = RecordKey::term(term_id);
    let key = RecordKey::new(RecordKind::Proposal, proposal_id);
    for key in [term, key] {
        if folded_current(store, key, ledger)?.is_some() {
            return Err(CreateError::IdCollision(key));
        }
    }
    let payload_ref = map_format(typed::raw_hash(&frame, ledger))?;
    let proposal = ProposalRecord {
        key,
        subject: term,
        revision: RecordRevision::FIRST,
        kind: ProposalKind::TermCreate,
        status: ProposalStatus::Pending,
        base_revisions: Vec::new(),
        payload_ref,
        declared_author: admitted_string(draft.declared_author, ledger)?,
        common: audit(principal, draft.reason, meta.created_at, ledger)?,
        review_note: None,
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    publish(
        store,
        basis,
        Write {
            principal,
            meta,
            digest,
        },
        &proposal,
        None,
        blob(payload_ref, frame)?,
        &[
            Watermark {
                space: IdSpace::TermId,
                hwm: u128::from(term_id),
            },
            Watermark {
                space: IdSpace::ProposalId,
                hwm: proposal_id,
            },
        ],
        limits,
        ledger,
    )
}

/// Decide a pending creation proposal at its exact revision.
///
/// # Errors
/// Refuses unauthorized or stale reviews, malformed intent, graph violations and ledger limits.
#[allow(clippy::too_many_arguments)]
pub fn review(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    key: RecordKey,
    revision: RecordRevision,
    decision: u16,
    note: &str,
    budget: Budget,
    control: &dyn QueryControl,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CreateError> {
    let authorized = grants
        .authorize(store.kb_uuid(), Capability::Review, ledger)
        .map_err(CommandError::from)?;
    let principal = authorized.principal();
    check_meta(&meta, principal).map_err(CommandError::from)?;
    let payload = concept_intent::encode_review(key, decision, note, ledger)?;
    let (_, digest) = frame_and_digest(
        REVIEW_TERM_CREATE,
        VERSION,
        &payload,
        &[RevPair { key, revision }],
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, principal, meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;
    let basis = store
        .commit_basis(IdSpace::TermId, ledger)
        .map_err(CommandError::from)?;
    let record = folded_current(store, key, ledger)?
        .ok_or(CommandError::Input(InputError::MissingProposal(key)))?;
    let FoldedRecord::Proposal(record) = record else {
        return Err(internal().into());
    };
    if record.revision != revision {
        return Err(CommandError::Input(InputError::StaleBaseline {
            key,
            expected: revision,
        })
        .into());
    }
    if record.status != ProposalStatus::Pending {
        return Err(CommandError::Input(InputError::ProposalNotPending(record.status)).into());
    }
    if record.kind != ProposalKind::TermCreate {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::Kind)).into(),
        );
    }
    let typed::DispatchedPayload::Proposal(mut proposal) = map_format(typed::copy_payload(
        EventInput::Proposal {
            decided: false,
            record,
        },
        ledger,
    ))?
    else {
        return Err(internal().into());
    };
    let content = store
        .read_committed_blob(proposal.payload_ref, typed::MAX_FRAME_BYTES, ledger)
        .map_err(CommandError::from)?;
    if content.basis() != basis.basis {
        return Err(CommandError::BasisDivergence.into());
    }
    let intent = BlobDeclaration {
        sha256: proposal.payload_ref,
        declared_len: extent(content.len())?,
        source: BlobSource::Existing,
    };
    proposal.revision = revision_next(revision)?;
    proposal.common.updated_at = Some(meta.created_at);
    proposal.common.recorded_by = Some(admitted_string(principal, ledger)?);
    proposal.common.reason = Some(admitted_string(note, ledger)?);
    proposal.review_note = Some(admitted_string(note, ledger)?);
    let write = Write {
        principal,
        meta,
        digest,
    };
    if decision == concept_intent::DECISION_REJECT {
        proposal.status = ProposalStatus::Rejected;
        return publish(
            store,
            basis,
            write,
            &proposal,
            None,
            intent,
            &[],
            limits,
            ledger,
        );
    }
    let draft = decode_stored(&proposal, content.as_bytes(), ledger)?;
    let subject = proposal.subject;
    if subject.id() > basis.allocated_high_water {
        return Err(CreateError::StoredPayload);
    }
    if folded_current(store, subject, ledger)?.is_some()
        || slug_exists(store, basis.basis, draft.slug, ledger)?
    {
        proposal.status = ProposalStatus::Conflicted;
        return publish(
            store,
            basis,
            write,
            &proposal,
            None,
            intent,
            &[],
            limits,
            ledger,
        );
    }
    candidate::validate(
        store,
        &basis,
        Addition::Term(TermMeta {
            id: candidate::term_id(subject)?,
            layer: draft.layer,
        }),
        budget,
        control,
        ledger,
    )
    .map_err(|error| match error {
        CandidateError::Command(error) => CreateError::Command(error),
        CandidateError::Invalid(violations) => CreateError::InvalidCandidate(violations),
        CandidateError::Validation(error) => CreateError::Validation(error),
    })?;
    let body = admitted_bytes(draft.body.as_bytes(), ledger)?;
    let term = TermRecord {
        key: subject,
        revision: RecordRevision::FIRST,
        retired: false,
        slug: admitted_string(draft.slug, ledger)?,
        atom: admitted_string(draft.atom, ledger)?,
        body_ref: map_format(typed::raw_hash(&body, ledger))?,
        layer: draft.layer,
        domains: draft.domains,
        triggers: draft.triggers,
        confidence: draft.confidence,
        source: draft.source,
        common: audit(principal, note, write.meta.created_at, ledger)?,
        legacy_id: None,
        legacy_source_path: None,
        legacy_content_digest: None,
        legacy_source_file_digest: None,
        legacy_source_revision: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    proposal.status = ProposalStatus::Approved;
    publish(
        store,
        basis,
        write,
        &proposal,
        Some((&term, body)),
        intent,
        &[],
        limits,
        ledger,
    )
}
