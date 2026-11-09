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

  test("installs ca-certificates in the runner", async () => {
    // Without it every HTTPS request inside the container fails.
    const dockerfile = await read("Dockerfile");
    const runnerStage = dockerfile.slice(dockerfile.indexOf("FROM base AS runner"));
    expect(runnerStage).toContain("ca-certificates");
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

  test("CLI has no local mode", async () => {
    // Every client goes through the REST API; a filesystem path would let the
    // CLI bypass the server and read a KB the deployment does not serve.
    const cli = await read("apps/cli/src/index.ts");
    expect(cli).not.toContain("--kb");
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
