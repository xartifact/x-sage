use crate::claims::frame_and_digest_with_version;
use sage_db_store::handle::HandleRefusal;
use sage_db_types::{
    identity::AssertionId, persist::AssertionRecordFull, record::AssertionRecord,
    relation::RelationId,
};

use super::{
    Addition, BlobDeclaration, BlobSource, Budget, CandidateError, Capability, CommandError,
    Confidence, CreateError, Draft, EventInput, Extensions, FileSystem, FoldedRecord, Grants,
    HandleOutcome, IdAllocationEvent, IdSpace, InputError, LeaseLedger, Limits,
    PreparedTransaction, QueryControl, RecordKey, RecordKind, RecordRevision, RequestMeta, RevPair,
    SharedLedger, Store, TermMeta, TermRecord, Watermark, admitted_bytes, admitted_string,
    admitted_vec, audit, blob, candidate, check_meta, declare, encode_payload, ensure_import_ready,
    extent, folded_current, map_format, outcome_event, retry_outcome, slug_exists, typed,
    version_next,
};

// The v1 payload is the existing draft encoding followed by the count and the
// authored relations. Each relation uses the target key, relation and confidence.
const OPERATION: u16 = 14;
const INLINE_VERSION: u16 = 1;
// v2 replaces the u32-length-prefixed body with its u64 byte extent and
// canonical raw SHA-256. Remaining fields are byte-for-byte the v1 layout.
const REFERENCED_VERSION: u16 = 2;
/// Largest raw UTF-8 body accepted by the direct concept command.
pub const MAX_DIRECT_BODY_BYTES: usize = 256 * 1024;

fn request_identity(
    request: &DirectCreate<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<([u8; 32], Option<[u8; 32]>), CreateError> {
    let draft = &request.draft;
    if draft.body.len() <= typed::MAX_PAYLOAD as usize {
        match encode_payload(draft, Some(request.initial_relations), None, ledger) {
            Ok(payload) => {
                let (_, digest) = frame_and_digest_with_version(
                    OPERATION,
                    INLINE_VERSION,
                    &payload,
                    request.read_set,
                    request.expect_version,
                    ledger,
                )?;
                return Ok((digest, None));
            }
            Err(CreateError::Field("payload")) => {}
            Err(error) => return Err(error),
        }
    }
    if extent(draft.body.len())? > MAX_DIRECT_BODY_BYTES as u64 {
        return Err(CreateError::Field("body"));
    }
    let body_ref = map_format(typed::raw_hash(draft.body.as_bytes(), ledger))?;
    let payload = encode_payload(
        draft,
        Some(request.initial_relations),
        Some(body_ref),
        ledger,
    )?;
    let (_, digest) = frame_and_digest_with_version(
        OPERATION,
        REFERENCED_VERSION,
        &payload,
        request.read_set,
        request.expect_version,
        ledger,
    )?;
    Ok((digest, Some(body_ref)))
}

/// One authored relation from the new Term, in input declaration order.
#[derive(Clone, Copy, Debug)]
pub struct InitialRelation {
    /// Existing active target.
    pub target: RecordKey,
    /// Authored relation.
    pub relation: RelationId,
    /// Authored confidence.
    pub confidence: Confidence,
}

/// A direct concept creation and exactly one revision baseline per distinct target.
#[derive(Debug)]
pub struct DirectCreate<'a> {
    /// Metadata and Markdown of the new Term. Attribution must be empty because
    /// direct creation records the authenticated principal and reason.
    pub draft: Draft<'a>,
    /// All initial assertions, in authored order.
    pub initial_relations: &'a [InitialRelation],
    /// Sorted by target key, with no duplicates or extraneous keys.
    pub read_set: &'a [RevPair],
    /// Whole-KB version observed by a caller that prevalidated a snapshot.
    pub expect_version: Option<u64>,
}

fn check_targets(
    store: &Store<impl FileSystem>,
    request: &DirectCreate<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CreateError> {
    let mut previous = None;
    let mut used = admitted_vec(request.read_set.len(), ledger)?;
    for pair in request.read_set {
        ledger.checkpoint().map_err(CommandError::from)?;
        if previous == Some(pair.key) || pair.key.kind() != RecordKind::Term {
            return Err(CreateError::Field("read_set"));
        }
        previous = Some(pair.key);
        let Some(FoldedRecord::Term(term)) = folded_current(store, pair.key, ledger)? else {
            return Err(CommandError::Input(InputError::MissingSubject(pair.key)).into());
        };
        if term.retired {
            return Err(CommandError::Input(InputError::SubjectRetired(pair.key)).into());
        }
        if term.revision != pair.revision {
            return Err(CommandError::Input(InputError::StaleBaseline {
                key: pair.key,
                expected: pair.revision,
            })
            .into());
        }
        used.push(false);
    }

    let mut used_count = 0;
    for relation in request.initial_relations {
        let mut remaining = 0..request.read_set.len();
        let index = loop {
            if remaining.is_empty() {
                return Err(CreateError::Field("read_set"));
            }
            let mid = remaining.start + remaining.len() / 2;
            ledger.try_charge_entries(1).map_err(CommandError::from)?;
            ledger.checkpoint().map_err(CommandError::from)?;
            match request.read_set[mid].key.cmp(&relation.target) {
                std::cmp::Ordering::Less => remaining.start = mid + 1,
                std::cmp::Ordering::Greater => remaining.end = mid,
                std::cmp::Ordering::Equal => break mid,
            }
        };
        if !used[index] {
            used[index] = true;
            used_count += 1;
        }
    }
    if used_count != request.read_set.len() {
        return Err(CreateError::Field("read_set"));
    }
    Ok(())
}

/// Commit one active Term, all initial assertions and the durable result atomically.
///
/// This is a kernel-only command. The caller supplies the KB-scoped direct-creation
/// grant and committed revision baselines for the existing targets.
///
/// # Errors
/// Refuses unauthorized, invalid, conflicting, resource-bounded or unwritable requests.
#[allow(clippy::too_many_arguments)]
pub fn create_direct(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    request: DirectCreate<'_>,
    budget: Budget,
    control: &dyn QueryControl,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CreateError> {
    let authorized = grants
        .authorize(store.kb_uuid(), Capability::WriteConcept, ledger)
        .map_err(CommandError::from)?;
    let principal = authorized.principal();
    check_meta(&meta, principal).map_err(CommandError::from)?;
    if !request.draft.declared_author.is_empty() {
        return Err(CreateError::Field("declared_author"));
    }
    let (digest, body_ref) = request_identity(&request, ledger)?;
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
    if slug_exists(store, basis.basis, request.draft.slug, ledger)? {
        return Err(CreateError::SlugExists);
    }
    if let Some(expected) = request.expect_version
        && Some(expected) != store.head()
    {
        return Err(CommandError::Store(HandleRefusal::ExpectVersionMismatch {
            expected,
            found: store.head(),
        })
        .into());
    }
    check_targets(store, &request, ledger)?;

    let term_id = u32::try_from(basis.allocated_high_water)
        .ok()
        .and_then(|mark| mark.checked_add(1))
        .ok_or(CreateError::IdExhausted(IdSpace::TermId))?;
    let key = RecordKey::term(term_id);
    if folded_current(store, key, ledger)?.is_some() {
        return Err(CreateError::IdCollision(key));
    }
    let mut counters = [Watermark {
        space: IdSpace::TermId,
        hwm: u128::from(term_id),
    }; 2];
    let mut assertions = admitted_vec(request.initial_relations.len(), ledger)?;
    if !request.initial_relations.is_empty() {
        let before = store
            .commit_basis(IdSpace::AssertionId, ledger)
            .map_err(CommandError::from)?;
        if before.basis != basis.basis {
            return Err(CommandError::BasisDivergence.into());
        }
        let mut next = before.allocated_high_water;
        for relation in request.initial_relations {
            ledger.checkpoint().map_err(CommandError::from)?;
            next = next
                .checked_add(1)
                .ok_or(CreateError::IdExhausted(IdSpace::AssertionId))?;
            let assertion = RecordKey::new(RecordKind::Assertion, next);
            if folded_current(store, assertion, ledger)?.is_some() {
                return Err(CreateError::IdCollision(assertion));
            }
            assertions.push(AssertionRecord {
                id: AssertionId::new(next),
                revision: RecordRevision::FIRST,
                from: candidate::term_id(key)?,
                target: candidate::term_id(relation.target)?,
                relation: relation.relation,
                confidence: relation.confidence,
            });
        }
        counters[1] = Watermark {
            space: IdSpace::AssertionId,
            hwm: next,
        };
    }
    let counters = &counters[..1 + usize::from(!assertions.is_empty())];
    candidate::validate(
        store,
        &basis,
        Addition::TermWithAssertions(
            TermMeta {
                id: candidate::term_id(key)?,
                layer: request.draft.layer,
            },
            &assertions,
        ),
        budget,
        control,
        ledger,
    )
    .map_err(|error| match error {
        CandidateError::Command(error) => CreateError::Command(error),
        CandidateError::Invalid(violations) => CreateError::InvalidCandidate(violations),
        CandidateError::Validation(error) => CreateError::Validation(error),
    })?;

    let version = version_next(store.head())?;
    let owner_count = 1 + assertions.len();
    let mut events = admitted_vec(owner_count + counters.len() + 1, ledger)?;
    let mut affected = admitted_vec(owner_count, ledger)?;
    let mut blobs = admitted_vec(3, ledger)?;
    declare(
        &mut blobs,
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
    )?;

    let draft = request.draft;
    let body = admitted_bytes(draft.body.as_bytes(), ledger)?;
    let body_ref = match body_ref {
        Some(body_ref) => body_ref,
        None => map_format(typed::raw_hash(&body, ledger))?,
    };
    let term = TermRecord {
        key,
        revision: RecordRevision::FIRST,
        retired: false,
        slug: admitted_string(draft.slug, ledger)?,
        atom: admitted_string(draft.atom, ledger)?,
        body_ref,
        layer: draft.layer,
        domains: copy_strings(draft.domains, ledger)?,
        triggers: copy_strings(draft.triggers, ledger)?,
        confidence: draft.confidence,
        source: draft.source,
        common: audit(principal, draft.reason, meta.created_at, ledger)?,
        legacy_id: None,
        legacy_source_path: None,
        legacy_content_digest: None,
        legacy_source_file_digest: None,
        legacy_source_revision: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    events.push(map_format(typed::prepare_event(
        EventInput::Term(typed::TermEvent::Created, &term),
        ledger,
    ))?);
    declare(&mut blobs, blob(term.body_ref, body)?)?;
    affected.push(RevPair {
        key,
        revision: RecordRevision::FIRST,
    });

    for (index, assertion) in assertions.iter().enumerate() {
        let declaration_order =
            u32::try_from(index).map_err(|_| CreateError::Field("relations"))?;
        let row = AssertionRecordFull {
            key: RecordKey::new(RecordKind::Assertion, assertion.id.get()),
            revision: assertion.revision,
            retracted: false,
            from: key,
            target: RecordKey::term(assertion.target.get()),
            relation: assertion.relation,
            confidence: assertion.confidence,
            declaration_order,
            common: audit(principal, draft.reason, meta.created_at, ledger)?,
            legacy_id: None,
            legacy_content_digest: None,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        events.push(map_format(typed::prepare_event(
            EventInput::Assertion(typed::AssertionEvent::Created, &row),
            ledger,
        ))?);
        affected.push(RevPair {
            key: row.key,
            revision: row.revision,
        });
    }
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
        principal,
        meta.request_id,
        digest,
        result_ref,
        meta.created_at,
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
        tx_id: meta.tx_id,
        kb_uuid: store.kb_uuid(),
        created_at: meta.created_at,
        principal_id: admitted_string(principal, ledger)?,
        request_id: admitted_string(meta.request_id, ledger)?,
        payload_digest: digest,
        expect_version: request.expect_version,
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

fn copy_strings(
    values: &[String],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<String>, CommandError> {
    let mut copied = admitted_vec(values.len(), ledger)?;
    for value in values {
        copied.push(admitted_string(value, ledger)?);
    }
    Ok(copied)
}
