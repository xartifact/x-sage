import { z } from "zod";

export const EdgeTypeSchema = z.enum([
  "is-a",
  "part-of",
  "depends-on",
  "enables",
  "contradicts",
  "instantiates",
  "analogous-to",
]);

export const ConfidenceSchema = z.enum([
  "confirmed",
  "hypothetical",
  "contested",
]);

export const SourceSchema = z.enum([
  "personal",    // 个人观点、立场、方法论
  "domain",      // 来自某个思想领域的客观描述
  "synthesis",   // 对多个领域材料的综合再建构
]);

// target 格式：本 KB 内用 "concept-id"，跨 KB 用 "kb-id::concept-id"
const ConceptRefSchema = z.string().regex(
  /^[a-z0-9-]+(::([a-z0-9-]+))?$/,
  'Must be "concept-id" or "kb-id::concept-id"'
);

export const EdgeSchema = z.object({
  type: EdgeTypeSchema,
  target: ConceptRefSchema,
  confidence: ConfidenceSchema.default("confirmed"),
});

export const ConceptFrontmatterSchema = z.object({
  id: z.string().min(1),
  atom: z.string().min(1).max(200),
  layer: z.union([
    z.literal(0),
    z.literal(1),
    z.literal(2),
    z.literal(3),
  ]),
  domain: z.array(z.string()).min(1),
  confidence: ConfidenceSchema,
  source: SourceSchema,
  triggers: z.array(z.string()).min(1),
  requires: z.array(z.string()).default([]),
  enables: z.array(z.string()).default([]),
  edges: z.array(EdgeSchema).default([]),
});

export type EdgeType = z.infer<typeof EdgeTypeSchema>;
export type Confidence = z.infer<typeof ConfidenceSchema>;
export type Source = z.infer<typeof SourceSchema>;
export type Edge = z.infer<typeof EdgeSchema>;
export type ConceptFrontmatter = z.infer<typeof ConceptFrontmatterSchema>;

export type ConceptFile = {
  frontmatter: ConceptFrontmatter;
  molecule: string;
  filePath: string;
};
