//! Independent public codec boundary matrix for the four collection families.
use sage_db_format::{
    collection::*,
    event,
    wire::{DecodeError, ReadError, Reader, WriteError, Writer},
};
use sage_db_types::{
    identity::RecordRevision as Revision,
    ledger::{BudgetError, SharedLedger},
    persist::*,
    query::ControlStop,
};
#[derive(Default)]
struct Open {
    bytes: Vec<u64>,
}
impl SharedLedger for Open {
    fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
        self.bytes.push(n);
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}
fn common() -> CommonFields {
    CommonFields {
        recorded_by: None,
        actor_origin: ActorOrigin::Unavailable,
        reason: None,
        created_at: 1,
        updated_at: None,
    }
}
fn vector() -> VectorRecord {
    VectorRecord {
        key: RecordKey::new(RecordKind::Vector, 1),
        revision: Revision::FIRST,
        status: VectorStatus::Active,
        subject: RecordKey::term(1),
        subject_revision: Revision::FIRST,
        model_id: "m".into(),
        dimensions: 1,
        input_digest: [1; 32],
        values_ref: [2; 32],
        embedding_revision: 1,
        common: common(),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn proposal() -> ProposalRecord {
    ProposalRecord {
        key: RecordKey::new(RecordKind::Proposal, 1),
        revision: Revision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ClaimEdit,
        subject: RecordKey::term(1),
        base_revisions: vec![RevPair {
            key: RecordKey::term(1),
            revision: Revision::FIRST,
        }],
        payload_ref: [1; 32],
        review_note: None,
        declared_author: "a".into(),
        common: common(),
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn frontier() -> FrontierEntryRecord {
    FrontierEntryRecord {
        key: RecordKey::new(RecordKind::FrontierEntry, 1),
        revision: Revision::FIRST,
        status: FrontierStatus::Open,
        concept_refs: vec![RecordKey::term(1)],
        text_ref: [1; 32],
        priority: 1,
        common: common(),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn checkpoint() -> SessionCheckpointRecord {
    SessionCheckpointRecord {
        key: RecordKey::new(RecordKind::SessionCheckpoint, 1),
        revision: Revision::FIRST,
        location: sage_db_types::persist::CheckpointLocation::Term {
            concept: RecordKey::term(1),
            session_no: 1,
        },
        content_ref: [1; 32],
        common: common(),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn encode(f: impl FnOnce(&mut Writer<'_>) -> Result<(), WriteError>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut l = Open::default();
    f(&mut Writer::new(&mut out, &mut l)).unwrap();
    out
}
fn edit(
    bytes: &[u8],
    tag: Option<u16>,
    drop: Option<u16>,
    replace: Option<(u16, Vec<u8>)>,
) -> Vec<u8> {
    let mut l = Open::default();
    let e = event::read_event(&mut Reader::new(bytes), &mut l).unwrap();
    let mut fields: Vec<(u16, Vec<u8>)> = e
        .fields()
        .iter()
        .filter(|f| Some(f.tag()) != drop && replace.as_ref().is_none_or(|r| r.0 != f.tag()))
        .map(|f| (f.tag(), f.value().to_vec()))
        .collect();
    if let Some(pair) = replace {
        fields.push(pair)
    }
    fields.sort_by_key(|p| p.0);
    let mut out = Vec::new();
    out.extend_from_slice(&tag.unwrap_or(e.tag()).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (t, v) in fields {
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(&v)
    }
    out
}
fn raw_key(key: RecordKey) -> Vec<u8> {
    let mut v = key.kind().as_u16().to_le_bytes().to_vec();
    v.extend_from_slice(&key.id().to_le_bytes());
    v
}

#[test]
fn upsert_can_store_a_later_revision() {
    let mut r = vector();
    r.revision = Revision::new(2).unwrap();
    r.common.updated_at = Some(2);
    let mut out = Vec::new();
    let mut l = Open::default();
    write_vector(&mut Writer::new(&mut out, &mut l), false, &r).unwrap();
    let (_, got) = read_vector(&mut Reader::new(&out), &mut l).unwrap();
    assert_eq!(got.revision.get(), 2);
}
#[test]
fn later_revision_requires_update_time_in_all_writers() {
    let mut v = vector();
    v.revision = Revision::new(2).unwrap();
    v.status = VectorStatus::Retired;
    v.common.reason = Some("retire".into());
    let mut p = proposal();
    p.revision = Revision::new(2).unwrap();
    p.status = ProposalStatus::Approved;
    p.common.reason = Some("approve".into());
    p.review_note = Some("ok".into());
    let mut f = frontier();
    f.revision = Revision::new(2).unwrap();
    let mut c = checkpoint();
    c.revision = Revision::new(2).unwrap();
    let mut l = Open::default();
    let results = [
        write_vector(&mut Writer::new(&mut Vec::new(), &mut l), true, &v),
        write_proposal(&mut Writer::new(&mut Vec::new(), &mut l), true, &p),
        write_frontier(&mut Writer::new(&mut Vec::new(), &mut l), &f),
        write_checkpoint(&mut Writer::new(&mut Vec::new(), &mut l), &c),
    ];
    assert!(
        results.iter().all(Result::is_err),
        "missing tag9 [vector,proposal,frontier,checkpoint]:{results:?}"
    );
}
#[test]
fn vector_retire_reader_requires_time_and_reason() {
    let mut r = vector();
    r.revision = Revision::new(2).unwrap();
    r.status = VectorStatus::Retired;
    r.common.updated_at = Some(2);
    r.common.reason = Some("r".into());
    let bytes = encode(|w| write_vector(w, true, &r));
    let results=[7,9].map(|tag|{let raw=edit(&bytes,None,Some(tag),None);matches!(read_vector(&mut Reader::new(&raw),&mut Open::default()),Err(ReadError::Data(DecodeError::MissingField{field,..}))if field==tag)});
    assert_eq!(results, [true, true], "[reason,time] checks");
}
#[test]
fn proposal_decision_requires_review_note() {
    let mut r = proposal();
    r.revision = Revision::new(2).unwrap();
    r.status = ProposalStatus::Approved;
    r.common.reason = Some("r".into());
    r.common.updated_at = Some(2);
    let mut l = Open::default();
    let mut out = Vec::new();
    assert!(
        write_proposal(&mut Writer::new(&mut out, &mut l), true, &r).is_err(),
        "missing116 accepted"
    );
}
#[test]
fn proposal_reader_requires_update_time() {
    let mut r = proposal();
    r.revision = Revision::new(2).unwrap();
    r.status = ProposalStatus::Approved;
    r.common.reason = Some("r".into());
    r.common.updated_at = Some(2);
    r.review_note = Some("ok".into());
    let bytes = encode(|w| write_proposal(w, true, &r));
    let raw = edit(&bytes, None, Some(9), None);
    assert!(read_proposal(&mut Reader::new(&raw), &mut Open::default()).is_err());
}
#[test]
fn vector_subject_and_generation_are_constrained_on_read() {
    let bytes = encode(|w| write_vector(w, false, &vector()));
    let raw = edit(
        &bytes,
        None,
        None,
        Some((96, raw_key(RecordKey::new(RecordKind::Vector, 1)))),
    );
    assert!(
        read_vector(&mut Reader::new(&raw), &mut Open::default()).is_err(),
        "Vector subject kind6 accepted"
    );
}
#[test]
fn embedding_generation_zero_is_refused() {
    let mut r = vector();
    r.embedding_revision = 0;
    let mut l = Open::default();
    let mut out = Vec::new();
    assert!(
        write_vector(&mut Writer::new(&mut out, &mut l), false, &r).is_err(),
        "generation0 accepted"
    );
}
#[test]
fn proposal_subject_is_term_or_claim() {
    let bytes = encode(|w| write_proposal(w, false, &proposal()));
    let raw = edit(
        &bytes,
        None,
        None,
        Some((113, raw_key(RecordKey::new(RecordKind::Evidence, 1)))),
    );
    assert!(
        read_proposal(&mut Reader::new(&raw), &mut Open::default()).is_err(),
        "evidence subject accepted"
    );
}
#[test]
fn proposal_author_has_800_byte_bound() {
    let mut r = proposal();
    r.declared_author = "a".repeat(800);
    let mut l = Open::default();
    let mut out = Vec::new();
    write_proposal(&mut Writer::new(&mut out, &mut l), false, &r).unwrap();
}
#[test]
fn proposal_writer_limits_baseline_count() {
    let mut r = proposal();
    r.base_revisions = (1..=257)
        .map(|id| RevPair {
            key: RecordKey::term(id),
            revision: Revision::FIRST,
        })
        .collect();
    let mut l = Open::default();
    let mut out = Vec::new();
    assert!(
        write_proposal(&mut Writer::new(&mut out, &mut l), false, &r).is_err(),
        "257 pairs accepted"
    );
}
#[test]
fn frontier_writer_refuses_disorder() {
    let mut r = frontier();
    r.concept_refs = vec![RecordKey::term(2), RecordKey::term(1)];
    let mut l = Open::default();
    let mut out = Vec::new();
    assert!(
        write_frontier(&mut Writer::new(&mut out, &mut l), &r).is_err(),
        "unsorted keys accepted"
    );
}
#[test]
fn frontier_priority_is_closed_on_read() {
    let bytes = encode(|w| write_frontier(w, &frontier()));
    let raw = edit(&bytes, None, None, Some((130, 3u16.to_le_bytes().to_vec())));
    assert!(
        read_frontier(&mut Reader::new(&raw), &mut Open::default()).is_err(),
        "priority3 accepted"
    );
}
#[test]
fn single_event_decoders_check_event_tag() {
    let f = encode(|w| write_frontier(w, &frontier()));
    let f = edit(&f, Some(1), None, None);
    let f_rejected = read_frontier(&mut Reader::new(&f), &mut Open::default()).is_err();
    let c = encode(|w| write_checkpoint(w, &checkpoint()));
    let c = edit(&c, Some(1), None, None);
    let c_rejected = read_checkpoint(&mut Reader::new(&c), &mut Open::default()).is_err();
    assert_eq!(
        [f_rejected, c_rejected],
        [true, true],
        "tag1 refused by [frontier,checkpoint]"
    );
}
#[test]
fn checkpoint_does_not_invent_status() {
    let bytes = encode(|w| write_checkpoint(w, &checkpoint()));
    let e = event::read_event(&mut Reader::new(&bytes), &mut Open::default()).unwrap();
    assert!(
        e.field(4).is_none(),
        "status has no kind9 domain or checkpoint field"
    );
}
#[test]
fn checkpoint_without_status_decodes() {
    let bytes = encode(|w| write_checkpoint(w, &checkpoint()));
    let raw = edit(&bytes, None, Some(4), None);
    read_checkpoint(&mut Reader::new(&raw), &mut Open::default()).unwrap();
}
#[test]
fn retained_pair_table_uses_rust_slot_size() {
    let mut r = proposal();
    r.base_revisions = (1..=3)
        .map(|id| RevPair {
            key: RecordKey::term(id),
            revision: Revision::FIRST,
        })
        .collect();
    let bytes = encode(|w| write_proposal(w, false, &r));
    let mut l = Open::default();
    read_proposal(&mut Reader::new(&bytes), &mut l).unwrap();
    let wanted = (3 * std::mem::size_of::<RevPair>()) as u64;
    assert!(
        l.bytes.contains(&wanted),
        "slot bytes{wanted},charges{:?}",
        l.bytes
    );
}
#[test]
fn retained_key_table_uses_rust_slot_size() {
    let mut r = frontier();
    r.concept_refs = (1..=3).map(RecordKey::term).collect();
    let bytes = encode(|w| write_frontier(w, &r));
    let mut l = Open::default();
    read_frontier(&mut Reader::new(&bytes), &mut l).unwrap();
    let wanted = (3 * std::mem::size_of::<RecordKey>()) as u64;
    assert!(
        l.bytes.contains(&wanted),
        "slot bytes{wanted},charges{:?}",
        l.bytes
    );
}
