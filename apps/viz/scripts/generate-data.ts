import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { parseConceptFile } from "@x-concepts/core/manifest";

/**
 * Generate a rich visualization data file from a KB directory.
 * Combines graph topology with full concept details.
 */
async function generateVizData(kbPath: string, outputPath: string) {
  const layers = ["L0-axioms", "L1-core", "L2-support", "L3-instances"];
  const nodes: Array<{
    id: string;
    atom: string;
    layer: number;
    domain: string[];
    confidence: string;
    source: string;
    triggers: string[];
    requires: string[];
    enables: string[];
    filePath: string;
    molecule: string;
  }> = [];

  const edges: Array<{
    from: string;
    type: string;
    target: string;
    confidence: string;
  }> = [];

  for (const layer of layers) {
    const layerDir = join(kbPath, "concepts", layer);
    let entries: string[];
    try {
      entries = await readdir(layerDir);
    } catch {
      continue;
    }

    for (const file of entries) {
      if (!file.endsWith(".md")) continue;
      const filePath = join(layerDir, file);
      const raw = await readFile(filePath, "utf-8");
      const concept = parseConceptFile(raw, relative(kbPath, filePath));

      nodes.push({
        id: concept.frontmatter.id,
        atom: concept.frontmatter.atom,
        layer: concept.frontmatter.layer,
        domain: concept.frontmatter.domain,
        confidence: concept.frontmatter.confidence,
        source: concept.frontmatter.source,
        triggers: concept.frontmatter.triggers,
        requires: concept.frontmatter.requires,
        enables: concept.frontmatter.enables,
        filePath: concept.filePath,
        molecule: concept.molecule,
      });

      for (const edge of concept.frontmatter.edges) {
        edges.push({
          from: concept.frontmatter.id,
          type: edge.type,
          target: edge.target,
          confidence: edge.confidence,
        });
      }
    }
  }

  await writeFile(outputPath, JSON.stringify({ nodes, edges }, null, 2), "utf-8");
  console.log(`Generated viz data: ${nodes.length} nodes, ${edges.length} edges → ${outputPath}`);
}

const kbPath = process.argv[2] || "../../kb/default";
const outputPath = process.argv[3] || "public/graph-data.json";
generateVizData(kbPath, outputPath).catch(console.error);
