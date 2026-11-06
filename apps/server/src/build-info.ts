/**
 * Build identity reported by `/api/health`.
 *
 * Values arrive as environment variables baked into the image by CI. They exist
 * so a deployment can assert that the *running container* is the build it just
 * pushed: a 200 from the health endpoint cannot distinguish a fresh container
 * from an old one still serving the previous image.
 *
 * Every hop must keep the same names — workflow build-args → Dockerfile ARG →
 * Dockerfile ENV → process.env. Renaming any one of them silently falls back to
 * these defaults and the deployment appears to succeed while reporting "dev".
 */
export interface BuildInfo {
  version: string;
  commitHash: string;
  buildRef: string;
}

export function readBuildInfo(env: NodeJS.ProcessEnv = process.env): BuildInfo {
  return {
    version: env.APP_VERSION ?? "dev",
    commitHash: env.GIT_COMMIT_HASH ?? "unknown",
    buildRef: env.BUILD_REF ?? "unknown",
  };
}
