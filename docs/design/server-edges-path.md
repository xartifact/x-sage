# Edge and path compatibility reads

The loopback HTTP process serves `GET /api/concepts/:slug/edges` and `GET /api/path`.
Each requires an explicit read grant for the opened KB. An optional `kb` selector must name
that KB's committed slug. The CLI's JSON and text output shapes are served directly.

The edge endpoint accepts `dir=in|out|both`, a registered relation name in `type`, and
`derived=true`. Directed assertions retain their direction and declared confidence, with
parallel supports displayed separately. Symmetric relations are grouped as `both`, with
the strongest support as required by the native profile. When requested, derived-only
directed facts come from the canonical evaluator, including its strongest-path confidence.
Direct supports remain authored rows. The text renderer labels inferred-only rows.

The path endpoint accepts `from` and `to` slugs. The native shortest-path query decides
reachability and distance using authored assertions in either traversal direction.
The compatibility view selects among equal-length paths by legacy declaration order, and
checks that it has the canonical length. Identical known endpoints produce a one-node path.
Disconnected known endpoints return a complete empty result. Unknown and retired endpoints
return 404, including when both slugs are the same.

Both responses are built over one authorized durable snapshot. The same request ledger
bounds projection, ordering and rendering. Calls to the fact evaluator share a cumulative
evaluation allowance. Work exhaustion, stop and incomplete import refuse the response.
These routes do not mutate the Store or read original Markdown files.
