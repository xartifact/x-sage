# Loopback JSON command process

`sage-db-server` opens one existing KB through the qualified OS backend and keeps its Store lock
for the process lifetime. It accepts HTTP/1.1 on a loopback address. The process handles one
connection and one request at a time, then closes the connection. The supported POST methods
under `/sage.v1.SageCommandService/` are `Handshake`, `SubmitProposal`, `ReviewProposal` and
`GetRequestOutcome`. This listener admits the existing ConceptText proposal family.

## Run configuration

Build the binary with `SAGE_DB_SOURCE_IDENTITY` set to the sealed source manifest's SHA-256.
The binary reports that compiled identity and the Store's actual committed rule profile.
The runner is responsible for verifying the manifest before building.

Set `SAGE_DB_ROOT` to the absolute directory of an existing KB and `SAGE_DB_KB_UUID` to its
32-digit lowercase hex identity. Set `SAGE_DB_LISTEN` to an explicit loopback socket address.
`SAGE_DB_CREDENTIALS` names a deployment-owned UTF-8 TSV file. Each row contains the lowercase
SHA-256 of an opaque bearer token, a principal and a comma-separated set of explicit capabilities.
This slice accepts `read`, `propose` and `review`. The file holds hashes only. Standard output
reports the bound address. Error output excludes request bodies and credential material.

## Request and recovery contract

The framing parser accepts a single numeric Content-Length and `application/json`. The request
head and body are bounded at 8 KiB and 256 KiB. Ambiguous framing is refused. An absolute
five-second request deadline bounds socket I/O and pre-persist work. Early refusal responses close the
write side and briefly discard unread input under a fixed byte and time ceiling. A positive envelope deadline
can shorten it. The response uses a bounded 128 KiB serialization buffer.

The listener authenticates the supplied bearer against the actual KB before accepting the
request body beyond any bytes already buffered with the headers, and before JSON decoding. The command adapter verifies the same credential again before accessing Store
records. Descriptor-generated ingress seeds, wire validation and the existing command adapter
share the attempt ledger. The server creates transaction IDs from OS entropy and uses its own
clock for commit metadata. Review and submission remain distinct capabilities.

Outcome lookup requires `read` and uses the authenticated principal and request ID. It returns the
durable result, basis-bound absence or incomplete coverage. A lost response should be resolved
using the original request ID. A retry with identical content returns the original recorded
commit. A changed payload with the same principal and request ID is refused. Response assembly
or transfer failure after a commit closes the connection so the client can recover its outcome.

The Linux operator test uses a private copy of an imported KB. It verifies credential isolation,
separate approval permissions, committed term and proposal after-images after process death, and
an exact retry plus a conflicting replay following restart.

This is an operator rehearsal entry point. HTTP/2, the legacy REST adapter, query endpoints,
multi-KB scheduling, graceful draining and production routing require separate integration.
