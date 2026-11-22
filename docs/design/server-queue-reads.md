# Durable proposal and frontier reads

Queue reads require the `Read` grant for the selected KB and bind the complete
durable publication. A response has at most 512 rows and stays inside the
general HTTP rendering limit. Frontier reads authenticate each body against
its committed digest and retain at most 512 KiB of source content per request.

`GET /api/frontier` preserves the four-column output of the imported
`未决概念` table, including the original priorities and row order. It binds
each row to the import audit identifier and preserves the legacy pretty-printed
text format. `GET /api/knowledge/frontier` lists all current native entries
with their identities, revisions, statuses, references and verbatim content.
Native free-form entries and subsequent native revisions use this typed
projection because they need not have the legacy columns.

`GET /api/proposals` follows the structured legacy envelope with a
`proposals` list and empty `edges`, `atoms` and `bodies` arrays.
`GET /api/knowledge/proposals` returns the same list without those arrays.
Both accept an exact status filter. The proposal record's current persisted
status and revision are authoritative. Imported example files remain archive
attachments. Both paths can look up a native `prp_<id>` and return its
metadata. Lookup requires definitive absence before returning 404.
