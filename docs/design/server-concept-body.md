# Native concept body read

`GET /api/concepts/<slug>/body` requires the Read capability on the selected
knowledge base. It binds the slug, record and content to the same durable
publication. Unknown or retired slugs return 404. The endpoint never accepts a
request body.

`format=markdown` and `format=text` return the body bytes as stored, using
`text/markdown`. The JSON reply carries `conceptId`, `body`, `atom`,
`structured: true`, and a canonical editable Markdown `document` with authored
relations from the same publication. The JSON `termId`, `revision` and
`version` are decimal strings. These are native baseline values for the
command protocol. The legacy Markdown revision is a content hash.

The Markdown-only reply reads the committed body without building an inference
projection. JSON reuses the existing activation document renderer, with the
same snapshot and output bounds. A later native approval still checks the
reviewed revision as part of its transaction.
