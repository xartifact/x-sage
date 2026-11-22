//! Independent public-boundary probes for the new record families.
use sage_db_format::{
    claim::{self, ClaimEvent},
    event,
    evidence::{self, EvidenceEvent, LinkEvent},
    wire::{EncodeError, Reader, WriteError, Writer},
};
use sage_db_types::{
    confidence::Confidence,
    identity::RecordRevision,
    ledger::{BudgetError, SharedLedger},
    persist::*,
    query::ControlStop,
};

struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
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
fn claim() -> ClaimRecord {
    ClaimRecord {
        key: RecordKey::new(RecordKind::Claim, 1),
        revision: RecordRevision::FIRST,
        status: ClaimStatus::Accepted,
        concept: RecordKey::term(1),
        kind: ClaimKind::Premise,
        text_ref: [1; 32],
        confidence: Confidence::Confirmed,
        common: common(),
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn evidence() -> EvidenceRecord {
    EvidenceRecord {
        key: RecordKey::new(RecordKind::Evidence, 1),
        revision: RecordRevision::FIRST,
        kind: EvidenceKind::Text,
        title: "title".into(),
        locator: None,
        excerpt_ref: None,
        asset_sha256: None,
        captured_at: 1,
        declared_captured_by: None,
        common: common(),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
fn link() -> EvidenceLinkRecord {
    EvidenceLinkRecord {
        key: RecordKey::new(RecordKind::EvidenceLink, 1),
        revision: RecordRevision::FIRST,
        retracted: false,
        evidence: RecordKey::new(RecordKind::Evidence, 1),
        evidence_revision: RecordRevision::FIRST,
        target: RecordKey::term(1),
        target_revision: RecordRevision::FIRST,
        common: common(),
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

#[test]
fn claim_creation_refuses_changed_fields() {
    let mut record = claim();
    record.changed_fields = Some(vec![21]);
    let mut out = Vec::new();
    let mut ledger = Open;
    let got = claim::write_claim(
        &mut Writer::new(&mut out, &mut ledger),
        ClaimEvent::Created,
        &record,
    );
    assert!(
        matches!(
            got,
            Err(WriteError::Data(EncodeError::ForbiddenField {
                field: 13,
                ..
            }))
        ),
        "{got:?}"
    );
    assert!(out.is_empty());
}
#[test]
fn evidence_creation_refuses_changed_fields() {
    let mut record = evidence();
    record.changed_fields = Some(vec![65]);
    let mut out = Vec::new();
    let mut ledger = Open;
    let got = evidence::write_evidence(
        &mut Writer::new(&mut out, &mut ledger),
        EvidenceEvent::Created,
        &record,
    );
    assert!(
        matches!(
            got,
            Err(WriteError::Data(EncodeError::ForbiddenField {
                field: 13,
                ..
            }))
        ),
        "{got:?}"
    );
    assert!(out.is_empty());
}
#[test]
fn link_creation_refuses_changed_fields() {
    let mut record = link();
    record.changed_fields = Some(vec![82]);
    let mut out = Vec::new();
    let mut ledger = Open;
    let got = evidence::write_link(
        &mut Writer::new(&mut out, &mut ledger),
        LinkEvent::Created,
        &record,
    );
    assert!(
        matches!(
            got,
            Err(WriteError::Data(EncodeError::ForbiddenField {
                field: 13,
                ..
            }))
        ),
        "{got:?}"
    );
    assert!(out.is_empty());
}
#[test]
fn link_legacy_id_is_not_silently_dropped() {
    let mut record = link();
    record.legacy_id = Some("original-link".into());
    let mut out = Vec::new();
    let mut ledger = Open;
    evidence::write_link(
        &mut Writer::new(&mut out, &mut ledger),
        LinkEvent::Created,
        &record,
    )
    .unwrap();
    let (_, decoded) = evidence::read_link(&mut Reader::new(&out), &mut ledger).unwrap();
    assert_eq!(decoded.legacy_id, record.legacy_id);
}
#[test]
fn declared_capturer_accepts_its_800_byte_limit_on_write() {
    let mut record = evidence();
    record.declared_captured_by = Some("x".repeat(800));
    let mut out = Vec::new();
    let mut ledger = Open;
    evidence::write_evidence(
        &mut Writer::new(&mut out, &mut ledger),
        EvidenceEvent::Created,
        &record,
    )
    .unwrap();
}
#[test]
fn declared_capturer_accepts_its_800_byte_limit_on_read() {
    let record = evidence();
    let mut out = Vec::new();
    let mut ledger = Open;
    evidence::write_evidence(
        &mut Writer::new(&mut out, &mut ledger),
        EvidenceEvent::Created,
        &record,
    )
    .unwrap();
    let parsed = event::read_event(&mut Reader::new(&out), &mut ledger).unwrap();
    let mut fields: Vec<(u16, Vec<u8>)> = parsed
        .fields()
        .iter()
        .map(|f| (f.tag(), f.value().to_vec()))
        .collect();
    let mut value = 800u32.to_le_bytes().to_vec();
    value.extend_from_slice(&vec![b'x'; 800]);
    fields.push((70, value));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&48u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (tag, value) in fields {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&value);
    }
    let (_, decoded) = evidence::read_evidence(&mut Reader::new(&bytes), &mut ledger).unwrap();
    assert_eq!(
        decoded.declared_captured_by.as_deref(),
        Some("x".repeat(800).as_str())
    );
}
