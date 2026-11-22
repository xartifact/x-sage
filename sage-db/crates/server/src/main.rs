//! Loopback JSON entry point for an existing, qualified sage-db root.

mod activation;
mod checkpoint_reads;
mod compat_checkpoint;
mod db_graph;
mod frontier_document;
mod graph_viz;
mod http;
mod import_source;
mod legacy_query;
mod manifest_search;
mod operator_read;
mod proposal_files;
mod queue_reads;
mod runtime;

use std::env;
use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;

use sage_db_kernel::auth::{Capability, CapabilitySet};
use sage_db_server::credentials::{
    CredentialBinding, CredentialVerifier, TokenHash, VerifierLimits,
};
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::os::OsFileSystem;
use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::query::RunToCompletion;

const MAX_CREDENTIAL_FILE: usize = 16 * 1024;
const MAX_BINDINGS: usize = 32;

fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    fn nibble(ch: u8) -> Option<u8> {
        match ch {
            b'0'..=b'9' => Some(ch - b'0'),
            b'a'..=b'f' => Some(ch - b'a' + 10),
            _ => None,
        }
    }
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0; N];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

fn bindings(text: &str, kb: KbUuid) -> Option<Vec<CredentialBinding<'_>>> {
    let mut bindings = Vec::new();
    bindings.try_reserve_exact(MAX_BINDINGS).ok()?;
    for line in text.lines() {
        let mut fields = line.split('\t');
        let hash = TokenHash::from_sha256(hex(fields.next()?)?);
        let principal = fields.next()?;
        let capabilities = fields.next()?;
        if fields.next().is_some() || bindings.len() == MAX_BINDINGS {
            return None;
        }
        let mut set = CapabilitySet::NONE;
        for label in capabilities.split(',') {
            let capability = match label {
                "read" => Capability::Read,
                "propose" => Capability::Propose,
                "review" => Capability::Review,
                "write_aux" => Capability::WriteAux,
                "write_concept" => Capability::WriteConcept,
                _ => return None,
            };
            if set.contains(capability) {
                return None;
            }
            set = set.with(capability);
        }
        bindings.push(CredentialBinding::active(hash, principal, kb, set));
    }
    (!bindings.is_empty()).then_some(bindings)
}

fn run() -> Result<(), &'static str> {
    let root = PathBuf::from(env::var_os("SAGE_DB_ROOT").ok_or("SAGE_DB_ROOT is required")?);
    let kb = env::var("SAGE_DB_KB_UUID")
        .ok()
        .and_then(|value| hex(&value))
        .map(KbUuid::from_bytes)
        .ok_or("SAGE_DB_KB_UUID must be 32 lowercase hex digits")?;
    let listen: SocketAddr = env::var("SAGE_DB_LISTEN")
        .map_err(|_| "SAGE_DB_LISTEN is required")?
        .parse()
        .map_err(|_| "invalid SAGE_DB_LISTEN address")?;
    if !listen.ip().is_loopback() {
        return Err("SAGE_DB_LISTEN must be a loopback address");
    }
    let source_identity = option_env!("SAGE_DB_SOURCE_IDENTITY")
        .filter(|&value| hex::<32>(value).is_some())
        .ok_or("build with SAGE_DB_SOURCE_IDENTITY set to the source manifest digest")?;

    let path = env::var_os("SAGE_DB_CREDENTIALS").ok_or("SAGE_DB_CREDENTIALS is required")?;
    let mut input = [0; MAX_CREDENTIAL_FILE + 1];
    let used = File::open(path)
        .and_then(|mut file| {
            let mut used = 0;
            while used < input.len() {
                let len = file.read(&mut input[used..])?;
                if len == 0 {
                    return Ok(used);
                }
                used += len;
            }
            Ok(used)
        })
        .map_err(|_| "cannot read credential file")?;
    if used > MAX_CREDENTIAL_FILE {
        return Err("credential file exceeds its byte limit");
    }
    let text = std::str::from_utf8(&input[..used]).map_err(|_| "invalid credential encoding")?;
    let bindings = bindings(text, kb).ok_or("invalid credential table")?;
    let mut ledger = AttemptLedger::open(256 * 1024 * 1024, 10_000_000, &RunToCompletion)
        .map_err(|_| "cannot open startup ledger")?;
    let verifier = CredentialVerifier::new(&bindings, VerifierLimits::DEFAULT, &mut ledger)
        .map_err(|_| "credential validation refused")?;
    let fs = OsFileSystem::for_root(&root).map_err(|_| "store platform qualification refused")?;
    let opened = open_existing(fs, &root, Some(kb), &Limits::V1, &mut ledger)
        .map_err(|_| "store reopen refused")?;
    let Opened::Ready(store) = opened else {
        return Err("store is not ready to start this service");
    };
    let random = File::open("/dev/urandom").map_err(|_| "cannot open OS entropy source")?;
    manifest_search::initialize();
    let listener = TcpListener::bind(listen).map_err(|_| "cannot bind loopback listener")?;
    let address = listener
        .local_addr()
        .map_err(|_| "cannot read listener address")?;
    println!("SAGE_DB_LISTEN={address}");

    runtime::Service::new(store, verifier, random, source_identity)
        .serve(listener)
        .map_err(|_| "listener stopped")
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sage-db-server: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_file_requires_an_explicit_direct_concept_grant() {
        let kb = KbUuid::from_bytes([1; 16]);
        let mut ledger = AttemptLedger::open(1 << 20, 1024, &RunToCompletion).unwrap();
        for (label, token, can_create) in [
            ("read,propose,review,write_aux", "a", false),
            ("write_concept", "b", true),
        ] {
            let raw = token.repeat(64);
            let digest = sage_db_store::typed::raw_hash(raw.as_bytes(), &mut ledger);
            let sage_db_types::framing::FormatOutcome::Ok(digest) = digest else {
                panic!("test token hash");
            };
            let digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let line = format!("{digest}\tprincipal\t{label}");
            let configured = bindings(&line, kb).unwrap();
            let verifier =
                CredentialVerifier::new(&configured, VerifierLimits::default(), &mut ledger)
                    .unwrap();
            let grant = verifier
                .verify_bearer(raw.as_bytes(), kb, &mut ledger)
                .unwrap();
            assert_eq!(
                grant.capabilities().contains(Capability::WriteConcept),
                can_create
            );
            assert_eq!(grant.capabilities().contains(Capability::Read), !can_create);
        }
    }
}
