# Concept activation in the Rust JSON process

The loopback listener serves the existing CLI route `GET /api/concepts/:slug`.
It requires an explicit `read` grant for the opened KB. The optional `kb` selector
must name that KB's committed metadata slug. The route has no request body.

A request resolves the exact active slug, verifies its current body blob and binds the
body and graph snapshot to the same durable publication. Incomplete imports, retired
concepts, authorization refusals and unavailable publications produce distinct errors.
An unknown or retired slug returns a 404.

The endpoint accepts the legacy JSON, AI text and Markdown formats, and the
exploration, validation and generation strategies. Default depth is one without a
strategy and two with a strategy. The request depth ceiling is 64. The projection
accepts up to 512 active terms and 8,192 authored assertions. Resource limits refuse
the complete response if it does not fit.

The canonical fact evaluator supplies the exact prerequisite set using the snapshot's
committed rules. The adapter projects that set in authored-first breadth-first order
for legacy consumers. The graph includes current authored edges with their original
directions and declaration order. A strategy can expand through a derived prerequisite.
Derived rows do not enter `directEdges`. The evaluator's work limits accumulate across
all source queries for one request. Projection and response construction share the
request's byte, entry and deadline ledger.

Markdown projects the current term metadata, current outgoing authored edges and
verified body bytes. It canonicalizes frontmatter formatting. Imported source files
remain archival attachments. After an edit is approved, the next activation reflects
the new committed body through JSON, text and Markdown.

This compatibility slice covers concept activation. Other legacy routes and
production routing remain separate.
