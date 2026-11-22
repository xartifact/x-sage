import { mkdir } from "node:fs/promises";
import { isAbsolute, join } from "node:path";
import { writeFileAtomic } from "@x-sage/core";
import type { NativeTermReader } from "@x-sage/service";

/** Regenerate compatibility exports under an operator-owned projection directory. */
export function connectNativeDerivedArtifacts(directory: string): NonNullable<NativeTermReader["publishArtifacts"]> {
  if (!isAbsolute(directory)) throw new Error("The projection directory must be absolute");
  let tail = Promise.resolve();
  let graphVersion = 0n;
  let manifestVersion = 0n;
  return (basis, files) => {
    const version = BigInt(basis);
    const publication = tail.then(async () => {
      await mkdir(directory, { recursive: true });
      if (files.graph !== undefined && version >= graphVersion) {
        await writeFileAtomic(join(directory, "graph.json"), files.graph);
        graphVersion = version;
      }
      if (files.manifest !== undefined && version >= manifestVersion) {
        await writeFileAtomic(join(directory, "MANIFEST.md"), files.manifest);
        manifestVersion = version;
      }
    });
    tail = publication.catch(() => {});
    return publication;
  };
}
