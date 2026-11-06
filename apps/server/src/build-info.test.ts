import { describe, expect, test } from "bun:test";
import { readBuildInfo } from "./build-info.ts";

/**
 * The version triple travels workflow → Dockerfile ARG → Dockerfile ENV →
 * process.env. A rename at any hop silently degrades to these defaults, which is
 * exactly how a production instance reports "dev" for months unnoticed.
 */
describe("readBuildInfo", () => {
  test("reads the three CI-provided variables", () => {
    const info = readBuildInfo({
      APP_VERSION: "1.2.3",
      GIT_COMMIT_HASH: "abc1234",
      BUILD_REF: "main",
    } as NodeJS.ProcessEnv);

    expect(info).toEqual({ version: "1.2.3", commitHash: "abc1234", buildRef: "main" });
  });

  test("falls back to sentinel values when unset", () => {
    const info = readBuildInfo({} as NodeJS.ProcessEnv);
    expect(info).toEqual({ version: "dev", commitHash: "unknown", buildRef: "unknown" });
  });
});
