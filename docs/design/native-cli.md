# CLI access to sage-db

The public Bun endpoint uses REST commands and the public write credential.
`status` reads `GET /api/health` and displays the reported authority, readiness,
and version. Graph, concept, proposal, Frontier, and checkpoint commands use
the public REST representations.

Use `kb list` to discover IDs and `--kb <id>` to scope REST requests.
`meta` reads counts, `concept create request.json` submits `frontmatter`
and `body`, and `graph read --semantic` reads the graph with derived edges.
`graph export <path>` saves the graph, and `proposals list --json` prints
the structured proposal list.

The `native` group uses the private Rust endpoint and its role credential.
Select that endpoint explicitly with `--server` and use the normal token
configuration for the selected endpoint. Use `checkpoint read <id> [session]`
to read the public REST representation; the native representation comes from
the private endpoint.

```bash
x-sage native checkpoint agent 1
x-sage native handshake handshake-request.json
x-sage native submit submit-request.json
x-sage native review review-request.json
x-sage native commit commit-request.json
x-sage native outcome outcome-request.json
x-sage native read-term-text request.json
x-sage native read-term-document request.json
x-sage native read-term-activation request.json
x-sage native read-graph-snapshot request.json
x-sage native read-proposal-files request.json
x-sage native revision agent --revision 1
x-sage native frontier
x-sage native proposals --status pending
x-sage native proposals prp_id --preview
```

The RPC commands accept a caller-owned ProtoJSON request file, or `-` for
stdin. The client sends those bytes unchanged so the server can enforce the
protocol's field shape, range, and duplicate checks. The request envelope
carries the KB UUID and caller-generated request ID. Submit, review and commit
read sets carry the exact revisions the caller observed. Prepare and keep the
request file before sending a mutation.

A native write exits 0 after it receives a recorded commit and exits 1 on a
definitive refusal. Exit 2 signals an unconfirmed write or incomplete outcome
lookup. Use `native outcome` with the same authenticated principal, KB UUID
and original request ID. If the server confirms durable absence, retry with the
original request file. Successful replies and explicit incomplete replies go
to stdout. HTTP errors go to stderr.

Use `propose-edge`, `refine-atom`, `refine-body` and `proposals merge` for
public proposal workflows. The private RPCs provide native proposal and
auxiliary command envelopes to authorized operators.

The internal document adapters use binary frames. Pass an existing encoded
request file or `-` to `native submit-large-body`, `native commit-direct-concept`,
and `native checkpoint-document read|write`. Use
`native frontier-document read` to read the raw frame and
`native frontier-document write <request.bin|->` to submit a replacement frame.
The CLI writes binary read responses to stdout. Document writes require the
private server's HTTP 204 acknowledgement.

## Offline administration

Run from an x-sage source checkout or pass `--workspace <checkout>` after the
operation name. The archive and plan commands use the existing Bun tools,
including `archive --verify`. The other commands use the checkout's pinned
Rust toolchain and the qualified OS filesystem backend.

The plan extractor preserves its exit codes. Exit 66 can accompany a written
plan whose extraction succeeded and whose target mapping is still pending.
The native import command performs that mapping and validates the result before
creating the target.

```bash
x-sage admin archive --help
x-sage admin plan --help
x-sage admin import /absolute/new-root "$KB_UUID" plan.json archive "$PLAN_SHA" "$SOURCE_SHA" "$SESSION"
x-sage admin backup /absolute/root "$KB_UUID" /absolute/artifact
x-sage admin restore /absolute/artifact "$KB_UUID" /absolute/restored-root
```

Import takes a verified archive and explicit, independently recorded plan
and source digests. It creates a fresh root and publishes the mapping in a
single import transaction under the canonical transaction ceilings. It
reopens the database for full verification before recording completion, then
reopens it again to confirm readiness. Preserve an incomplete target for
diagnosis if an operation reports failure or an unknown outcome.

Stop the process holding the source store before backup. An optional final
`VERSION` selects an ancestor, while omission selects the current version.
Restore creates a fresh absolute root after validating the artifact and
expected KB identity.
