import { z } from "zod";
import { ConfidenceSchema, SourceSchema } from "./concept.ts";

export const ManifestRowSchema = z.object({
  id: z.string().min(1),
  atom: z.string(),
  layer: z.union([z.literal(0), z.literal(1), z.literal(2), z.literal(3)]),
  domain: z.array(z.string()),
  confidence: ConfidenceSchema,
  source: SourceSchema,
  triggers: z.array(z.string()),
});

export type ManifestRow = z.infer<typeof ManifestRowSchema>;
