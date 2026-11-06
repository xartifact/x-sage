import { z } from "zod";
import { EdgeSchema, ConfidenceSchema } from "./concept.ts";

export const GraphNodeSchema = z.object({
  id: z.string().min(1),
  atom: z.string(),
  layer: z.union([z.literal(0), z.literal(1), z.literal(2), z.literal(3)]),
  domain: z.array(z.string()),
  confidence: ConfidenceSchema,
  filePath: z.string(),
});

export const GraphEdgeSchema = EdgeSchema.extend({
  from: z.string().min(1),
  // Present only on edges inferred by transitive closure. Holds the first
  // intermediate node ids of one witness path, so a derived edge is explainable.
  derived: z.array(z.string()).optional(),
});

export const GraphSchema = z.object({
  nodes: z.array(GraphNodeSchema),
  edges: z.array(GraphEdgeSchema),
});

export type GraphNode = z.infer<typeof GraphNodeSchema>;
export type GraphEdge = z.infer<typeof GraphEdgeSchema>;
export type Graph = z.infer<typeof GraphSchema>;
