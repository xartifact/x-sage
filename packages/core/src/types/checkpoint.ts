import { z } from "zod";

export const CheckpointSchema = z.object({
  concept: z.string().min(1),
  session: z.number().int().positive(),
  date: z.string(),
  established: z.array(z.string()),
  exploredEdges: z.array(z.string()),
  frontier: z.array(z.object({
    id: z.string(),
    reason: z.string(),
    priority: z.enum(["high", "medium", "low"]),
  })),
  nextEntryPoint: z.string().optional(),
});

export type Checkpoint = z.infer<typeof CheckpointSchema>;
