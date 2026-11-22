//! Operator-only HTTP and subprocess recovery test on a private, imported KB copy.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use sage_db_store::fold::FoldedRecord;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::os::OsFileSystem;
use sage_db_store::view::RecordLookup;
use sage_db_types::framing::Limits;
use sage_db_types::identity::KbUuid;
use sage_db_types::persist::{RecordKey, RecordKind};
use sage_db_types::query::RunToCompletion;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const KB_TEXT: &str = "77777777777777777777777777777777";
const KB: KbUuid = KbUuid::from_bytes([0x77; 16]);
const NEW_ATOM: &str = "RuntimeProbe ΟΣ İ";
const NEW_BODY: &str = "# Runtime recovery\n\nApproved via HTTP. 中文。\n";

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(256 * 1024 * 1024, 10_000_000, &RunToCompletion).unwrap()
}

fn open(root: &Path) -> Store<OsFileSystem> {
    let fs = OsFileSystem::for_root(root).expect("qualified scratch root");
    let Opened::Ready(store) =
        open_existing(fs, root, Some(KB), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("scratch root must open Ready");
    };
    store
}

fn token() -> String {
    let mut buffer = [0; 32];
    File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut buffer)
        .unwrap();
    hex(&buffer)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").unwrap();
    }
    text
}

struct Process {
    child: Child,
    address: SocketAddr,
}

impl Process {
    fn start(root: &Path, credentials: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sage-db-server"))
            .env("SAGE_DB_ROOT", root)
            .env("SAGE_DB_KB_UUID", KB_TEXT)
            .env("SAGE_DB_LISTEN", "127.0.0.1:0")
            .env("SAGE_DB_CREDENTIALS", credentials)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let address = line
            .trim()
            .strip_prefix("SAGE_DB_LISTEN=")
            .expect("server must announce its listening address")
            .parse()
            .unwrap();
        Self { child, address }
    }

    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }

    fn send(&self, method: &str, token: &str, body: &Value) -> TcpStream {
        let body = serde_json::to_vec(body).unwrap();
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST /sage.v1.SageCommandService/{method} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            self.address, body.len(),
        ).unwrap();
        stream.write_all(&body).unwrap();
        stream
    }

    fn call(&self, method: &str, token: &str, body: &Value) -> (u16, Value) {
        let mut stream = self.send(method, token, body);
        let (status, body) = response(&mut stream, method);
        (status, serde_json::from_str(&body).unwrap())
    }

    fn activate(&self, token: &str, slug: &str, format: &str) -> (u16, String) {
        self.get(token, &format!("/api/concepts/{slug}?format={format}"))
    }

    fn get(&self, token: &str, path: &str) -> (u16, String) {
        self.get_as(Some(token), path)
    }

    fn get_as(&self, token: Option<&str>, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {}\r\n", self.address).unwrap();
        if let Some(token) = token {
            write!(stream, "Authorization: Bearer {token}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        response(&mut stream, path)
    }
}

fn response(stream: &mut TcpStream, method: &str) -> (u16, String) {
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .unwrap_or_else(|error| panic!("{method} must finish its HTTP response: {error}"));
    let end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("response headers");
    let head = std::str::from_utf8(&response[..end]).unwrap();
    let status = head
        .lines()
        .next()
        .unwrap()
        .split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let len: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(len, response.len() - end - 4);
    (
        status,
        String::from_utf8(response[end + 4..].to_vec()).unwrap(),
    )
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

fn envelope(id: &str) -> Value {
    json!({"protocolVersion": 1, "kbUuid": KB_TEXT, "requestId": id})
}

#[test]
#[ignore = "operator-only: requires a private imported fixture on qualified Linux/Btrfs"]
fn authenticated_write_recovers_lost_response_after_process_restart() {
    let parent =
        PathBuf::from(std::env::var_os("SAGE_DB_HTTP_TEST_PARENT").expect("scratch parent"));
    assert!(parent.is_absolute() && parent.starts_with("/var/tmp"));
    assert!(
        parent
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("sage-http-r1-")
    );
    let root = parent.join("root");
    let term_key = RecordKey::term(1);
    let (version, revision, old_ref, slug) = {
        let store = open(&root);
        let RecordLookup::Found {
            record: FoldedRecord::Term(term),
            ..
        } = store.record(term_key, &mut ledger()).unwrap()
        else {
            panic!("fixture term 1 must exist");
        };
        (
            store.head().unwrap(),
            term.revision.get(),
            term.body_ref,
            term.slug.clone(),
        )
    };
    assert_eq!(version, 6);
    let current_before = fs::read(root.join("CURRENT")).unwrap();
    let author = token();
    let reviewer = token();
    let reader = token();
    let creds = parent.join("credentials.tsv");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&creds)
        .unwrap();
    for (token, principal, capabilities) in [
        (&author, "http-proposer", "propose"),
        (&reviewer, "http-reviewer", "review,read"),
        (&reader, "http-reader", "read"),
    ] {
        writeln!(
            file,
            "{}\t{principal}\t{capabilities}",
            hex(&Sha256::digest(token.as_bytes()))
        )
        .unwrap();
    }
    drop(file);

    let mut process = Process::start(&root, &creds);
    let (status, health) = process.get_as(None, "/api/health");
    assert_eq!(status, 200, "{health}");
    let health: Value = serde_json::from_str(&health).unwrap();
    assert_eq!(health["engine"], "sage-db");
    assert_eq!(
        health["sourceIdentity"],
        std::env::var("SAGE_DB_SOURCE_IDENTITY").unwrap()
    );
    assert_eq!(health["readReady"], true);
    assert_eq!(health["writeReady"], true);
    assert_eq!(process.get_as(None, "/api/knowledge-bases").0, 401);
    let (status, catalog) = process.get(&reader, "/api/knowledge-bases");
    assert_eq!(status, 200, "{catalog}");
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["knowledgeBases"][0]["id"], "default");
    assert_eq!(catalog["knowledgeBases"][0]["isDefault"], true);
    let (status, validation) = process.get(&reader, "/api/validate?graph=true&format=graph");
    assert_eq!(status, 200);
    assert_eq!(validation, "✓ Graph integrity: no issues found.\n");
    let (status, old_revision) = process.get(&reader, &format!("/api/revisions?id={slug}"));
    assert_eq!(status, 200, "{old_revision}");
    let old_revision: Value = serde_json::from_str(&old_revision).unwrap();
    assert_eq!(old_revision["revision"], revision.to_string());
    assert_eq!(old_revision["bodySha256"], hex(&old_ref));
    let hello = json!({"envelope": envelope("hello")});
    let (status, handshake) = process.call("Handshake", &reviewer, &hello);
    assert_eq!(status, 200);
    assert_eq!(handshake["profile"], "legacy-v1");
    assert_eq!(
        handshake["sourceIdentity"],
        std::env::var("SAGE_DB_SOURCE_IDENTITY").expect("expected sealed source identity")
    );
    assert_eq!(handshake["capabilities"], json!(["read", "review"]));
    assert_eq!(handshake["limits"]["maxRefs"], "65");
    assert_eq!(process.call("Handshake", "unregistered", &hello).0, 401);

    let term = json!({"kind": 1, "id": "1"});
    let mut input = envelope("http-submit-v1");
    input["readSet"] = json!([{"key": term, "revision": revision.to_string()}]);
    let submit = json!({
        "envelope": input,
        "payload": {"conceptText": {"subject": term, "atom": NEW_ATOM, "body": NEW_BODY, "declaredAuthor": "literal attribution"}}
    });
    assert_eq!(process.call("SubmitProposal", &reviewer, &submit).0, 403);
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), current_before);
    let find_atom = "/api/search?q=RuntimeProbe+%CE%BF%CF%82";
    let (status, before) = process.get(&reader, find_atom);
    assert_eq!(status, 200, "{before}");
    assert_eq!(
        serde_json::from_str::<Value>(&before).unwrap()["results"],
        json!([])
    );
    let (status, submitted) = process.call("SubmitProposal", &author, &submit);
    assert_eq!(status, 200, "{submitted}");
    let committed = &submitted["outcome"]["committed"];
    assert_eq!(
        committed["recorded"]["commitVersion"],
        (version + 1).to_string()
    );
    let proposal = &committed["submit"]["proposal"];
    assert_eq!(proposal["kind"], 7);
    let proposal_id: u128 = proposal["id"].as_str().unwrap().parse().unwrap();
    let after_submit = fs::read(root.join("CURRENT")).unwrap();

    let mut input = envelope("http-review-v1");
    input["readSet"] = json!([{"key": proposal, "revision": "1"}]);
    let review = json!({
        "envelope": input,
        "payload": {"conceptText": {"proposal": proposal, "decision": "DECISION_APPROVE", "note": "approved by reviewer"}}
    });
    let (status, denied) = process.call("ReviewProposal", &author, &review);
    assert_eq!(status, 403);
    assert_eq!(
        denied["outcome"]["error"]["code"],
        "ERROR_CODE_PERMISSION_DENIED"
    );
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), after_submit);

    // Send and close without reading an acknowledgement. A separate authenticated
    // lookup proves the write reached its durable point before we kill the process.
    let lost = process.send("ReviewProposal", &reviewer, &review);
    lost.shutdown(std::net::Shutdown::Write).unwrap();
    drop(lost);
    let lookup = json!({"envelope": envelope("http-review-v1")});
    let (status, observed) = process.call("GetRequestOutcome", &reviewer, &lookup);
    assert_eq!(status, 200);
    assert_eq!(
        observed["found"]["originalVersion"],
        (version + 2).to_string()
    );
    assert!(observed["found"]["recorded"].is_object());
    let (status, absent) = process.call("GetRequestOutcome", &reader, &lookup);
    assert_eq!(status, 200);
    assert!(absent["absent"].is_object(), "{absent}");
    assert_eq!(process.call("GetRequestOutcome", &author, &lookup).0, 403);
    let committed_current = fs::read(root.join("CURRENT")).unwrap();
    process.stop();

    let mut reopened = open(&root);
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = reopened.record(term_key, &mut ledger()).unwrap()
    else {
        panic!("term must survive process death");
    };
    assert_eq!(term.revision.get(), revision + 1);
    assert_eq!(term.atom, NEW_ATOM);
    assert_ne!(term.body_ref, old_ref);
    assert_eq!(term.common.recorded_by.as_deref(), Some("http-reviewer"));
    let body_ref = term.body_ref;
    assert_eq!(
        reopened
            .read_committed_blob(body_ref, NEW_BODY.len() as u64, &mut ledger())
            .unwrap()
            .as_bytes(),
        NEW_BODY.as_bytes()
    );
    let proposal_key = RecordKey::new(RecordKind::Proposal, proposal_id);
    let RecordLookup::Found {
        record: FoldedRecord::Proposal(row),
        ..
    } = reopened.record(proposal_key, &mut ledger()).unwrap()
    else {
        panic!("proposal must survive process death");
    };
    assert_eq!(row.status, sage_db_types::persist::ProposalStatus::Approved);
    assert_eq!(row.declared_author, "literal attribution");
    assert_eq!(row.common.recorded_by.as_deref(), Some("http-reviewer"));
    assert_eq!(reopened.head(), Some(version + 2));
    drop(reopened);

    let process = Process::start(&root, &creds);
    assert_eq!(process.activate(&author, &slug, "json").0, 403);
    let (status, activation) = process.activate(&reviewer, &slug, "json");
    assert_eq!(status, 200, "{activation}");
    let activation: Value = serde_json::from_str(&activation).unwrap();
    assert_eq!(activation["concept"], slug);
    assert_eq!(activation["molecule"], NEW_BODY.trim());
    let (status, markdown) = process.activate(&reviewer, &slug, "markdown");
    assert_eq!(status, 200);
    assert!(markdown.ends_with(&format!("{NEW_BODY}\n")));
    let (status, error) = process.activate(&reviewer, "missing-from-fixture", "text");
    assert_eq!(status, 404);
    assert_eq!(error, "Concept not found: missing-from-fixture\n");
    eprintln!("HTTP_ACTIVATION_AFTER_REVIEW revision={}", revision + 1);

    let (status, body) = process.get(&reader, find_atom);
    assert_eq!(status, 200, "{body}");
    let search: Value = serde_json::from_str(&body).unwrap();
    let hits = search["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{search}");
    assert_eq!(hits[0]["id"], slug);
    assert_eq!(hits[0]["atom"], NEW_ATOM);
    let (status, manifest) = process.get(&reader, "/api/manifest?format=markdown");
    assert_eq!(status, 200);
    assert!(manifest.contains(&format!("| {slug} | {NEW_ATOM} |")));
    let (status, body_only) = process.get(&reader, "/api/search?q=Approved+via+HTTP");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body_only).unwrap()["results"],
        json!([])
    );
    eprintln!(
        "HTTP_MANIFEST_SEARCH_AFTER_REVIEW revision={}",
        revision + 1
    );

    let (status, body) = process.get(&reader, "/api/graph");
    assert_eq!(status, 200, "{body}");
    let graph: Value = serde_json::from_str(&body).unwrap();
    let graph_nodes = graph["nodes"].as_array().unwrap();
    assert!(
        graph_nodes
            .iter()
            .any(|node| node["id"] == slug && node["atom"] == NEW_ATOM)
    );
    let (status, body) = process.get(&reader, "/api/viz");
    assert_eq!(status, 200, "viz after restart");
    let viz: Value = serde_json::from_str(&body).unwrap();
    let visual_node = viz["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == slug)
        .unwrap();
    assert_eq!(visual_node["atom"], NEW_ATOM);
    assert_eq!(visual_node["molecule"], NEW_BODY.trim());
    let (status, old_after) = process.get(
        &reader,
        &format!("/api/revisions?id={slug}&revision={revision}"),
    );
    assert_eq!(status, 200, "{old_after}");
    let old_after: Value = serde_json::from_str(&old_after).unwrap();
    assert_eq!(old_after["atom"], old_revision["atom"]);
    assert_eq!(old_after["bodySha256"], old_revision["bodySha256"]);
    assert_eq!(old_after["revision"], old_revision["revision"]);
    assert_eq!(old_after["version"], (version + 2).to_string());
    assert!(old_after["sourceVersion"].is_string());
    let (status, new_revision) = process.get(&reader, &format!("/api/revisions?id={slug}"));
    assert_eq!(status, 200, "{new_revision}");
    let new_revision: Value = serde_json::from_str(&new_revision).unwrap();
    assert_eq!(new_revision["revision"], (revision + 1).to_string());
    assert_eq!(new_revision["atom"], NEW_ATOM);
    assert_eq!(
        process
            .get(
                &reader,
                &format!("/api/revisions?id={slug}&revision={}", revision + 2)
            )
            .0,
        404
    );
    eprintln!(
        "HTTP_OPERATOR_READINESS_HISTORY_AFTER_REVIEW revision={}",
        revision + 1
    );
    eprintln!("HTTP_GRAPH_VIZ_AFTER_REVIEW revision={}", revision + 1);

    let (status, found) = process.call("GetRequestOutcome", &reviewer, &lookup);
    assert_eq!(status, 200);
    assert_eq!(found, observed);
    let (status, retry) = process.call("ReviewProposal", &reviewer, &review);
    assert_eq!(status, 200);
    let original = &retry["outcome"]["alreadyCommitted"];
    assert_eq!(original["recorded"], found["found"]["recorded"]);
    assert_eq!(original["originalStatus"], "PROPOSAL_STATUS_APPROVED");

    let mut changed = review.clone();
    changed["payload"]["conceptText"]["note"] = json!("different content");
    let (status, mismatch) = process.call("ReviewProposal", &reviewer, &changed);
    assert_eq!(status, 409);
    assert_eq!(
        mismatch["outcome"]["error"]["code"],
        "ERROR_CODE_IDEMPOTENCY_MISMATCH"
    );
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), committed_current);
    eprintln!(
        "HTTP_RUNTIME_RESTART submit={} review={} same_request_digest=true",
        version + 1,
        version + 2
    );
}
