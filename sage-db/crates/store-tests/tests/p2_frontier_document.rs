//! Canonical document replacement through the real kernel and model store.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{CommandError, RequestMeta};
use sage_db_kernel::frontier::create_frontier;
use sage_db_kernel::frontier_document::{
    FrontierDocumentError, read_frontier_document, replace_frontier_document,
};
use sage_db_kernel::frontier_intent::FrontierState;
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::view::BlobReadError;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::Limits;
use sage_db_types::identity::RecordRevision;
use sage_db_types::query::RunToCompletion;
use sage_db_types::snapshot::SnapshotView;

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn frontier_count(store: &Store<ObservingFs>) -> usize {
    let mut count = 0;
    store
        .scan_current_records(&mut ledger(), |record, _| {
            count += usize::from(matches!(record, FoldedRecord::FrontierEntry(_)));
            Ok::<(), core::convert::Infallible>(())
        })
        .unwrap();
    count
}

#[test]
fn raw_document_is_versioned_hash_verified_and_retryable_after_reopen() {
    let fs = ObservingFs::new(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let path = Path::new(ROOT);
    let mut store = bootstrap(
        fs.clone(),
        path,
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut ledger(),
    )
    .expect("bootstrap historical Genesis");
    let grants = Grants::new(
        fixture::PRINCIPAL,
        kb(),
        CapabilitySet::of(&[Capability::Read, Capability::WriteAux]),
    );
    let read_only = Grants::new(
        fixture::PRINCIPAL,
        kb(),
        CapabilitySet::of(&[Capability::Read]),
    );
    let absent = read_frontier_document(&mut store, &grants, BYTES, &mut ledger()).unwrap();
    assert_eq!(absent.revision, RecordRevision::FIRST);
    assert!(absent.raw.is_none());
    assert!(matches!(
        create_frontier(
            &mut store,
            &grants,
            RequestMeta {
                request_id: "ordinary-frontier",
                tx_id: [2; 16],
                created_at: 1_700_000_000_009_000,
            },
            FrontierState {
                concept_refs: &[],
                text: "a genuine typed Frontier row",
                priority: 1,
                status: 0,
            },
            &[],
            &Limits::V1,
            &mut ledger(),
        ),
        Ok(HandleOutcome::Committed { version: 2, .. })
    ));
    let original_graph = store.published_pin().unwrap().id().graph_digest;
    let original_frontier_rows = frontier_count(&store);
    assert_eq!(original_frontier_rows, 1);
    let (before, _) = store.kb_metadata(&mut ledger()).unwrap();
    let original_metadata = before.clone();

    let mut original = b"# Frontier\r\n\r\n| id | source | reason | priority |\r\n".to_vec();
    original.extend("原始内容|混合\r\n".as_bytes());
    original.extend(std::iter::repeat_n(b'x', 260 * 1024));
    original.extend_from_slice(b"\r\n| last | s | r | medium |\r\n");
    assert!(original.len() > 256 * 1024);
    let base = RequestMeta {
        request_id: "seed-frontier-document",
        tx_id: [3; 16],
        created_at: 1_700_000_000_010_000,
    };

    let change =
        |store: &mut Store<ObservingFs>, grants: &Grants<'_>, meta, revision, bytes: &[u8]| {
            replace_frontier_document(
                store,
                grants,
                meta,
                revision,
                bytes,
                &Limits::V1,
                &mut ledger(),
            )
        };
    let counts = fs.counts();
    let before_denied = counts.borrow().total();
    assert!(matches!(
        change(&mut store, &read_only, base, absent.revision, &original),
        Err(FrontierDocumentError::Command(CommandError::Auth(_)))
    ));
    assert_eq!(counts.borrow().total(), before_denied);
    assert!(matches!(
        change(&mut store, &grants, base, absent.revision, &original),
        Ok(HandleOutcome::Committed { version: 3, .. })
    ));
    assert!(matches!(
        read_frontier_document(&mut store, &grants, 128 * 1024, &mut ledger()),
        Err(FrontierDocumentError::Command(CommandError::Blob(
            BlobReadError::Ceiling {
                ceiling: 131072,
                ..
            }
        )))
    ));

    let read = read_frontier_document(&mut store, &grants, BYTES, &mut ledger()).unwrap();
    assert_eq!(read.basis.version.get(), 3);
    assert_eq!(read.revision.get(), 2);
    assert_eq!(read.raw.unwrap().as_bytes(), original);
    let (metadata, _) = store.kb_metadata(&mut ledger()).unwrap();
    assert_eq!(metadata.slug, original_metadata.slug);
    assert_eq!(metadata.name, original_metadata.name);
    assert_eq!(metadata.domains, original_metadata.domains);
    assert_eq!(metadata.description, original_metadata.description);
    assert_eq!(metadata.extensions, original_metadata.extensions);
    assert_eq!(
        metadata.legacy_frontier_ref,
        Some(sage_db_format::digest::raw_sha256(&original))
    );
    let next = RequestMeta {
        request_id: "replace-frontier-document",
        tx_id: [4; 16],
        created_at: base.created_at + 1,
    };
    let mut replacement = original.clone();
    replacement.extend_from_slice(b"| another | extra | reason | high |\n");
    assert!(matches!(
        change(&mut store, &grants, next, absent.revision, &replacement),
        Err(FrontierDocumentError::Stale { expected, actual })
            if expected.get() == 1 && actual.get() == 2
    ));
    assert_eq!(store.head(), Some(3));
    assert!(matches!(
        change(&mut store, &grants, next, read.revision, &replacement),
        Ok(HandleOutcome::Committed { version: 4, .. })
    ));
    assert_eq!(frontier_count(&store), original_frontier_rows);
    drop(store);

    let Opened::Ready(mut reopened) =
        open_existing(fs, path, Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("canonical document and historical Genesis reopen");
    };
    let reopened_read =
        read_frontier_document(&mut reopened, &grants, BYTES, &mut ledger()).unwrap();
    assert_eq!(reopened_read.revision.get(), 3);
    assert_eq!(reopened_read.basis.version.get(), 4);
    assert_eq!(reopened_read.raw.unwrap().as_bytes(), replacement);
    assert!(matches!(
        change(&mut reopened, &grants, base, absent.revision, &original),
        Ok(HandleOutcome::AlreadyCommitted { version: 3, .. })
    ));
    assert!(matches!(
        change(&mut reopened, &grants, base, absent.revision, &replacement),
        Err(FrontierDocumentError::Command(
            CommandError::IdempotencyMismatch
        ))
    ));
    assert_eq!(reopened.head(), Some(4));
    assert_eq!(frontier_count(&reopened), original_frontier_rows);
    let snapshot = Snapshot::open(&mut reopened, Expectation::latest(), &mut ledger()).unwrap();
    assert_eq!(snapshot.basis().version.get(), 4);
    assert_eq!(snapshot.basis().graph_digest, original_graph);
}
