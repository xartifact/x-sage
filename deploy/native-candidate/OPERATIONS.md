# Bun and native x-sage-db release candidate

Bun owns public REST, authentication and the static application. One host
Rust process owns the qualified Btrfs database, with its private transport
bound to `127.0.0.1:3901`. The Bun candidate has host networking so that its
private client reaches that loopback. Its public HTTP listener also binds
loopback, on port 3301 in rehearsals or the reverse proxy's approved local
upstream port for activation. Confirm the actual host proxy before routing
traffic. The example includes no externally published database port.

The standalone Compose file consumes the immutable Bun image digest. That
image must contain the candidate startup code built through the existing
Dockerfile. Build the Rust artifact natively for the exact qualified host
using the pinned workspace toolchain and the approved source manifest
digest as build-time `SAGE_DB_SOURCE_IDENTITY`. Install the host template
as an operator user unit under `~/.config/systemd/user`. Its environment file
binds the exact Btrfs authority through `SAGE_DB_ROOT` and `SAGE_DB_KB_UUID`.
The user manager must be active with linger enabled. Keep configuration and
tokens owner-private. Set the binary manifest to the actual tested binary
at its absolute installed path and keep the prior binary and image identities.
This path uses the existing operator account and does not require a new
system account or a write under /etc or /srv.

Bun receives only its read-only credential directory and dedicated writable
projection directory. The old image's baked KB mount is masked. The Bun
runtime never receives a mount of the Btrfs database or the retired writable
Markdown/SQLite root. Rust alone opens the selected database. The Rust
credential table contains token hashes and exactly the capability for each
role. Runtime logs and deployment receipts should contain artifact digests
and capability names, with credentials kept in the mounted files.

## Startup binding

| Role | Binding |
|---|---|
| Bun public writes | `X_SAGE_WRITE_TOKEN_FILE`, established legacy middleware |
| Graph, term body/document, activation and catalog | `X_SAGE_DB_READ_TOKEN_FILE`, scope `read` |
| Concept creation | `X_SAGE_DB_CONCEPT_TOKEN_FILE`, scope `write_concept` |
| Frontier and checkpoint | Read file plus `X_SAGE_DB_AUX_TOKEN_FILE`, scope `write_aux` |
| Derived graph and manifest publication | `X_SAGE_DB_PROJECTION_DIR` |
| Proposal list and details | Same database Read credential as term reads |
| Proposal submission | `X_SAGE_DB_PROPOSE_TOKEN_FILE` mounted role file, scope `propose` |
| Proposal merge | `X_SAGE_DB_REVIEW_TOKEN_FILE` mounted role file, scope `review` |

The startup source identity must be the one bound into the selected Rust
binary. Every configured role performs an authenticated Handshake on the
selected KB UUID and checks its granted scope. The catalog validates the KB
slug and provides the display name. A healthy database means both ReadReady
and WriteReady. This rejects a fenced root and a read-only service even
when its process is alive.

Use `X_SAGE_DB_MODE=rehearsal` only on an isolated copy. It binds loopback,
checks the database, and reports the tested native readiness with public
health status 503. A partial adapter cannot qualify for release. Native
`active` requires the complete route bindings in code, exact Bun
`commitHash` and `buildRef`, the expected Rust source identity and the ready
database. Missing proposal operations or unavailable credentials prevent
active startup. The gating code checks concrete adapter functions;
there is no environment acknowledgement that can substitute for them.

## Release and rollback boundaries

1. Prepare and seal the production legacy authority capture after ingress is
   fenced and its previous writer has stopped. Prepare an independent backup
   and verify a restoration in isolation.
2. Import the sealed capture into a new qualified native root. Verify
   an actual complete import, the intended source identity, source-to-record
   comparisons and ready restart. Capture the pre-activation baseline below
   while the native service is stopped. Preserve the old image, credentials,
   exact upstream configuration and old authority read-only.
3. Run the Bun and Rust release artifacts against the isolated clone and run
   the public parity and operation matrix. Check both artifact identities in
   the actual running processes. When the complete code gate exists, start
   the selected native authority and verify Bun's in-container `/api/health`
   is green with its expected build, ref and Rust source identity before
   exchanging the public upstream. Keep the old writer stopped. On x99
   the existing HTTPS reverse proxy already targets localhost port 3001.
   The native Bun container can take that same loopback port after the
   old container has stopped. Disable and drain old automated deployment
   jobs before giving the candidate that port. Check the ongoing Compose
   restart policy and the user unit's failure restart from the staged
   release. The old wildcard direct HTTP listener is retired at this step.
4. For rollback, first fence public ingress, settle any unknown native
   outcome, and stop Bun and Rust. Keep both stopped while determining which
   authority may resume. Compare the new native CURRENT and complete old
   authority tree with the sealed pre-activation receipt.
5. Re-enable the preserved legacy service only when the pre-activation
   comparison matches and the previous routes, image and credentials are
   verified. If the native CURRENT advanced, keep the native authority and
   repair forward or perform an independently verified reverse migration
   before switching. A service downgrade must never discard committed
   native data. Do not point an older Rust binary at a chain that contains
   fields it cannot decode.

Capture and compare with the helper using **stopped writers** and paths on
the selected host. Store the receipt separately from both authority roots.

```bash
python3 deploy/native-candidate/rollback-guard.py capture "$native_root" "$legacy_root" > "$receipt"
python3 deploy/native-candidate/rollback-guard.py check "$native_root" "$legacy_root" --receipt "$receipt"
```

The helper obtains the native store's existing exclusive LOCK, checks a
bounded CURRENT, and compares a deterministic digest of regular legacy files,
directories and their names/modes. It refuses a held lock, an altered
CURRENT, missing or new legacy data and unsupported file types. It releases
the lock upon completion. The operator must keep the services stopped.
This comparison does not fence legacy processes, validate a backup or
resolve outcomes. A passing comparison is an input to the rollback decision
and performs no routing change.
