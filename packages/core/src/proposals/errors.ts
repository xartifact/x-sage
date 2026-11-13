/**
 * Errors that carry a distinct HTTP meaning.
 *
 * Kept apart from the merge code so the body path and the edge/atom path can
 * report the same conditions without importing each other.
 */

/** Raised when a proposal was already merged; callers treat it as success. */
export class AlreadyMergedError extends Error {
  constructor(proposalFile: string) {
    super(`Proposal already merged: ${proposalFile}`);
  }
}

/**
 * Raised when a proposal's baseline no longer matches the concept it targets.
 *
 * Distinct from a generic failure because the caller must report it as a
 * conflict (409) with the current revision: the proposal itself is fine, it is
 * simply out of date, and it stays reviewable.
 */
export class ProposalConflictError extends Error {
  constructor(
    message: string,
    readonly proposalFile: string,
  ) {
    super(message);
  }
}

/** A named merge target that does not exist under `proposals/`. */
export class ProposalTargetNotFoundError extends Error {
  constructor(readonly target: string) {
    super(`No such proposal: ${target}`);
  }
}