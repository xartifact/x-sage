import { z } from "zod";

export const KnowledgeBaseSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1),
  description: z.string().default(""),
  path: z.string().min(1),
  domains: z.array(z.string()).default([]),
  created: z.string(),
});

export const RegistrySchema = z.object({
  version: z.literal("1"),
  knowledgeBases: z.array(KnowledgeBaseSchema),
});

export type KnowledgeBase = z.infer<typeof KnowledgeBaseSchema>;
export type Registry = z.infer<typeof RegistrySchema>;
