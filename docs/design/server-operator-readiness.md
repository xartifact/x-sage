# Native deployment probes and operator reads

`GET /api/health` is a public liveness and readiness probe for an already
started listener. It reports the exact build source manifest, the engine,
and separate read and write readiness. It returns 503 when there is no
complete readable publication. The write flag reflects the Store fence.
Individual writes still require their own capability and admission.

Authenticated callers with `Read` can list the listener's one KB with
`/api/knowledge-bases` and read the active term and authored-edge counts
with `/api/meta`. The graph browser requests a read token when the list
requires authentication and keeps it in memory for the browser session.

`/api/validate` projects active canonical records in the legacy report
shape. `graph=true` runs the canonical validator under the committed rule
profile and retains the legacy disconnected-concept diagnostic.
`format=graph` prints only that graph verdict. The structured report
describes committed records and graph integrity.

`/api/revisions?id=<term-slug>` reports the native Term identity and
current revision. Adding `revision=<positive decimal>` verifies the exact
historical event against the durable publication and returns its source
version. A complete search may return 404. A stopped, incomplete, or
corrupt search retains its failure. The legacy SQLite proposal baseline
continues to belong to the legacy API.

The authenticated `CommitCommand` RPC accepts the generated checkpoint
and frontier command families. Every command requires a `write_aux` grant.
Checkpoint creation pins the concept. Revision pins the concept and the
checkpoint. Frontier commands pin the named concept references and, on
revision, the frontier entry. The maximum read set is 65 pairs. The existing kernel remains the authority for
request identity, write fencing, revision checks, and persisted outcomes.
