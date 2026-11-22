# Frontier and session snapshots in the native workbench

The workbench creates a Frontier record with the existing WriteAux command. It
attaches the current Term revision if the name entered is a known slug. Otherwise
the issue has no Term reference. The issue name, source and reason survive as a
JSON document in the record's text blob; priority and status stay typed.

The latest checkpoint for a concept is available at
`GET /api/knowledge/checkpoints/<slug>`. A positive `session` can select an
explicit session. If more than one record has the requested session number,
the highest native checkpoint key wins. The response returns the exact
committed UTF-8 text alongside its checkpoint identity and revision. This read
requires Read and binds its chosen key and leased content to the same durable
publication. No storage checkpoint is exposed.

Editing an existing session preserves its identity and sends the checkpoint
revision and the concept revision in the command's read set. Creating a new
snapshot explicitly allocates a checkpoint. It does not implicitly replace
another record under that session number. The workbench form accepts the legacy
JSON snapshot document shape. Other well-formed native text is available as
opaque content over HTTP, outside this form.

Write credentials now pass a handshake for Read before a command is attempted.
The same principal must be able to look up its request outcome after a lost
response. Authorization for the command itself remains with the kernel.

## Compatibility for checkpoint readers

`GET /api/checkpoints/<slug>` and its optional `session` parameter read the
same native checkpoint and use the same Read authorization as the native route.
They return the legacy JSON document shape. The admitted parser verifies the
complete document and the identity against the canonical record. Opaque
content outside that shape remains accessible through the native read endpoint.
The legacy route returns a typed refusal for such content.

JSON removes whitespace outside string literals. Plain text preserves the
document's existing formatting and puts one newline after it. The workbench
writes indented checkpoint documents. The five imported checkpoint files match
both legacy responses byte for byte. Their directly stored native content
stays unchanged. Clients still send checkpoint writes through the native
command service or the native workbench.
