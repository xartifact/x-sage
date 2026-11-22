//! Independent process fixture. Canonical projection is reused as a byte-format authority;
//! Genesis rows are an explicitly labelled preservation baseline, not a new golden serializer.
use sage_db_format::{
    digest::{Domain, digest, raw_sha256},
    projection::project_event,
};
use sage_db_store::{fold::FoldedRecord, lease::AttemptLedger};
use sage_db_types::{
    framing::{FormatOutcome, Limits},
    identity::KbUuid,
    persist::StateEntry,
    prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding},
    query::RunToCompletion,
    relation::RuleProfile,
};

pub(crate) const RULES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/rules.formal-v1.v1.bin"
));
pub(crate) const UUID: [u8; 16] = [0x5a; 16];
pub(crate) const ROOT: &str = "/p16-root-process-fixture";
pub(crate) const PRINCIPAL: &str = "process-actor";
pub(crate) const BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const ENTRIES: u64 = 2_000_000;
pub(crate) fn kb() -> KbUuid {
    KbUuid::from_bytes(UUID)
}
pub(crate) fn request(version: u64) -> &'static str {
    match version {
        2 => "base-2",
        3 => "candidate-3",
        _ => panic!("fixture version"),
    }
}
fn text(s: &str) -> Vec<u8> {
    let mut out = (s.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(s.as_bytes());
    out
}
pub(crate) fn body(id: u128, revision: u64) -> Vec<u8> {
    format!("p16-process-body/{id}/{revision}/complete-afterimage").into_bytes()
}
pub(crate) fn result(version: u64) -> Vec<u8> {
    format!("p16-process-result/{version}").into_bytes()
}
pub(crate) fn term(id: u128, revision: u64) -> OwnedEvent {
    let mut key = 1u16.to_le_bytes().to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    let mut domains = 1u32.to_le_bytes().to_vec();
    domains.extend_from_slice(&text("process-test"));
    let mut triggers = 1u32.to_le_bytes().to_vec();
    triggers.extend_from_slice(&text("restart"));
    let mut extensions = 1u32.to_le_bytes().to_vec();
    extensions.extend_from_slice(&text("process.note"));
    extensions.extend_from_slice(&text(&format!("term-{id}-revision-{revision}-extension")));
    let mut fields = vec![
        (1, key),
        (2, revision.to_le_bytes().to_vec()),
        (4, 0u16.to_le_bytes().to_vec()),
        (5, text(PRINCIPAL)),
        (6, 0u16.to_le_bytes().to_vec()),
        (7, text("complete fixture reason")),
        (8, (id as i64).to_le_bytes().to_vec()),
        (14, extensions),
        (16, text(&format!("term-{id}-revision-{revision}"))),
        (17, text("process fixture atom")),
        (18, vec![1]),
        (19, domains),
        (20, 0u16.to_le_bytes().to_vec()),
        (21, 2u16.to_le_bytes().to_vec()),
        (22, triggers),
        (23, raw_sha256(&body(id, revision)).to_vec()),
    ];
    if revision > 1 {
        fields.push((9, 99i64.to_le_bytes().to_vec()));
    }
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent {
        tag: if revision == 1 { 1 } else { 2 },
        fields,
    }
}
pub(crate) fn outcome(version: u64) -> OwnedEvent {
    let mut fields = vec![
        (5, text(PRINCIPAL)),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_000_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, text(PRINCIPAL)),
        (177, text(request(version))),
        (178, vec![version as u8; 32]),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, raw_sha256(&result(version)).to_vec()),
        (181, version.to_le_bytes().to_vec()),
    ];
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent { tag: 192, fields }
}
pub(crate) fn events(version: u64) -> Vec<OwnedEvent> {
    let mut e = match version {
        2 => vec![term(1, 1), term(2, 1)],
        3 => vec![term(1, 2), term(3, 1)],
        _ => panic!("fixture version"),
    };
    e.push(outcome(version));
    e
}
pub(crate) fn carrier(version: u64) -> PreparedTransaction {
    let events = events(version);
    let records: Vec<(u128, u64)> = match version {
        2 => vec![(1, 1), (2, 1)],
        3 => vec![(1, 2), (3, 1)],
        _ => panic!("fixture version"),
    };
    let mut blobs = vec![BlobDeclaration {
        sha256: raw_sha256(RULES),
        declared_len: RULES.len() as u64,
        source: BlobSource::Provide(RULES.to_vec()),
    }];
    for (id, revision) in records {
        let bytes = body(id, revision);
        blobs.push(BlobDeclaration {
            sha256: raw_sha256(&bytes),
            declared_len: bytes.len() as u64,
            source: BlobSource::Provide(bytes),
        });
    }
    let bytes = result(version);
    blobs.push(BlobDeclaration {
        sha256: raw_sha256(&bytes),
        declared_len: bytes.len() as u64,
        source: BlobSource::Provide(bytes),
    });
    PreparedTransaction {
        version,
        prev_version: Some(version - 1),
        tx_id: [0xa0 + version as u8; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: PRINCIPAL.into(),
        request_id: request(version).into(),
        payload_digest: [version as u8; 32],
        expect_version: Some(version - 1),
        extensions: Default::default(),
        events,
        blobs,
        rules: RulesBinding {
            profile: RuleProfile::FormalV1,
            rules_digest: digest(Domain::Rules, RULES),
            rules_ref: raw_sha256(RULES),
        },
    }
}
pub(crate) fn projected(version: u64) -> Vec<StateEntry> {
    let ctl = RunToCompletion;
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, &ctl).unwrap();
    let mut rows = Vec::new();
    for e in events(version) {
        let fields: Vec<_> = e
            .fields
            .iter()
            .map(|(tag, bytes)| sage_db_format::event::Field::new(*tag, bytes))
            .collect();
        match project_event(e.tag, &fields, &mut rows, &mut ledger) {
            FormatOutcome::Ok(_) => {}
            other => panic!("fixture projection: {other:?}"),
        }
    }
    rows
}
pub(crate) fn expected_rows(genesis: &[StateEntry], head: u64) -> Vec<StateEntry> {
    assert!(matches!(head, 2 | 3));
    let mut rows = genesis.to_vec();
    for version in 2..=head {
        for row in projected(version) {
            if let Some(at) = rows
                .iter()
                .position(|r| r.kind == row.kind && r.key == row.key)
            {
                rows[at] = row;
            } else {
                rows.push(row);
            }
        }
    }
    rows.sort_by(|a, b| (a.kind, a.key.as_slice()).cmp(&(b.kind, b.key.as_slice())));
    rows
}
pub(crate) fn expected_history_a1() -> FoldedRecord {
    let ctl = RunToCompletion;
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, &ctl).unwrap();
    match sage_db_format::prepared::decode_owned_event(&term(1, 1), &Limits::V1, &mut ledger) {
        FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::Term(record)) => {
            FoldedRecord::Term(record)
        }
        other => panic!("fixture history: {other:?}"),
    }
}
pub(crate) fn expected_outcome(version: u64) -> sage_db_types::persist::RequestOutcomeEvent {
    let ctl = RunToCompletion;
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, &ctl).unwrap();
    match sage_db_format::prepared::decode_owned_event(&outcome(version), &Limits::V1, &mut ledger)
    {
        FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::RequestOutcome(record)) => {
            record
        }
        other => panic!("fixture outcome: {other:?}"),
    }
}
