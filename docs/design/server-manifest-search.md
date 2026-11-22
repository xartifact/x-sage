# Native HTTP manifest and search

The isolated native HTTP listener serves `GET /api/manifest` and
`GET /api/search?q=...` from the committed, complete KB publication. Each
read requires the `Read` grant for the opened KB. An optional `kb` selects
its committed slug. Both routes scan active terms and exclude retired
terms. The response is assembled before transmission.

## Compatibility

Manifest defaults to JSON with a single `markdown` field. Text, Markdown
and manifest formats return the Markdown table with the legacy extra
newline. The compatibility `PUT /api/manifest` returns 409 because the
structured store derives the table from committed metadata. It performs
no writes.

Search matches ID, atom and triggers after Unicode lowercase. In descending
priority it ranks exact ID, exact trigger, atom substring, then ID or
trigger substring. Ties use manifest order by layer and UTF-16 source
filename, using the slug when a legacy path is absent. JSON returns the
decoded keyword and matching rows. Text uses the legacy compact output,
and `format=manifest` presents those same ranked rows in the manifest
table. Search uses metadata only. Bodies and authored edges do not
participate.

The decoded keyword ceiling is 1,024 UTF-8 bytes. The result ceiling is
512 rows and the HTTP response ceiling is 128 KiB. Refusals return without
a partial response. Lowercase uses stack buffers and the pinned Unicode
properties for contextual final sigma. The case property tables are
initialized before the listener binds. Substring matching and sorting
charge their comparisons against the request ledger.
