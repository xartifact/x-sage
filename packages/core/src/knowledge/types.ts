import { z } from "zod";
import { ConceptIdSchema, ConfidenceSchema } from "../types/index.ts";

/**
 * Structured knowledge contracts.
 *
 * The authoritative record is not Markdown: a concept is a row, its claims are
 * rows, and evidence is a row with a content-addressed asset. Markdown is a
 * projection for human editing and review, never the source of truth once a
 * knowledge store exists.
 *
 * Every identifier below is generated server-side (`clm_…`, `evd_…`, `prp_…`,
 * `vec_…`) and validated on the way in, so an id that arrives from a request
 * body can never be joined onto a filesystem path or a SQL fragment.
 */

const ID_SUFFIX = "[a-z0-9]{6,40}";

/**
 * Who submitted or reviewed something.
 *
 * A declared identity, not authentication: the write token gates the *channel*,
 * and this field records which agent or human used it. Kept to a token charset
 * so it is safe to print in a text report and to use as a filename fragment.
 */
export const PrincipalSchema = z
  .string()
  .trim()
  .min(1)
  .max(120)
  .regex(/^[A-Za-z0-9][A-Za-z0-9._:-]*$/, "Use letters, digits and . _ : - ");
export const ClaimIdSchema = z.string().regex(new RegExp(`^clm_${ID_SUFFIX}$`), "Must look like clm_<hex>");
export const EvidenceIdSchema = z.string().regex(new RegExp(`^evd_${ID_SUFFIX}$`), "Must look like evd_<hex>");
export const ProposalIdSchema = z.string().regex(new RegExp(`^prp_${ID_SUFFIX}$`), "Must look like prp_<hex>");
export const VectorIdSchema = z.string().regex(new RegExp(`^vec_${ID_SUFFIX}$`), "Must look like vec_<hex>");

export const SHA256Schema = z.string().regex(/^[a-f0-9]{64}$/, "Must be a lowercase hex sha256");

/**
 * What a claim is *doing* in an argument.
 *
 * Deliberately small: the kind selects how a reviewer reads the sentence, it is
 * not a taxonomy to file knowledge under. A claim that needs two kinds is two
 * claims.
 */
export const ClaimKindSchema = z.enum([
  "premise",
  "constraint",
  "decision",
  "risk",
  "observation",
  "requirement",
]);

/**
 * Review lifecycle of a claim.
 *
 * Only `accepted` claims are authoritative context. `proposed` claims live in
 * the proposal table until a review approves them, so this field mainly
 * distinguishes accepted from retired claims in historical reads.
 */
export const ClaimStatusSchema = z.enum(["accepted", "superseded", "rejected"]);

export const ClaimSchema = z.object({
  id: ClaimIdSchema,
  conceptId: ConceptIdSchema,
  kind: ClaimKindSchema,
  status: ClaimStatusSchema,
  text: z.string().min(1).max(4000),
  confidence: ConfidenceSchema,
  /** Monotonic per-claim edit counter; a review conflict uses the hash, not this. */
  revision: z.number().int().min(1),
  createdAt: z.string(),
  updatedAt: z.string(),
});

export const EvidenceKindSchema = z.enum(["text", "image"]);

/** Supported image types. Anything else is rejected rather than stored opaque. */
export const IMAGE_MEDIA_TYPES = ["image/png", "image/jpeg", "image/webp", "image/gif"] as const;
export const ImageMediaTypeSchema = z.enum(IMAGE_MEDIA_TYPES);

/**
 * A relation target identity, `from|type|target`.
 *
 * Evidence about an asserted relation is what makes a contradiction traceable
 * to the documents that produced it, so relations are first-class evidence
 * targets alongside concepts and claims.
 */
export const RelationRefSchema = z
  .string()
  .regex(/^[a-z0-9-]+(::[a-z0-9-]+)?\|[a-z-]+\|[a-z0-9-]+(::[a-z0-9-]+)?$/);

export const EvidenceTargetSchema = z.object({
  type: z.enum(["concept", "claim", "relation"]),
  id: z.string().min(1).max(200),
});

export const EvidenceAssetSchema = z.object({
  sha256: SHA256Schema,
  mediaType: ImageMediaTypeSchema,
  byteSize: z.number().int().min(1).max(8 * 1024 * 1024),
});

export const EvidenceSchema = z
  .object({
    id: EvidenceIdSchema,
    kind: EvidenceKindSchema,
    title: z.string().min(1).max(300),
    /** Where the material came from: a URL, a repository path, a document id. */
    locator: z.string().max(2000).optional(),
    /** Text evidence body, or a caption/transcription note for an image. */
    excerpt: z.string().max(20_000).optional(),
    asset: EvidenceAssetSchema.optional(),
    capturedAt: z.string().max(40),
    capturedBy: z.string().max(200),
    createdAt: z.string(),
    targets: z.array(EvidenceTargetSchema).min(1).max(50),
  })
  .superRefine((value, ctx) => {
    if (value.kind === "text" && (value.excerpt ?? "").trim() === "") {
      ctx.addIssue({ code: "custom", path: ["excerpt"], message: "Text evidence needs an excerpt" });
    }
    if (value.kind === "image" && value.asset === undefined) {
      ctx.addIssue({ code: "custom", path: ["asset"], message: "Image evidence needs an asset" });
    }
  });

export type ClaimKind = z.infer<typeof ClaimKindSchema>;
export type ClaimStatus = z.infer<typeof ClaimStatusSchema>;
export type Claim = z.infer<typeof ClaimSchema>;
export type EvidenceKind = z.infer<typeof EvidenceKindSchema>;
export type EvidenceTarget = z.infer<typeof EvidenceTargetSchema>;
export type EvidenceAsset = z.infer<typeof EvidenceAssetSchema>;
export type Evidence = z.infer<typeof EvidenceSchema>;

/** What a vector is *about*. Claims and evidence are embedded as well as concepts. */
export const VectorSubjectTypeSchema = z.enum(["concept", "claim", "evidence"]);

/**
 * An externally supplied embedding.
 *
 * `revision` is the authority revision the embedding was computed from. It is
 * the whole point of the record: an embedding of text that has since changed is
 * a claim about a document that no longer exists, and retrieval must exclude it
 * rather than rank it.
 *
 * `model` and `dimensions` travel with every row because comparing vectors from
 * different models is meaningless; a query names its model and only rows with
 * the same model and dimension participate.
 */
export const VectorRecordSchema = z.object({
  id: VectorIdSchema,
  subjectType: VectorSubjectTypeSchema,
  subjectId: z.string().min(1).max(200),
  model: z.string().min(1).max(120).regex(/^[A-Za-z0-9._:/-]+$/, "Model ids are provider/model tokens"),
  dimensions: z.number().int().min(1).max(4096),
  revision: SHA256Schema,
  provider: z.string().max(120).optional(),
  /**
   * Float32 range is enforced, not just finiteness: a finite double like 1e39
   * narrows to +Infinity in the stored buffer and makes every later cosine()
   * over that model throw.
   */
  values: z
    .array(
      z
        .number()
        .finite()
        .refine((value) => Math.abs(value) <= 3.4028234663852886e38, "Magnitude exceeds float32"),
    )
    .min(1)
    .max(4096),
  createdAt: z.string(),
});

export type VectorSubjectType = z.infer<typeof VectorSubjectTypeSchema>;
export type VectorRecord = z.infer<typeof VectorRecordSchema>;

/** A vector without its raw values, for listings and retrieval explanations. */
export type VectorSummary = Omit<VectorRecord, "values"> & { norm: number };

export const ReviewDecisionSchema = z.enum(["approve", "reject"]);

/**
 * Candidate knowledge submitted by an external agent.
 *
 * Every kind is review-gated: nothing here reaches the authority tables until a
 * review approves it. The payload shapes are discriminated by `kind` in the
 * store layer; keeping the kind list here documents the whole surface.
 */
export const ProposalKindSchema = z.enum([
  "concept-markdown",
  "claim-create",
  "claim-edit",
  "evidence-attach",
  "relation-assert",
]);

export const ProposalStatusSchema = z.enum(["pending", "approved", "rejected", "conflicted"]);

export const KnowledgeProposalSchema = z.object({
  id: ProposalIdSchema,
  kind: ProposalKindSchema,
  /** Concept id (or claim id for `claim-edit`) the proposal is written against. */
  subject: z.string().min(1).max(200),
  /**
   * Authority revision quoted by the author. A review re-checks it against the
   * live record; a mismatch is a conflict, never a silent rebase.
   */
  expectedRevision: SHA256Schema,
  payload: z.record(z.unknown()),
  reason: z.string().min(1).max(2000),
  author: z.string().min(1).max(200),
  status: ProposalStatusSchema,
  createdAt: z.string(),
  reviewedAt: z.string().nullable(),
  reviewer: z.string().nullable(),
  reviewNote: z.string().nullable(),
});

export type ProposalKind = z.infer<typeof ProposalKindSchema>;
export type ProposalStatus = z.infer<typeof ProposalStatusSchema>;
export type KnowledgeProposal = z.infer<typeof KnowledgeProposalSchema>;

/** Bounded search request shared by lexical and hybrid retrieval. */
export const MAX_QUERY_CHARS = 512;
export const MAX_SEARCH_LIMIT = 50;
export const DEFAULT_SEARCH_LIMIT = 10;

export type SearchSignal =
  | { axis: "lexical"; score: number; terms: Array<{ term: string; tf: number }> }
  | { axis: "vector"; score: number; model: string; dimensions: number; cosine: number };

export interface AssertedRelation {
  /** Relations are what an author asserted; vectors are only associations. */
  direction: "in" | "out";
  type: string;
  target: string;
  confidence: string;
}

export interface SearchEvidenceRef {
  id: string;
  kind: EvidenceKind;
  title: string;
}

export interface SearchHit {
  /** Which table the hit came from. */
  kind: "concept" | "claim" | "evidence";
  id: string;
  conceptId: string | null;
  label: string;
  score: number;
  signals: SearchSignal[];
  assertedRelations: AssertedRelation[];
  /**
   * Vector associations are kept in their own field so no consumer can mistake
   * them for edges. `cosine` is null when the value was not computed for this
   * request — a measured 0 and "not measured" must not look alike.
   */
  vectorAssociations: Array<{
    subjectType: VectorSubjectType;
    subjectId: string;
    model: string;
    cosine: number | null;
  }>;
  claims: string[];
  evidence: SearchEvidenceRef[];
}

export interface SearchResponse {
  query: string;
  mode: "lexical" | "hybrid";
  vector: {
    model: string;
    dimensions: number;
    /** Rows skipped because the subject changed since the embedding was made. */
    excludedStale: number;
    /** Rows skipped because no embedding existed for that subject. */
    missing: number;
    /** Rows skipped because they hold non-finite values (pre-guard uploads). */
    invalid: number;
  } | null;
  results: SearchHit[];
  limit: number;
  notes: string[];
}
