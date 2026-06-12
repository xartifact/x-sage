#!/usr/bin/env bun
import { generateManifest, formatManifest } from "@x-concepts/core/manifest";
import { readCheckpoint, writeCheckpoint } from "@x-concepts/core/checkpoint";
import { readFrontier, addFrontierEntry } from "@x-concepts/core/frontier";
import { resolve } from "node:path";

const KB_ROOT = resolve(import.meta.dir, "../../../kb/default");

const command = process.argv[2];

async function main() {
  switch (command) {
    case "manifest": {
      const rows = await generateManifest(KB_ROOT);
      const md = formatManifest(rows);
      console.log(md);
      break;
    }
    case "activate": {
      const conceptId = process.argv[3];
      if (!conceptId) {
        console.error("Usage: x-concepts activate <concept-id>");
        process.exit(1);
      }
      // TODO: implement activate — load concept file + 1-hop neighbors
      console.log(`Activating: ${conceptId}`);
      break;
    }
    case "frontier": {
      const entries = await readFrontier(KB_ROOT);
      console.log(JSON.stringify(entries, null, 2));
      break;
    }
    case "validate": {
      // TODO: implement validate — check all concept files against schema
      console.log("Validation not yet implemented");
      break;
    }
    default:
      console.log(`x-concepts CLI

Usage: x-concepts <command> [args]

Commands:
  manifest    Generate MANIFEST.md from concept files
  activate    Activate a concept and its neighbors
  frontier    Read current frontier entries
  validate    Validate all concept files against schema
`);
  }
}

main().catch(console.error);
