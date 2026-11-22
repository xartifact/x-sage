# Native concept and relation proposals in the workbench

The workbench selects the native command flow only after the health endpoint
reports `engine: sage-db`. Markdown deployments keep their existing forms and
requests. The two bearer credentials remain in memory. Reads use the graph
session's Read credential and commands use the separate workbench credential.

Concept editors collect the native Term key and revision with the current body
and atom. Submitting replaces only the selected text field. Relation submission
reads both Term baselines and sends them in numeric key order while preserving
the authored direction. The pending queue presents approved, rejected and
conflicted as separate persisted results.

Approval and rejection require the preview of the requested proposal and a
reviewer-authored note. The
preview schema describes the existing hash-verified command intent and carries
the proposal's revision. It shares the canonical stored-intent validator with
approval. The approval command rechecks the stored proposal and term
baselines. Imported legacy proposals retain their metadata and are not
decoded as native intent.

A command with an ambiguous response keeps its original request identity and
payload in memory. Repeating the action first looks up the outcome. A durable
absence allows another attempt using the original request identity; incomplete
coverage stops the retry. Closing the dialog is blocked while the result is
unconfirmed. A full browser reload clears this in-memory state.

This slice wires text and relation proposals. See
[native auxiliary work](native-workbench-auxiliary.md) for Frontier entries and
session checkpoints. New concepts use the TermCreate proposal and review
flow described in [kernel operations](kernel-operations.md).
