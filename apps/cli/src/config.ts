import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/**
 * Client configuration.
 *
 * Resolution order, first match wins:
 *   1. `--server` / `--token` flags
 *   2. `X_SAGE_SERVER` / `X_SAGE_TOKEN` environment variables
 *   3. `$XDG_CONFIG_HOME/x-sage/config.toml` (default `~/.config/x-sage/`)
 *
 * A config file is what makes the CLI usable without exporting variables in
 * every shell. The token belongs there rather than in a shell rc, which tends
 * to be copied into dotfile repos and backups.
 */
export interface ClientConfig {
  server?: string;
  token?: string;
}

/** Path of the config file, honouring XDG_CONFIG_HOME. */
export function configPath(env: NodeJS.ProcessEnv = process.env): string {
  const base = env.XDG_CONFIG_HOME ?? join(homedir(), ".config");
  return join(base, "x-sage", "config.toml");
}

/**
 * Minimal TOML reader: `key = "value"` lines plus `#` comments.
 *
 * Deliberately not a TOML parser. The file holds two flat string keys, and a
 * full parser would be a dependency plus a much larger failure surface for no
 * gain. Unknown keys and malformed lines are ignored rather than fatal, so a
 * hand-edited typo never makes every command fail.
 */
export function parseConfig(raw: string): ClientConfig {
  const config: ClientConfig = {};

  for (const line of raw.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#")) continue;

    const eq = trimmed.indexOf("=");
    if (eq === -1) continue;

    const key = trimmed.slice(0, eq).trim();
    let value = trimmed.slice(eq + 1).trim();

    // Strip matching quotes; leave bare values (e.g. 3001) as-is.
    const quoted =
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"));
    if (quoted && value.length >= 2) {
      value = value.slice(1, -1);
    }

    if (value === "") continue;
    if (key === "server") config.server = value;
    else if (key === "token") config.token = value;
  }

  return config;
}

/** Read the config file; a missing or unreadable file yields an empty config. */
export function readConfig(path: string = configPath()): ClientConfig {
  try {
    return parseConfig(readFileSync(path, "utf-8"));
  } catch {
    return {};
  }
}

export interface ResolvedTarget {
  server?: string;
  token?: string;
  /** Where `server` came from, for the "not configured" hint. */
  source: "flag" | "env" | "file" | "none";
}

/** Merge flags, environment and config file into one target. */
export function resolveTarget(
  flags: { server?: string; token?: string },
  env: NodeJS.ProcessEnv = process.env,
  path: string = configPath(env),
): ResolvedTarget {
  // An empty environment variable means "not configured", not "empty value":
  // a blank X_SAGE_SERVER should not shadow a usable config file.
  const fromEnv = (name: string): string | undefined => {
    const value = env[name];
    return value === undefined || value === "" ? undefined : value;
  };

  const file = readConfig(path);

  const server = flags.server ?? fromEnv("X_SAGE_SERVER") ?? file.server;
  const token = flags.token ?? fromEnv("X_SAGE_TOKEN") ?? file.token;

  const source: ResolvedTarget["source"] =
    flags.server !== undefined
      ? "flag"
      : fromEnv("X_SAGE_SERVER") !== undefined
        ? "env"
        : file.server !== undefined
          ? "file"
          : "none";

  return {
    ...(server === undefined ? {} : { server }),
    ...(token === undefined ? {} : { token }),
    source,
  };
}
