import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

/**
 * CI contract tests.
 *
 * These assert the properties that make a pipeline trustworthy rather than
 * merely present: gates really fail, deployment can tell a fresh container from
 * a stale one, and the version triple survives every hop. Editing the workflow
 * or Dockerfile without honouring them turns these red.
 */

const ROOT = resolve(import.meta.dir, "..");

async function read(relative: string): Promise<string> {
  return Bun.file(resolve(ROOT, relative)).text();
}

test("the native gateway release validates the pinned Rust source and stages Bun before promotion", async () => {
  const script = await read("deploy/native-candidate/release-bun.sh");
  const ordered = [
    'rev-parse "$commit:sage-db"',
    "./scripts/ci.sh",
    'smoke "$preflight" "$short" main "$version"',
    'mv -f "$next" "$settings"',
    'smoke "$public" "$short" main "$version"',
  ].map((entry) => script.indexOf(entry));
  expect(ordered.every((index, position) => index > (ordered[position - 1] ?? -1))).toBe(true);
  expect(script).toContain('(umask 022; git --git-dir="$repo" archive');
  expect(script).toContain('x-sage-native-production up --pull never --no-build --wait --wait-timeout 75 bun');
  expect(script).toContain('cp -p "$slot/bun.env.previous" "$rollback"');
  expect(script).not.toContain("systemctl");
});

describe("workflow structure", () => {
  test("test job has no continue-on-error", async () => {
    // A gate that tolerates failure is not a gate.
    const workflow = await read(".github/workflows/docker-build.yml");
    const testJob = workflow.slice(workflow.indexOf("  test:"), workflow.indexOf("  build:"));
    expect(testJob).not.toContain("continue-on-error");
  });

  test("build requires test, deploy requires both", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    expect(workflow).toContain("  build:\n    needs: test");
    expect(workflow).toContain("  deploy:\n    needs: [test, build]");
  });

  test("notify runs on a hosted runner and always fires", async () => {
    // Notify is the alerting channel for a failed deploy. Sharing the
    // self-hosted runner would silence the alert exactly when the runner is down.
    const workflow = await read(".github/workflows/docker-build.yml");
    const notifyJob = workflow.slice(workflow.indexOf("  notify:"));
    expect(notifyJob).toContain("runs-on: ubuntu-latest");
    expect(notifyJob).toContain("always()");
  });

  test("deploy has a timeout", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    const deployJob = workflow.slice(workflow.indexOf("  deploy:"), workflow.indexOf("  notify:"));
    expect(deployJob).toContain("timeout-minutes");
  });

  test("deploy never runs for pull requests or tags", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    const deployJob = workflow.slice(workflow.indexOf("  deploy:"), workflow.indexOf("  notify:"));
    expect(deployJob).toContain("github.event_name != 'pull_request'");
    expect(deployJob).toContain("refs/tags/");
  });
});

describe("Dockerfile", () => {
  test("declares the version args in the base stage so both stages inherit them", async () => {
    // Declaring them only in the runner leaves the builder unable to inject the
    // build identity into the SPA bundle.
    const dockerfile = await read("Dockerfile");
    const baseStage = dockerfile.slice(0, dockerfile.indexOf("FROM base AS builder"));
    expect(baseStage).toContain("ARG APP_VERSION");
    expect(baseStage).toContain("ARG GIT_COMMIT_HASH");
    expect(baseStage).toContain("ARG BUILD_REF");
    expect(baseStage).toContain("ENV APP_VERSION=${APP_VERSION}");
    expect(baseStage).toContain("ENV GIT_COMMIT_HASH=${GIT_COMMIT_HASH}");
    expect(baseStage).toContain("ENV BUILD_REF=${BUILD_REF}");
  });

  test("the runner verifies its TLS trust bundle", async () => {
    // Without it every HTTPS request inside the container fails.
    const dockerfile = await read("Dockerfile");
    const runnerStage = dockerfile.slice(dockerfile.indexOf("FROM base AS runner"));
    expect(runnerStage).toContain("install -y --no-install-recommends ca-certificates");
    expect(runnerStage).toContain("test -s /etc/ssl/certs/ca-certificates.crt");
  });

  test("health check probes the container's own port", async () => {
    const dockerfile = await read("Dockerfile");
    expect(dockerfile).toContain("localhost:3000/api/health");
    expect(dockerfile).toContain("EXPOSE 3000");
  });

  test("bakes the KB into the image", async () => {
    const dockerfile = await read("Dockerfile");
    expect(dockerfile).toContain("COPY --from=builder /app/kb ./kb");
    expect(dockerfile).toContain("ENV X_SAGE_KB=/app/kb/default");
  });

  test("does not copy a node_modules the CLI never gets", async () => {
    // The CLI declares no dependencies, so bun creates no apps/cli/node_modules
    // in a clean checkout. Copying it worked only where a stale directory from
    // before that change still existed, and broke the first CI build.
    const dockerfile = await read("Dockerfile");
    const runnerStage = dockerfile.slice(dockerfile.indexOf("FROM base AS runner"));
    expect(runnerStage).not.toContain("apps/cli/node_modules");
    expect(runnerStage).not.toContain("COPY --from=builder /app/apps/cli/src");
  });

  test("does not ship devDependencies in the runtime stage", async () => {
    // typescript alone is 23 MB and is never loaded at runtime.
    const dockerfile = await read("Dockerfile");
    expect(dockerfile).toContain("--production");
    const runnerStage = dockerfile.slice(dockerfile.indexOf("FROM base AS runner"));
    expect(runnerStage).toContain("COPY --from=prod-deps");
    expect(runnerStage).not.toContain("COPY --from=builder /app/node_modules");
  });
});

describe("compose", () => {
  test("image tag is interpolable", async () => {
    // A hard-coded tag makes `docker pull <tag>` succeed while compose silently
    // starts the previous image.
    const compose = await read("docker-compose.yml");
    expect(compose).toMatch(/image: ghcr\.io\/xartifact\/x-sage:\$\{IMAGE_TAG:-/);
  });

  test("does not override the baked build identity", async () => {
    // Writing ${GIT_COMMIT_HASH:-unknown} here would replace the correct value
    // baked into the image with "unknown" at runtime.
    const compose = await read("docker-compose.yml");
    const environment = compose.slice(compose.indexOf("environment:"), compose.indexOf("ports:"));
    // Ignore comments: the block explains *why* the variables are absent, which
    // means their names legitimately appear in prose.
    const assignments = environment
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "" && !line.startsWith("#"));

    for (const name of ["APP_VERSION", "GIT_COMMIT_HASH", "BUILD_REF"]) {
      expect(assignments.some((line) => line.startsWith(`${name}:`))).toBe(false);
    }
  });

  test("probes the container port, not the host mapping", async () => {
    const compose = await read("docker-compose.yml");
    expect(compose).toContain("localhost:3000/api/health");
  });
});

describe("build identity wiring", () => {
  test("server reports the three values", async () => {
    const buildInfo = await read("apps/server/src/build-info.ts");
    expect(buildInfo).toContain("APP_VERSION");
    expect(buildInfo).toContain("GIT_COMMIT_HASH");
    expect(buildInfo).toContain("BUILD_REF");
  });

  test("health endpoint exposes them", async () => {
    const app = await read("apps/server/src/app.ts");
    const health = app.slice(app.indexOf('"/api/health"'), app.indexOf('"/api/meta"'));
    expect(health).toContain("readBuildInfo");
  });

  test("CLI scopes REST by KB id and isolates offline administration", async () => {
    const cli = await read("apps/cli/src/index.ts");
    const remote = cli.slice(cli.indexOf("async function runRemote"), cli.indexOf("async function runAdmin"));
    expect(remote).toContain("url.searchParams.set");
    expect(remote).not.toContain("Bun.file");
    expect(cli).toContain("invocation.query.kb = kbFlag");
    expect(cli).toContain('if (command === "admin")');
    expect(cli).not.toContain("X_SAGE_KB");
    expect(cli).not.toContain("handleRequest");
  });

  test("route table is resource-shaped, not verb-shaped", async () => {
    const api = await read("packages/service/src/api.ts");
    // Regeneration is idempotent, so it is PUT on the resource.
    expect(api).toContain('path === "/graph" && method === "PUT"');
    expect(api).toContain('path === "/manifest" && method === "PUT"');
    // Proposal kind travels in the body, not the path.
    expect(api).toContain('path === "/proposals" && method === "POST"');
    expect(api).not.toContain('"/proposals/edge"');
    expect(api).not.toContain('"/proposals/atom"');
    expect(api).not.toContain('"/graph/regenerate"');
  });

  test("workflow passes all three as build-args with matching names", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    for (const name of ["APP_VERSION", "GIT_COMMIT_HASH", "BUILD_REF"]) {
      expect(workflow).toContain(`${name}=`);
    }
  });
});

describe("deployment bootstrap", () => {
  test("bootstrap script keeps the runner able to read the deploy dir", async () => {
    // The CI deploy job runs as github-actions and must be able to read
    // docker-compose.yml. Handing the directory solely to a human user would
    // break every subsequent deploy, so the script sets a shared group plus
    // setgid rather than a plain chown.
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain("chmod 2775");
    expect(script).toContain("github-actions");
    expect(script).toContain("chmod 664");
  });

  test("bootstrap leaves .env readable by the deploy runner", async () => {
    // `docker compose up` reads .env as github-actions before any container
    // starts. A 600 binzhan-owned .env makes the deploy job pull the new image
    // and then fail on `open .env: permission denied`, leaving the old
    // container running and every concept invisible.
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain("chmod 640");
    expect(script).not.toContain("chmod 600");
  });

  test("bootstrap does not embed its own compose copy", async () => {
    // An embedded copy drifts from the repo silently, and the drift is what
    // lost x99's volume: the host compose omitted `./data/kb:/app/kb`, so every
    // write landed in the container's writable layer and vanished on recreate.
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain("curl -fsSL");
    expect(script).toContain("docker-compose.yml");
    expect(script).not.toContain("<<'COMPOSE'");
  });

  test("compose mounts the KB so writes survive container recreation", async () => {
    const compose = await read("docker-compose.yml");
    expect(compose).toContain("./data/kb:/app/kb");
  });

  test("bootstrap seeds the volume before first boot", async () => {
    // The bind mount hides the image's KB; an empty host dir would leave the
    // server with nothing to serve.
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain("docker cp");
    expect(script).toContain("data/kb");
  });

  test("bootstrap always provisions a write token", async () => {
    // The container binds 0.0.0.0, and the server refuses to start without a
    // token in that case. A bootstrap that skipped it would produce a restart
    // loop rather than a running service.
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain("X_SAGE_WRITE_TOKEN");
    expect(script).toContain("openssl rand");
  });

  test("bootstrap refuses to run unprivileged", async () => {
    const script = await read("scripts/bootstrap-x99.sh");
    expect(script).toContain('id -u');
    expect(script).toContain("must run as root");
  });
});

describe("multi-arch build safety", () => {
  test("SPA is prebuilt on a native runner, not under QEMU", async () => {
    // esbuild segfaults deterministically when emulated. A multi-arch build that
    // runs `vite build` inside buildx will fail every time.
    const workflow = await read(".github/workflows/docker-build.yml");
    const testJob = workflow.slice(workflow.indexOf("  test:"), workflow.indexOf("  build:"));
    expect(testJob).toContain("Build SPA");
    expect(testJob).toContain("upload-artifact");
    expect(testJob).toContain("viz-dist");

    const buildJob = workflow.slice(workflow.indexOf("  build:"), workflow.indexOf("  deploy:"));
    expect(buildJob).toContain("download-artifact");
    expect(buildJob).toContain("PREBUILT_VIZ=true");
  });

  test("Dockerfile fails loudly when PREBUILT_VIZ is set but dist is missing", async () => {
    // Silently falling back to `vite build` would reintroduce the QEMU crash.
    const dockerfile = await read("Dockerfile");
    expect(dockerfile).toContain("PREBUILT_VIZ=true but viz-dist/index.html is missing");
  });

  test("upload path matches the build output, and missing files fail", async () => {
    // Both halves of the handoff must agree: `vite build` writes apps/viz/dist,
    // and a wrong upload path with the default `warn` silently produces no
    // artifact — the build job then fails with a confusing "not found".
    const workflow = await read(".github/workflows/docker-build.yml");
    const uploadStep = workflow.slice(
      workflow.indexOf("name: Upload SPA bundle"),
      workflow.indexOf("  build:"),
    );
    expect(uploadStep).toContain("path: apps/viz/dist/");
    expect(uploadStep).toContain("if-no-files-found: error");

    const downloadStep = workflow.slice(
      workflow.indexOf("name: Download SPA bundle"),
      workflow.indexOf("setup-qemu-action"),
    );
    expect(downloadStep).toContain("name: viz-dist");
  });

  test("prebuilt SPA travels in its own directory, outside .dockerignore", async () => {
    // `apps/viz/dist` is gitignored and dockerignored: the working-tree copy may
    // be stale, so it must never silently become the shipped bundle.
    const dockerignore = await read(".dockerignore");
    expect(dockerignore).toContain("apps/viz/dist");

    const dockerfile = await read("Dockerfile");
    expect(dockerfile).toContain("viz-dist/index.html");

    const workflow = await read(".github/workflows/docker-build.yml");
    expect(workflow).toContain("path: viz-dist/");
  });
});

describe("client install", () => {
  const installer = () => read("scripts/install-cli.sh");

  test("pins one commit before downloading anything", async () => {
    // Downloading per file from a floating branch mixes versions when upstream
    // pushes mid-install: index.ts from one commit, build-info.ts from another.
    const script = await installer();
    const resolve = script.indexOf("resolve_commit");
    const fetchLoop = script.indexOf('fetch "$BASE/$COMMIT');
    expect(resolve).toBeGreaterThan(-1);
    expect(fetchLoop).toBeGreaterThan(resolve);
    expect(script).toContain('"$BASE/$COMMIT/apps/cli/src/$file"');
    expect(script).toContain('"$BASE/$COMMIT/skills/x-sage-kb/$file"');
    // No download path may still use the floating ref.
    expect(script).not.toMatch(/\$BASE\/\$REF/);
  });

  test("never overwrites its entry point with a redirect", async () => {
    // `> bin/x-sage` follows an existing symlink. On a development machine that
    // link points into a working tree, so the redirect would rewrite source.
    // Behaviour is covered in scripts/install-cli.test.ts; this guards the shape.
    // Comments legitimately mention the pattern as a counter-example, so only
    // executable lines are inspected.
    const code = (await installer())
      .split("\n")
      .filter((line) => !line.trimStart().startsWith("#"))
      .join("\n");
    expect(code).not.toMatch(/>\s*"?\$BIN_PATH/);
    expect(code).toContain("replace_link");
    expect(code).toContain("node:fs");
  });

  test("swaps the current version by renaming a link, not by moving a directory", async () => {
    // `mv src current` moves src *into* the directory current points at, on both
    // BSD and GNU. rename(2) replaces the entry, which is what a switch needs.
    const script = await installer();
    expect(script).toContain("renameSync");
    expect(script).not.toMatch(/mv -f "\$CURRENT\.new" "\$CURRENT"/);
    expect(script).toContain('CURRENT="$ROOT/current"');
  });

  test("fails instead of writing an unknown commit", async () => {
    const script = await installer();
    expect(script).toContain("nothing was installed");
    expect(script).toMatch(/cannot resolve .* to a commit/);
  });

  test("switches only after the staged tree passes its smoke checks", async () => {
    const script = await installer();
    // The last staged verification must precede the link swap. `|| true` on the
    // rollback line and the report line both contain the call, so the checks are
    // matched by their die-message instead.
    const lastCheck = script.lastIndexOf('does not document');
    // Not `indexOf`: the helper is defined near the top, so the first match is
    // the definition, not the switch.
    const switchLink = script.lastIndexOf('replace_link "$CURRENT.new" "$CURRENT"');
    expect(lastCheck).toBeGreaterThan(-1);
    expect(switchLink).toBeGreaterThan(lastCheck);
  });

  test("never rewrites the client config", async () => {
    const script = await installer();
    expect(script).not.toMatch(/>\s*"?\$HOME\/\.config\/x-sage/);
    expect(script).not.toContain("config.toml <<");
    expect(script).toContain("config kept as-is");
  });

  test("installs a self-contained CLI snapshot", async () => {
    // `@x-sage/service` is a type-only import, so the client is a handful of
    // source files. A value import would make the one-liner install a client
    // that cannot resolve its dependency.
    const cli = await read("apps/cli/src/index.ts");
    expect(cli).toContain('import type { ApiMethod, ApiResponse } from "@x-sage/service"');
    expect(cli).not.toMatch(/^import \{[^}]*\} from "@x-sage\//m);

    // Every relative import must be in the download list, or the installed
    // snapshot fails at import time.
    const script = await installer();
    const listed = script.slice(script.indexOf("CLI_FILES="), script.indexOf("\n", script.indexOf("CLI_FILES=")));
    for (const file of ["index.ts", "config.ts", "build-info.ts"]) {
      expect(listed).toContain(file);
    }
  });

  test("README documents the installer it ships, including pinning and rollback", async () => {
    const readme = await read("README.md");
    expect(readme).toContain("scripts/install-cli.sh");
    for (const variable of ["X_SAGE_REF", "X_SAGE_PREFIX", "X_SAGE_SKILLS_DIR"]) {
      expect(readme).toContain(variable);
    }
    // A pin example must use a placeholder: a real-but-stale hash installs a
    // commit that predates the files being documented.
    expect(readme).toContain("<完整40位commit>");
    expect(readme).not.toMatch(/X_SAGE_REF=[0-9a-f]{40}/);
    // The production host is tailnet-only; a reader must not treat the failure
    // as an outage.
    expect(readme).toContain("tailnet");
    // Rollback advice must match the link-based layout.
    expect(readme).toContain("versions/");
    // Reporting "installed" while an older PATH entry wins is how a machine ends
    // up running a stale client that looks freshly installed.
    expect(readme).toContain("command -v -a x-sage");
    expect(await installer()).toContain("comes first on PATH");
  });
});

describe("sage-db gate", () => {
  test("the Rust gate is mandatory, never conditional", async () => {
    // A workspace-optional gate stops being a gate the first time someone
    // deletes the directory to "unblock" CI.
    const script = await read("scripts/ci.sh");
    expect(script).toContain("workspace present");
    expect(script).toContain("is missing; the Rust gate is mandatory");
    expect(script).toContain("the toolchain pin is mandatory");
    expect(script).not.toMatch(/if \[ -f sage-db\/Cargo\.toml \]/);
  });

  test("each Rust step can fail and aborts the script", async () => {
    const script = await read("scripts/ci.sh");
    expect(script).toContain("sage-db: fmt");
    expect(script).toContain("cargo fmt --all -- --check");
    expect(script).toContain("cargo clippy --workspace --all-targets --locked -- -D warnings");
    expect(script).toContain("cargo test --workspace --locked");
    // `set -euo pipefail` plus per-step subshells means a failing step exits.
    expect(script).toContain("set -euo pipefail");
    expect(script).toContain("( cd sage-db && cargo test --workspace --locked )");
  });

  test("a failing cargo tree cannot pass the dependency gate", async () => {
    // `if pipeline; then` swallows the pipeline's exit status, so a broken
    // manifest or a missing crate used to look like "no forbidden edge".
    const script = await read("scripts/ci.sh");
    const gate = script.slice(
      script.indexOf("check_no_edge()"),
      script.indexOf("echo \"==> all gates passed\""),
    );
    expect(gate).toContain("cargo tree failed for");
    expect(gate).toMatch(/if ! \( cd sage-db && cargo tree/);
    expect(gate).toContain("exit 1");
    // Content check runs only after the tree command succeeded.
    expect(gate.indexOf("cargo tree failed")).toBeLessThan(gate.indexOf("must not depend on"));
  });

  test("dependency gate covers dev and build edges where they matter", async () => {
    const script = await read("scripts/ci.sh");
    // reference must not reach infer through *any* edge kind.
    expect(script).toContain("check_no_edge sage-db-reference sage-db-infer normal,build,dev");
    // infer may dev-depend on reference, but not in production or build.
    expect(script).toContain("check_no_edge sage-db-infer sage-db-reference normal,build");
    // types must stay dependency-free.
    expect(script).toContain("check_no_edge sage-db-types sage-db-infer");
    expect(script).toContain("check_no_edge sage-db-types sage-db-reference");
  });

  test("the store's direct allowlist is an exact set, not a subset check", async () => {
    const script = await read("scripts/ci.sh");
    // The gate must compare the DIRECT normal dependency set against a fixed list, so an
    // unapproved dependency fails even when no forbidden edge exists. A "subset" or
    // "no forbidden name" formulation would pass for a brand-new undeclared crate.
    expect(script).toContain("check_direct_allowlist()");
    expect(script).toContain(
      "check_direct_allowlist sage-db-store sage-db-types sage-db-format cap-std triomphe",
    );
    const gate = script.slice(
      script.indexOf("check_direct_allowlist()"),
      script.indexOf("check_no_edge sage-db-format sage-db-infer"),
    );
    // Equality, both directions: extra and missing dependencies both fail.
    expect(gate).toContain('if [ "$actual" != "$expected" ]');
    expect(gate).toContain("direct dependencies changed");
    // Normal edges only, and depth 1, so the name is honest about its scope.
    expect(gate).toContain("--edges normal --depth 1");
  });

  test("the allowlist gate fails closed when cargo tree fails", async () => {
    const script = await read("scripts/ci.sh");
    const gate = script.slice(
      script.indexOf("check_direct_allowlist()"),
      script.indexOf("check_no_edge sage-db-format sage-db-infer"),
    );
    // A broken manifest or renamed crate must not read as "allowlist satisfied".
    expect(gate).toContain("cannot verify direct dependencies");
    expect(gate).toMatch(/if ! \( cd sage-db && cargo tree/);
    expect(gate).toContain("exit 1");
    expect(gate.indexOf("cannot verify direct dependencies")).toBeLessThan(
      gate.indexOf('if [ "$actual" != "$expected" ]'),
    );
  });

  test("store cannot reach infer or the harness through any edge", async () => {
    const script = await read("scripts/ci.sh");
    // Storage performs no inference.
    expect(script).toContain(
      "check_no_edge sage-db-store sage-db-infer normal,build,dev",
    );
    // The independent oracle must never be reachable from production code, including
    // through a dev-dependency (which is how it would sneak into `cargo test`).
    expect(script).toContain(
      "check_no_edge sage-db-store sage-db-store-tests normal,build,dev",
    );
    // format is the lower layer: it must not depend on store or infer.
    expect(script).toContain("check_no_edge sage-db-format sage-db-store normal,build,dev");
    expect(script).toContain("check_no_edge sage-db-format sage-db-infer normal,build,dev");
  });

  test("the kernel graph admission uses canonical types, Store and infer", async () => {
    const script = await read("scripts/ci.sh");
    expect(script).toContain("check_direct_allowlist sage-db-kernel sage-db-types sage-db-store sage-db-infer");
    const manifest = await read("sage-db/crates/kernel/Cargo.toml");
    const production = manifest.split("[dependencies]")[1]?.split("[dev-dependencies]")[0];
    expect(production?.trim()).toBe("sage-db-types.workspace = true\nsage-db-store.workspace = true\n# Canonical graph admission, authorized by system.md section 3.\nsage-db-infer.workspace = true");
  });

  test("server commands use canonical grants, protocol and Store with approved mature crypto", async () => {
    const script = await read("scripts/ci.sh");
    expect(script).toContain("check_direct_allowlist sage-db-server sage-db-types sage-db-kernel sage-db-store sage-db-protocol sha2 subtle httparse serde serde_json sage-db-query regex-syntax chrono");
    expect(script).toContain("check_no_edge sage-db-server sage-db-store-tests normal,build,dev");
    const manifest = await read("sage-db/crates/server/Cargo.toml");
    expect(manifest).toContain('sha2 = "0.10.9"');
    expect(manifest).toContain('subtle = { version = "2.6.1", default-features = false }');
  });

  test("ci.sh asserts the pinned toolchain actually took effect", async () => {
    const script = await read("scripts/ci.sh");
    expect(script).toContain("sage-db/rust-toolchain.toml");
    expect(script).toContain("does not match pinned");
    const pin = await read("sage-db/rust-toolchain.toml");
    expect(pin).toContain("1.98.1");
    expect(pin).toContain('components = ["rustfmt", "clippy"]');
  });

  test("the workflow installs the pinned toolchain before the gates", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    const testJob = workflow.slice(workflow.indexOf("  test:"), workflow.indexOf("  build:"));
    const install = testJob.indexOf("Install pinned Rust toolchain");
    const gates = testJob.indexOf("./scripts/ci.sh");
    expect(install).toBeGreaterThan(-1);
    expect(gates).toBeGreaterThan(install);
    expect(testJob).toContain("rustup toolchain install");
    expect(testJob).not.toContain("continue-on-error");
  });

  test("reference depends only on types, infer only dev-depends on reference", async () => {
    // The manifest is the structural guarantee that the reference executor
    // cannot call optimized helpers to compute its "expected" answer.
    const reference = await read("sage-db/crates/reference/Cargo.toml");
    const infer = await read("sage-db/crates/infer/Cargo.toml");
    const types = await read("sage-db/crates/types/Cargo.toml");

    expect(reference).toContain("sage-db-types");
    expect(reference).not.toContain("sage-db-infer");
    expect(reference).not.toContain("dev-dependencies");

    expect(types).not.toContain("dependencies]");

    const inferDeps = infer.slice(infer.indexOf("[dependencies]"));
    expect(inferDeps).toContain("sage-db-types");
    expect(inferDeps).toContain("[dev-dependencies]");
    const production = inferDeps.slice(
      inferDeps.indexOf("[dependencies]"),
      inferDeps.indexOf("[dev-dependencies]"),
    );
    expect(production).not.toContain("sage-db-reference");
  });

  test("the workspace keeps build output out of version control", async () => {
    const ignore = await read("sage-db/.gitignore");
    expect(ignore).toContain("/target");
  });

  test("docs keep the Rust scope marks and no SQLite backend", async () => {
    // A regression guard for the approved direction: sage-db is self-developed,
    // and the design docs must not drift back to describing a SQLite engine.
    const system = await read("docs/design/system.md");
    expect(system).toContain("sage-db");
    expect(system).toContain("不依赖 SQLite");
    const adr = await read("docs/design/adr-001-sage-db.md");
    expect(adr).toContain("不采用");
  });
});

describe("sage-db protocol and query dependency boundaries", () => {
  test("protocol and query have explicit production allowlists and cannot reach storage/business", async () => {
    const ci = await read("scripts/ci.sh");
    expect(ci).toContain("check_direct_allowlist sage-db-protocol sage-db-types prost pbjson pbjson-types serde serde_json");
    expect(ci).toContain("check_direct_allowlist sage-db-query sage-db-types sage-db-infer");
    for (const target of ["sage-db-store", "sage-db-kernel"]) {
      expect(ci).toContain(`check_no_edge sage-db-protocol ${target} normal,build,dev`);
      expect(ci).toContain(`check_no_edge sage-db-query ${target} normal,build`);
    }
  });
});

describe("sage-db reproducible protocol generation", () => {
  test("CI provisions pinned generators and drift checking is a required gate", async () => {
    const workflow = await read(".github/workflows/docker-build.yml");
    const job = workflow.slice(workflow.indexOf("  test:"), workflow.indexOf("  build:"));
    expect(job).toContain('setup-sage-db-codegen.sh "$RUNNER_TEMP/sage-codegen"');
    expect(job.indexOf("Install pinned protocol generators")).toBeLessThan(job.indexOf("Quality gates + tests"));
    expect(await read("scripts/ci.sh")).toContain("bun tools/generate-sage-db-protocol.ts --check");
    const pkg = JSON.parse(await read("tools/protocol-codegen/package.json"));
    expect(pkg.dependencies["ts-proto"]).toBe("2.6.1");
  });
});


describe("sage-db administrative import dependency direction", () => {
  test("import tooling cannot be reached from serving and canonical crates", async () => {
    const ci = await read("scripts/ci.sh");
    expect(ci).toContain("check_direct_allowlist sage-db-migrate sage-db-types sage-db-format sage-db-store sage-db-kernel serde serde_json");
    expect(ci).toContain("for canonical in sage-db-types sage-db-format sage-db-store sage-db-kernel sage-db-infer sage-db-reference sage-db-query sage-db-protocol sage-db-server; do");
    expect(ci).toContain('check_no_edge "$canonical" sage-db-migrate normal,build,dev');
    const manifest = await read("sage-db/crates/migrate/Cargo.toml");
    const production = manifest.split("[dependencies]")[1]?.split("[dev-dependencies]")[0];
    expect(production).not.toContain("sage-db-store-tests");
    expect(manifest.split("[dev-dependencies]")[1]?.trim()).toBe("sage-db-store-tests.workspace = true");
  });
});
