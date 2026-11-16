import { z } from "zod";
import { ConceptIdSchema, EdgeTypeSchema, ConfidenceSchema } from "../types/index.ts";
import {
  ClaimIdSchema,
  ClaimKindSchema,
  EvidenceAssetSchema,
  EvidenceKindSchema,
  EvidenceTargetSchema,
  ProposalKindSchema,
  PrincipalSchema,
  SHA256Schema,
  VectorRecordSchema,
  VectorSubjectTypeSchema,
} from "./types.ts";

/**
 * Request contracts for the structured knowledge API.
 *
 * Validation lives in core so the HTTP dispatcher and any future client share
 * one definition of what a valid write is. Limits are hard caps, not advisory:
 * an unbounded evidence excerpt or vector is a memory exhaustion path.
 */

/**
 * Hard input ceilings for the structured surface.
 *
 * `MAX_EVIDENCE_BYTES` is the one the HTTP body cap is derived from, so a
 * document that fits the documented limit always fits the transport.
 */
export const MAX_VECTOR_VALUES = 4096;
export const MAX_EVIDENCE_BYTES = 8 * 1024 * 1024;

export const ImportKnowledgeBaseSchema = z.object({
  /** Explicit acknowledgement that Markdown files become a read-only archive. */
  mode: z.literal("authority"),
  /** Re-import an already imported KB; refuses unless the caller says so. */
  reimport: z.boolean().optional(),
});

export const CreateClaimSchema = z.object({
  conceptId: ConceptIdSchema,
  kind: ClaimKindSchema,
  text: z.string().trim().min(1).max(4000),
  confidence: ConfidenceSchema.default("confirmed"),
});

export const EditClaimSchema = z.object({
  expectedRevision: SHA256Schema,
  text: z.string().trim().min(1).max(4000).optional(),
  kind: ClaimKindSchema.optional(),
  confidence: ConfidenceSchema.optional(),
});

export const AttachEvidenceSchema = z
  .object({
    kind: EvidenceKindSchema,
    title: z.string().trim().min(1).max(300),
    locator: z.string().trim().max(2000).optional(),
    excerpt: z.string().max(20_000).optional(),
    capturedAt: z.string().max(40).optional(),
    capturedBy: z.string().trim().min(1).max(200),
    targets: z.array(EvidenceTargetSchema).min(1).max(50),
    asset: EvidenceAssetSchema.optional(),
  })
  .superRefine((value, ctx) => {
    if (value.kind === "text" && (value.excerpt ?? "").trim() === "") {
      ctx.addIssue({ code: "custom", path: ["excerpt"], message: "Text evidence needs an excerpt" });
    }
    if (value.kind === "image" && value.asset === undefined) {
      ctx.addIssue({ code: "custom", path: ["asset"], message: "Image evidence needs an asset" });
    }
  });

export const AssertRelationSchema = z.object({
  from: ConceptIdSchema,
  type: EdgeTypeSchema,
  target: z.string().regex(/^[a-z0-9-]+(::([a-z0-9-]+))?$/, "Must be a concept id or kb-id::concept-id"),
  confidence: ConfidenceSchema.default("confirmed"),
});

export const UploadVectorSchema = z.object({
  subjectType: VectorSubjectTypeSchema,
  subjectId: z.string().min(1).max(200),
  model: z.string().min(1).max(120).regex(/^[A-Za-z0-9._:/-]+$/, "Model ids are provider/model tokens"),
  provider: z.string().max(120).optional(),
  /**
   * The authority revision the embedding was computed from.
   *
   * Required, not derived: the agent that produced the vector read a specific
   * version of the text, and only it knows which one.
   */
  revision: SHA256Schema,
  values: z
    .array(
      z
        .number()
        .finite()
        .refine((value) => Math.abs(value) <= 3.4028234663852886e38, "Magnitude exceeds float32"),
    )
    .min(1)
    .max(MAX_VECTOR_VALUES),
});

export const UploadVectorsSchema = z.object({
  vectors: z.array(UploadVectorSchema).min(1).max(256),
});

export const ProposalPayloadSchema = z.object({
  kind: ProposalKindSchema,
  subject: z.string().min(1).max(200),
  expectedRevision: SHA256Schema,
  reason: z.string().trim().min(1).max(2000),
  author: PrincipalSchema,
  payload: z.record(z.unknown()),
});

export const ReviewSchema = z.object({
  decision: z.enum(["approve", "reject"]),
  reviewer: PrincipalSchema,
  note: z.string().trim().max(2000).optional(),
});

export const ProjectionWritebackSchema = z.object({
  document: z.string().min(1).max(512 * 1024),
  expectedRevision: SHA256Schema,
  reason: z.string().trim().min(1).max(2000),
  author: PrincipalSchema,
});

export { VectorRecordSchema, ClaimIdSchema };
