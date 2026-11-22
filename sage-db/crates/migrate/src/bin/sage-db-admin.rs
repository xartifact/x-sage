//! Offline command-line entry points for verified migration, backup and restore.

use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use sage_db_format::digest::{BoundedHashError, raw_sha256, raw_sha256_bounded};
use sage_db_format::rules::CanonicalRules;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_migrate::execute::{
    ExpectedIdentity, StepContext, mark_complete_after_verification, verify_and_attest, write_stage,
};
use sage_db_migrate::intent::ImportStep;
use sage_db_migrate::mapping::{
    ExecutorContext, MappingError, MappingLimits, SourceArchive, SourceArray, SourceLocation,
    prepare_native_mapping,
};
use sage_db_store::backup::{BackupLimits, create, restore};
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap_with_profile;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::os::OsFileSystem;
use sage_db_store::view::ImportReadiness;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::ImportStage;
use sage_db_types::query::RunToCompletion;
use sage_db_types::relation::RuleProfile;

const HELP: &str = "\
sage-db-admin import ROOT KB_UUID PLAN ARCHIVE PLAN_SHA256 SOURCE_SHA256 SESSION
sage-db-admin backup ROOT KB_UUID ARTIFACT [VERSION]
sage-db-admin restore ARTIFACT KB_UUID NEW_ROOT

ROOT, ARTIFACT and NEW_ROOT are absolute filesystem paths. Import and restore
require fresh roots on a qualified filesystem. Stop any process holding ROOT
before backing it up. Import takes an independently verified archive and plan.";
const LIMITS: Limits = Limits::V1;
const INPUT_LIMITS: MappingLimits = MappingLimits {
    max_plan_bytes: 16 * 1024 * 1024,
    max_files: 100_000,
    max_bytes: 256 * 1024 * 1024,
    max_records: 50_000,
};
const PRINCIPAL: &str = "local-import-operator";

fn error(label: &str, reason: impl std::fmt::Debug) -> String {
    format!("{label}: {reason:?}")
}

fn ledger() -> Result<AttemptLedger<'static>, String> {
    AttemptLedger::open(512 * 1024 * 1024, 10_000_000, &RunToCompletion)
        .map_err(|e| error("attempt ledger", e))
}

fn hex<const N: usize>(text: &str) -> Result<[u8; N], String> {
    let text = text
        .strip_prefix("sha256:")
        .filter(|_| N == 32)
        .unwrap_or(text);
    if text.len() != N * 2 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("expected {} hexadecimal characters", N * 2));
    }
    let mut bytes = [0; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|e| error("hexadecimal value", e))?;
    }
    Ok(bytes)
}

fn filesystem(path: &Path) -> Result<OsFileSystem, String> {
    if !path.is_absolute() {
        return Err("an absolute database or backup path is required".into());
    }
    OsFileSystem::for_root(path).map_err(|e| error("filesystem qualification", e))
}

fn ready_store(root: &Path, kb: KbUuid) -> Result<Store<OsFileSystem>, String> {
    match open_existing(filesystem(root)?, root, Some(kb), &LIMITS, &mut ledger()?)
        .map_err(|e| error("open store", e))?
    {
        Opened::Ready(store) => Ok(store),
        Opened::ReadOnly { .. } => Err("store only opened read-only".into()),
    }
}

fn require_fresh(root: &Path) -> Result<(), String> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(error("inspect target", e)),
        Ok(_) => Err("destination already exists".into()),
    }
}

fn read_staged_file(
    path: &Path,
    exact: Option<u64>,
    ceiling: u64,
    location: SourceLocation,
    work: &mut dyn SharedLedger,
) -> Result<Vec<u8>, MappingError> {
    let invalid = |detail| MappingError::InvalidSource { location, detail };
    work.checkpoint()?;
    let entry = fs::symlink_metadata(path).map_err(|_| invalid("file is unavailable"))?;
    if !entry.is_file() {
        return Err(invalid("expected a regular file"));
    }
    let mut file = File::open(path).map_err(|_| invalid("file cannot be opened"))?;
    let size = file
        .metadata()
        .map_err(|_| invalid("file size unavailable"))?
        .len();
    if size > ceiling || exact.is_some_and(|expected| expected != size) {
        return Err(invalid(
            "file size exceeds or disagrees with its declared extent",
        ));
    }
    let count = usize::try_from(size).map_err(|_| invalid("file extent cannot be represented"))?;
    work.try_charge(size)?;
    let mut data = Vec::new();
    data.try_reserve_exact(count)
        .map_err(|_| Refusal::Allocation { requested: count })?;
    if data.capacity() != count {
        return Err(invalid(
            "file reservation disagreed with the admitted extent",
        ));
    }
    let mut buffer = [0; 8192];
    while data.len() < count {
        work.checkpoint()?;
        let chunk = buffer.len().min(count - data.len());
        let read = file
            .read(&mut buffer[..chunk])
            .map_err(|_| invalid("file read failed"))?;
        if read == 0 {
            return Err(invalid("file shortened during read"));
        }
        data.extend_from_slice(&buffer[..read]);
    }
    if file
        .read(&mut buffer[..1])
        .map_err(|_| invalid("file read failed"))?
        != 0
    {
        return Err(invalid("file grew during read"));
    }
    Ok(data)
}

struct DirectoryArchive<'a>(&'a Path);

impl SourceArchive for DirectoryArchive<'_> {
    fn blob(
        &mut self,
        work: &mut dyn SharedLedger,
        path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<Vec<u8>, MappingError> {
        let location = SourceLocation::row(SourceArray::Attachments, 0);
        let invalid = |detail| MappingError::InvalidSource { location, detail };
        let digest = hex::<32>(expected_sha256).map_err(|_| invalid("invalid blob digest"))?;
        if expected_sha256.len() != 64 || path.strip_prefix("blobs/") != Some(expected_sha256) {
            return Err(invalid("invalid archive blob pathname"));
        }
        let bytes = read_staged_file(
            &self.0.join(expected_sha256),
            Some(expected_bytes),
            INPUT_LIMITS.max_bytes,
            location,
            work,
        )?;
        let got = raw_sha256_bounded(&bytes, work).map_err(|e| match e {
            BoundedHashError::Budget(e) => MappingError::from(e),
            BoundedHashError::Stopped(e) => MappingError::from(e),
        })?;
        if got != digest {
            return Err(invalid("archive blob digest mismatch"));
        }
        Ok(bytes)
    }
}

fn import(arguments: &[String]) -> Result<(), String> {
    let [root, kb, plan, archive, plan_sha, source_sha, session] = arguments else {
        return Err(HELP.into());
    };
    let root = Path::new(root);
    let fs = filesystem(root)?;
    require_fresh(root)?;
    let kb = KbUuid::from_bytes(hex(kb)?);
    let expected = ExpectedIdentity {
        plan_sha256: hex(plan_sha)?,
        source_digest: hex(source_sha)?,
        kb_uuid: kb,
        import_session_id: session,
    };
    let blobs = Path::new(archive).join("blobs");
    if !fs::symlink_metadata(&blobs).is_ok_and(|entry| entry.is_dir()) {
        return Err("archive blobs directory is unavailable".into());
    }
    let mut preparation = ledger()?;
    let plan = read_staged_file(
        Path::new(plan),
        None,
        INPUT_LIMITS.max_plan_bytes,
        SourceLocation::plan(),
        &mut preparation,
    )
    .map_err(|e| error("read plan", e))?;
    let plan_digest =
        raw_sha256_bounded(&plan, &mut preparation).map_err(|e| error("hash plan", e))?;
    if plan_digest != expected.plan_sha256 {
        return Err("plan digest mismatch".into());
    }
    let created_at = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| error("clock", e))?
            .as_micros(),
    )
    .map_err(|e| error("clock", e))?;
    let grants = Grants::new(PRINCIPAL, kb, CapabilitySet::of(&[Capability::Admin]));
    let prepared = prepare_native_mapping(
        &plan,
        &mut DirectoryArchive(&blobs),
        &grants,
        kb,
        &ExecutorContext {
            import_time: created_at,
            reason: "offline native import".into(),
            session_id: session.clone(),
        },
        INPUT_LIMITS,
        &mut preparation,
    )
    .map_err(|e| error("prepare native mapping", e))?;
    if prepared.import_session().source_digest != expected.source_digest {
        return Err("source digest mismatch".into());
    }
    let event_count =
        u32::try_from(prepared.events().len()).map_err(|e| error("event count", e))?;
    if event_count > LIMITS.max_events_per_tx - 2 {
        return Err("prepared mapping exceeds the single-transaction import ceiling".into());
    }
    let rules = CanonicalRules::new(RuleProfile::LegacyV1)
        .encode(&mut preparation)
        .map_err(|e| error("encode rules", e))?;
    let mut store = bootstrap_with_profile(
        fs,
        root,
        CreateDir::IfMissing,
        kb,
        (RuleProfile::LegacyV1, &rules),
        &LIMITS,
        &mut ledger()?,
    )
    .map_err(|e| error("bootstrap", e))?;

    let step = |stage| ImportStep {
        package_ref: prepared.package().sha256,
        source_digest: expected.source_digest,
        stage,
        batch_index: 0,
        batch_count: 1,
        first_event: 0,
        event_count: if stage == ImportStage::Import {
            event_count
        } else {
            0
        },
        import_session_id: session,
    };
    let context = |stage: ImportStage| {
        let mut identity = [0; 34];
        identity[..32].copy_from_slice(&plan_digest);
        identity[32..].copy_from_slice(&stage.as_u16().to_le_bytes());
        let mut tx_id = [0; 16];
        tx_id.copy_from_slice(&raw_sha256(&identity)[..16]);
        StepContext {
            principal: PRINCIPAL,
            tx_id,
            created_at,
            limits: &LIMITS,
            expect_version: None,
        }
    };
    for stage in [
        ImportStage::Prepare,
        ImportStage::Validate,
        ImportStage::Import,
        ImportStage::Verify,
    ] {
        let attempt = write_stage(
            &mut store,
            &grants,
            &expected,
            &plan,
            &prepared,
            &step(stage),
            &context(stage),
            &mut ledger()?,
        )
        .map_err(|e| error("import stage refused", (stage, e)))?;
        acknowledge(stage, attempt.outcome)?;
    }
    drop(store);
    let mut store = ready_store(root, kb)?;
    let mut verification = ledger()?;
    let attestation = verify_and_attest(&mut store, &prepared, &expected, &mut verification)
        .map_err(|e| error("verify imported data after reopen", e))?;
    let activation = ImportStage::Activate;
    let attempt = mark_complete_after_verification(
        &mut store,
        &grants,
        &expected,
        &prepared,
        attestation,
        &step(activation),
        &context(activation),
        &mut verification,
    )
    .map_err(|e| error("complete verified import", e))?;
    acknowledge(activation, attempt.outcome)?;
    drop(store);
    let store = ready_store(root, kb)?;
    let result = store
        .import_readiness(&mut ledger()?)
        .map_err(|e| error("read import status", e))?;
    let ImportReadiness::Ready { basis } = result else {
        return Err(error("import did not become ready after reopen", result));
    };
    println!(
        "{}",
        serde_json::json!({"kbUuid": kb.to_string(), "version": basis.version.get(), "importComplete": true})
    );
    Ok(())
}

fn acknowledge(stage: ImportStage, outcome: HandleOutcome) -> Result<(), String> {
    match outcome {
        HandleOutcome::Committed { .. } => Ok(()),
        HandleOutcome::Unknown => Err(format!(
            "{stage:?}: outcome unknown; preserve the target and inspect durable state"
        )),
        other => Err(error("import did not commit", (stage, other))),
    }
}

fn backup(arguments: &[String]) -> Result<(), String> {
    let [root, kb, artifact, rest @ ..] = arguments else {
        return Err(HELP.into());
    };
    let artifact = Path::new(artifact);
    let artifact_fs = filesystem(artifact)?;
    require_fresh(artifact)?;
    let kb = KbUuid::from_bytes(hex(kb)?);
    let mut store = ready_store(Path::new(root), kb)?;
    let version = match rest {
        [] => store.head().ok_or("store has no durable version")?,
        [version] => version.parse::<u64>().map_err(|e| error("version", e))?,
        _ => return Err(HELP.into()),
    };
    if version == 0 {
        return Err("version must be positive".into());
    }
    let report = create(
        &mut store,
        &artifact_fs,
        artifact,
        version,
        &BackupLimits::DEFAULT,
        &mut ledger()?,
    )
    .map_err(|e| error("backup", e))?;
    println!(
        "{}",
        serde_json::json!({"version": report.version, "files": report.files, "bytes": report.bytes})
    );
    Ok(())
}

fn restore_backup(arguments: &[String]) -> Result<(), String> {
    let [artifact, kb, root] = arguments else {
        return Err(HELP.into());
    };
    let (artifact, root) = (Path::new(artifact), Path::new(root));
    require_fresh(root)?;
    let kb = KbUuid::from_bytes(hex(kb)?);
    let (_restored, report) = restore(
        &filesystem(artifact)?,
        artifact,
        filesystem(root)?,
        root,
        kb,
        &BackupLimits::DEFAULT,
        &mut ledger()?,
    )
    .map_err(|e| error("restore", e))?;
    println!(
        "{}",
        serde_json::json!({"kbUuid": report.kb_uuid.to_string(), "version": report.version, "files": report.files})
    );
    Ok(())
}

fn run(arguments: &[String]) -> Result<(), String> {
    match arguments {
        [] => Err(HELP.into()),
        [command, ..]
            if command == "--help"
                || command == "-h"
                || arguments.get(1).is_some_and(|arg| arg == "--help") =>
        {
            println!("{HELP}");
            Ok(())
        }
        [command, args @ ..] => match command.as_str() {
            "import" => import(args),
            "backup" => backup(args),
            "restore" => restore_backup(args),
            _ => Err(HELP.into()),
        },
    }
}

fn main() -> ExitCode {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sage-db-admin: {message}");
            ExitCode::FAILURE
        }
    }
}
