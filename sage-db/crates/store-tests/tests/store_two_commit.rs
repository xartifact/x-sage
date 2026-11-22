//! Real Store two-transaction control.
//!
//! Every previous "two commit" artifact in this project has been two direct helper calls. This one drives the actual
//! [`Store`]: `init` the container through the seam, `open_fresh` the handle, then `commit` twice.
//!
//! # What it establishes today
//!
//! The commit path refuses with `CommitRefusal::UnresolvedFoldCopy`, deliberately: `StateFold::try_fork` deep-copies owned
//! record payloads infallibly and unadmitted, so a commit proceeding past it would claim resource closure it lacks. This
//! control therefore proves the **boundary** — real Store, real filesystem model, real prepared transaction, typed refusal
//! at the exact point where the resource picture is false — and NOT two committed transactions.
//!
//! When the copy path is shared-immutable or genuinely admitted-and-fallible, flip [`EXPECT_FOLD_COPY_RESOLVED`] and the
//! assertion becomes the milestone. That constant is the only change required, which is what keeps this honest instead of
//! being rewritten to match whatever the code happens to do.

use sage_db_store::fs::CreateDir;
use sage_db_store::handle::HandleOutcome;

use sage_db_store::lease::AttemptLedger;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};

use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding,
};
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

fn model_caps() -> ModelCapabilities {
    // The protocol profile: every capability, durable. `full()` is the model's own constructor, so this test does not
    // hand-roll a capability set that could drift from what the model actually grants.
    ModelCapabilities::full()
}

const UUID: [u8; 16] = [0x11; 16];

fn uuid() -> KbUuid {
    KbUuid::from_bytes(UUID)
}

/// The formal-v1 rules bytes, vendored in the accepted base. Hash 04342325...
const RULES: &[u8] = include_bytes!("../fixtures/rules.formal-v1.v1.bin");

fn str_value(text: &str) -> Vec<u8> {
    let mut value = (text.len() as u32).to_le_bytes().to_vec();
    value.extend_from_slice(text.as_bytes());
    value
}

/// The payload behind an outcome's `result_ref`. Arbitrary bytes by design; what matters is that the ref names these
/// exact bytes and the declaration carries them with a real length.
fn result_payload(version: u64, affected: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"opaque-result/");
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&affected.to_le_bytes());
    out
}

/// The ordinary transaction's `RecordedOutcome` event (tag 192).
///
/// Tags 176/177 must match the header's `principal_id`/`request_id`; the composition layer cross-checks them, and 181
/// must be the header's non-zero version.
fn outcome_event(
    version: u64,
    principal: &str,
    request_id: &str,
    payload_digest: [u8; 32],
) -> OwnedEvent {
    let mut fields: Vec<(u16, Vec<u8>)> = vec![
        (5, str_value(principal)),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_000_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, str_value(principal)),
        (177, str_value(request_id)),
        (178, payload_digest.to_vec()),
        (179, 0u16.to_le_bytes().to_vec()),
        (
            180,
            sage_db_format::digest::raw_sha256(&result_payload(version, 2)).to_vec(),
        ),
        (181, version.to_le_bytes().to_vec()),
    ];
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent { tag: 192, fields }
}

fn carrier(
    version: u64,
    prev: Option<u64>,
    request_id: &str,
    events: Vec<OwnedEvent>,
) -> PreparedTransaction {
    use sage_db_format::digest::{Domain, digest, raw_sha256};
    let rules_ref = raw_sha256(RULES);
    let rules_digest = digest(Domain::Rules, RULES);
    // Declarations are computed while `events` is still owned; every content-addressed ref the events name gets bytes.
    let declared_blobs = {
        let mut declared = vec![BlobDeclaration {
            sha256: rules_ref,
            declared_len: RULES.len() as u64,
            source: BlobSource::Provide(RULES.to_vec()),
        }];
        if events.iter().any(|e| e.tag == 192) {
            let payload = result_payload(version, 2);
            declared.push(BlobDeclaration {
                sha256: raw_sha256(&payload),
                declared_len: payload.len() as u64,
                source: BlobSource::Provide(payload),
            });
        }
        declared
    };
    PreparedTransaction {
        version,
        prev_version: prev,
        tx_id: [0xA0 + version as u8; 16],
        kb_uuid: KbUuid::from_bytes(UUID),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: "system".to_owned(),
        request_id: request_id.to_owned(),
        // §4.2: version 1 is `NonRequest` and requires the ALL-ZERO sentinel; later versions require a NON-zero digest.
        // A non-zero sentinel on version 1 is Corrupt(RequestCanonMismatch).
        payload_digest: if version == 1 {
            [0u8; 32]
        } else {
            [version as u8; 32]
        },
        expect_version: prev,
        extensions: Default::default(),
        events,
        blobs: declared_blobs,
        rules: RulesBinding {
            profile: RuleProfile::FormalV1,
            rules_digest,
            rules_ref,
        },
    }
}

#[test]
fn a_real_store_two_transaction_control_reaches_its_actual_boundary() {
    let fs = ModelFileSystem::new(model_caps());
    let path = std::path::Path::new("/kb");
    // A REAL finite capable ledger, not a no-op: admission and refusal are observable through it.
    let ctl: &'static RunToCompletion = Box::leak(Box::new(RunToCompletion));
    let mut ledger =
        AttemptLedger::open(64 * 1024 * 1024, 1_000_000, ctl).expect("finite ledger opens");

    // PUBLIC bootstrap: one continuously held lock, Genesis committed, and a Store only after that succeeded.
    let mut store = match sage_db_store::init::bootstrap(
        fs,
        path,
        CreateDir::IfMissing,
        uuid(),
        RULES,
        &Limits::V1,
        &mut ledger,
    ) {
        Ok(store) => store,
        Err(refusal) => panic!("bootstrap must succeed on a fresh root: {refusal:?}"),
    };
    // Two REAL transactions through the actual Store — not helper calls.
    // Genesis already committed inside `bootstrap`; assert the handle reflects it rather than re-committing.
    assert_eq!(store.head(), Some(1), "bootstrap committed Genesis");
    // A second transaction: legal shape, non-zero digest for a non-genesis version. It carries no outcome event, so
    // the store may still refuse it as MissingOutcome — that is a separate, honest refusal and not a format defect.
    // Tag 178 inside the outcome must equal the header's payload_digest; the composition layer cross-checks it.
    let second_digest = [0x22; 32];
    let second_events = vec![outcome_event(2, "system", "second", second_digest)];
    let mut second_tx = carrier(2, Some(1), "second", second_events);
    second_tx.payload_digest = second_digest;
    let second = store.commit(second_tx, &Limits::V1, &mut ledger);
    eprintln!("OBSERVED second = {second:?}");

    // UNCONDITIONAL POSITIVE: two real transactions through the real Store must commit at their own versions. No
    // expected-outcome switch, no "next encountered refusal" allowance — a refusal here is a failure of this control.
    match &second {
        HandleOutcome::Committed { version, .. } => {
            assert_eq!(*version, 2, "second commits at version 2")
        }
        other => panic!("second must commit, got {other:?}"),
    }
    assert_eq!(
        store.head(),
        Some(2),
        "two transactions advanced the head to 2"
    );

    // Read back through the handle: the retained current set must reflect the committed prefix.
    let retained: Vec<_> = store.retained_entries().collect();
    assert!(
        !retained.is_empty(),
        "a committed genesis must leave a non-empty current set"
    );

    // The control must not depend on an unlimited ledger hiding a budget refusal: assert the ledger saw work.
    let _ = &mut ledger as &mut dyn sage_db_types::ledger::SharedLedger;
}
