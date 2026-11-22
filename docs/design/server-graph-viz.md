# Native HTTP graph and visualisation

`GET /api/graph` and `GET /api/viz` require the `Read` capability on the
opened KB and serve one complete durable publication. An optional `kb` names
the committed slug. Active terms are listed in legacy layer/filename order.
Active authored edges keep their direction, confidence, and declaration order.

`/api/graph?semantic=true` appends missing symmetric directions once, using
their strongest active support. An authored row keeps its own annotation when
both directions were authored. The source evaluator supplies transitive and
formal-rule dependency facts with native confidence. The projection preserves
legacy breadth-first order and the first hop when that route supports the native
confidence. When it does not, the first hop comes from the native proof.
Formal-rule dependencies with no same-relation authored path follow the native
ordering. A direct definition-implied dependency has an empty `derived` array.

`GET /api/viz` returns authored edges and immediately follows each symmetric
declaration with a visual mirror. Its JSON includes verified committed body
content, source, triggers, and counts. Body owner identity and the full basis
are bound before rendering. `format=text` returns the legacy statistics without
reading bodies. The `generatedAt` field is UTC wall time at millisecond
precision. The response and aggregate raw bodies each have a 512 KiB ceiling
because the real 41-concept fixture already exceeds the general 128 KiB
response ceiling. The output buffer is admitted before allocation and the full
response is rendered before transmission. Graph output uses the general ceiling
and semantic graph output also has a 5,000-edge ceiling.

`PUT /api/graph` accepts the legacy request framing and returns a typed 409
for the structured authority. Its body does not select any action. The legacy
CLI's `viz stats`, `viz export`, and `graph generate --manifest` use these
routes. Native validation and revision history are handled separately.
