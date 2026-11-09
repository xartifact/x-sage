import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile, chmod } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { configPath, parseConfig, readConfig, resolveTarget } from "./config.ts";

describe("parseConfig", () => {
  test("reads flat string keys", () => {
    const config = parseConfig('server = "https://example.com"\ntoken = "abc"');
    expect(config).toEqual({ server: "https://example.com", token: "abc" });
  });

  test("ignores comments and blank lines", () => {
    const config = parseConfig('# a comment\n\nserver = "x"\n\n');
    expect(config).toEqual({ server: "x" });
  });

  test("accepts single quotes and unquoted values", () => {
    expect(parseConfig("server = 'https://a.b'").server).toBe("https://a.b");
    expect(parseConfig("server = https://a.b").server).toBe("https://a.b");
  });

  test("ignores unknown keys and malformed lines", () => {
    // A hand-edited typo must not make every command fail.
    const config = parseConfig('server = "x"\nwhoops\ngarbage = "y"');
    expect(config).toEqual({ server: "x" });
  });

  test("treats empty values as absent", () => {
    expect(parseConfig('server = ""').server).toBeUndefined();
  });

  test("keeps trailing path segments intact", () => {
    // Splitting on whitespace would truncate `.../a=b`; splitting on the first
    // `=` only is what makes this work.
    expect(parseConfig('server = "https://x/api?q=a=b"').server).toBe("https://x/api?q=a=b");
  });
});

describe("readConfig", () => {
  test("returns an empty config when the file is missing", () => {
    expect(readConfig("/nonexistent/x-sage/config.toml")).toEqual({});
  });
});

describe("configPath", () => {
  test("honours XDG_CONFIG_HOME", () => {
    expect(configPath({ XDG_CONFIG_HOME: "/custom" } as NodeJS.ProcessEnv)).toBe(
      "/custom/x-sage/config.toml",
    );
  });

  test("falls back to ~/.config", () => {
    expect(configPath({} as NodeJS.ProcessEnv)).toEndWith(".config/x-sage/config.toml");
  });
});

describe("resolveTarget", () => {
  const dir = () => mkdtemp(join(tmpdir(), "x-sage-cfg-"));

  test("flags win over environment and file", async () => {
    const home = await dir();
    try {
      const path = join(home, "config.toml");
      await writeFile(path, 'server = "file"\ntoken = "file-token"');
      const target = resolveTarget(
        { server: "flag", token: "flag-token" },
        { X_SAGE_SERVER: "env", X_SAGE_TOKEN: "env-token" } as NodeJS.ProcessEnv,
        path,
      );
      expect(target.server).toBe("flag");
      expect(target.token).toBe("flag-token");
      expect(target.source).toBe("flag");
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  });

  test("environment wins over the file", async () => {
    const home = await dir();
    try {
      const path = join(home, "config.toml");
      await writeFile(path, 'server = "file"');
      const target = resolveTarget({}, { X_SAGE_SERVER: "env" } as NodeJS.ProcessEnv, path);
      expect(target.server).toBe("env");
      expect(target.source).toBe("env");
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  });

  test("the file supplies a server when nothing else does", async () => {
    const home = await dir();
    try {
      const path = join(home, "config.toml");
      await writeFile(path, 'server = "file"\ntoken = "t"');
      const target = resolveTarget({}, {} as NodeJS.ProcessEnv, path);
      expect(target.server).toBe("file");
      expect(target.token).toBe("t");
      expect(target.source).toBe("file");
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  });

  test("an empty environment variable does not shadow the file", async () => {
    // `X_SAGE_SERVER=` in a shell means "not set", not "connect to nowhere".
    const home = await dir();
    try {
      const path = join(home, "config.toml");
      await writeFile(path, 'server = "file"');
      const target = resolveTarget({}, { X_SAGE_SERVER: "" } as NodeJS.ProcessEnv, path);
      expect(target.server).toBe("file");
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  });

  test("reports none when nothing is configured", () => {
    const target = resolveTarget({}, {} as NodeJS.ProcessEnv, "/nonexistent/config.toml");
    expect(target.server).toBeUndefined();
    expect(target.source).toBe("none");
  });
});
