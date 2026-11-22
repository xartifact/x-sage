# CLI access to the native command service

The existing REST commands keep their current routes. The `native` group uses
the Rust service deliberately. Both use the existing server and token
configuration and make one HTTP request per invocation.

```bash
x-sage native checkpoint agent 1
x-sage native handshake handshake-request.json
x-sage native submit submit-request.json
x-sage native review review-request.json
x-sage native commit commit-request.json
x-sage native outcome outcome-request.json
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

The old mutation commands keep their legacy REST behavior. The explicit native
group is the supported CLI path for Rust proposal and auxiliary commands.
