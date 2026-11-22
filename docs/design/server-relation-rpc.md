# Relation proposals over native HTTP

`SubmitProposal` and `ReviewProposal` accept the `relationAssert` family.
The submission names two Term keys, their two current revisions in canonical
order, the relation and confidence discriminants, declaration order, literal
attribution, and reason. Relation and confidence must appear explicitly.
Zero is valid for is-a and contested. The caller can obtain a Term key and
revision with `GET /api/revisions?id=<slug>`.

A review names the Proposal in the payload and pins its revision in the
envelope. The transport rejects disagreement between those keys. It gives
the kernel a validation budget and the HTTP request's actual deadline.
A candidate that violates committed rules returns 422 and stays pending.
The kernel remains responsible for capability checks, validity, atomic
publication and idempotency. A retry uses the original request ID and body.

The listener also dispatches the registered claim-edit family to its
existing command adapter. Claim edit requires the subject and claim
baselines. The concept text and auxiliary families use the same HTTP
dispatch and canonical response assembly.
