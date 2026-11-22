//! Sequential external-world model for commit attempts.
//!
//! # Why this replaced a set-based model
//!
//! An earlier version of this module folded attempts into a `BTreeSet<LogicalRequest>` and checked
//! membership. That was wrong in four ways, each found by review:
//!
//! 1. It treated `NotCommitted` as a permanent ban on the key, so the legal history
//!    "first attempt NotCommitted, retry Durable" was rejected. `NotCommitted` is a fact about one
//!    **attempt**, not about the logical key.
//! 2. It flagged a `PayloadMismatch` whenever the same key carried two digests, even when the second
//!    attempt was correctly **rejected**. A store that refuses a changed payload is behaving
//!    correctly; the refusal is not a violation.
//! 3. A set loses multiplicity and versions, so two distinct keys acknowledged at the **same**
//!    version looked fine, and a duplicated commit was indistinguishable from a single one.
//! 4. Set membership let an unrelated request through whenever some `Unknown` attempt existed.
//!
//! The fix is to model the external world the way a caller sees it: a base version plus an ordered
//! sequence of versioned commits. A candidate recovered state is compared as a **sequence**, with
//! versions and multiplicity intact.
//!
//! # The world
//!
//! ```text
//! World { base_version, commits: [Commit { version, request }] }
//! ```
//!
//! `commits[i].version == base_version + 1 + i` - versions are contiguous, no holes, no repeats.
//! `LogicalRequest` is `(principal, request_id, payload_digest)`; the **key** is `(principal,
//! request_id)` without the digest, because a key may be retried with a different payload and the
//! store's correct response to that is a refusal, not a second commit.
//!
//! # How attempts constrain the world
//!
//! * `Durable { version }` - either the request is already present at exactly `version` (an exact
//!   retry: no append), or the key is absent and the request appends at the next version, which must
//!   equal `version`. A key already carrying a **different** digest cannot append, so a `Durable`
//!   there contradicts the history.
//! * `NotCommitted` - the world is unchanged. It constrains nothing else: the same key may legally
//!   commit on a later attempt.
//! * `Unknown` - the world is either unchanged or appended, and only when the key is new. This is the
//!   only branching point; every other outcome is deterministic.
//!
//! # Bounded scope, stated rather than implied
//!
//! Sequential attempts from one writer, at most [`MAX_ATTEMPTS`] of them, expanding to at most
//! [`MAX_WORLDS`] candidate worlds. Concurrent writers are not modelled: interleaved attempts can
//! legitimately produce one commit plus one refusal, which is a different model. Exceeding either
//! bound is reported as an error, never silently truncated.
//!
//! This module judges **which commit sequences are legal**. It does not read record contents; whole
//! -record assertions belong to the other oracle layer. It never calls production fold, replay or
//! recovery, and does not reference `sage_db_store`.

use std::collections::BTreeMap;

/// Maximum attempts the model will replay.
pub const MAX_ATTEMPTS: usize = 64;

/// Maximum candidate worlds after expansion over `Unknown` outcomes.
pub const MAX_WORLDS: usize = 256;

/// The store's identity for a logical request.
///
/// `attempt_id` is deliberately absent: a retry is a new attempt of the same logical request, and
/// conflating the two is the bug this module exists to catch.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct LogicalRequest {
    /// Verified principal, ≤ 120 bytes per `format::field::MAX_PRINCIPAL_ID_BYTES`.
    pub principal: String,
    /// Caller request id, ≤ 128 bytes per `format::field::MAX_REQUEST_ID_BYTES`.
    pub request_id: String,
    /// Digest of the canonical request payload.
    pub payload_digest: [u8; 32],
}

impl LogicalRequest {
    /// Build a logical request, enforcing the declared bounds.
    ///
    /// # Panics
    ///
    /// Panics when a bound is exceeded: this is a test-model constructor, and truncating silently
    /// would hide a test bug.
    #[must_use]
    pub fn new(principal: &str, request_id: &str, payload_digest: [u8; 32]) -> Self {
        assert!(
            principal.len() <= sage_db_format::field::MAX_PRINCIPAL_ID_BYTES as usize,
            "principal exceeds the declared bound"
        );
        assert!(
            request_id.len() <= sage_db_format::field::MAX_REQUEST_ID_BYTES as usize,
            "request id exceeds the declared bound"
        );
        Self {
            principal: principal.to_owned(),
            request_id: request_id.to_owned(),
            payload_digest,
        }
    }

    /// The identity used for retry matching: principal plus request id, **without** the digest.
    #[must_use]
    pub fn key(&self) -> (&str, &str) {
        (&self.principal, &self.request_id)
    }
}

/// What the caller was told about one attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttemptOutcome {
    /// The write is durable at this commit version.
    Durable {
        /// Commit version reported to the caller.
        version: u64,
    },
    /// The attempt did not commit. Says nothing about later attempts of the same key.
    NotCommitted,
    /// The outcome cannot be determined.
    Unknown,
}

/// One attempt and its reported outcome.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Attempt {
    /// Caller-chosen attempt identity, distinct across retries.
    pub attempt_id: String,
    /// The logical request carried.
    pub request: LogicalRequest,
    /// What the caller was told.
    pub outcome: AttemptOutcome,
}

/// One committed version of one logical request.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Commit {
    /// Commit version, `base_version + 1 ..= base_version + len`.
    pub version: u64,
    /// The request committed at that version.
    pub request: LogicalRequest,
}

/// A candidate external world: base version plus an ordered commit sequence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct World {
    /// Version the observed history started from.
    pub base_version: u64,
    /// Commits in version order; versions must be contiguous from `base_version + 1`.
    pub commits: Vec<Commit>,
}

impl World {
    /// An empty world above `base_version`.
    #[must_use]
    pub fn empty(base_version: u64) -> Self {
        Self {
            base_version,
            commits: Vec::new(),
        }
    }

    /// The version the next append would take.
    #[must_use]
    pub fn next_version(&self) -> u64 {
        self.base_version + self.commits.len() as u64 + 1
    }

    /// Version of `request` if it is present.
    #[must_use]
    pub fn version_of(&self, request: &LogicalRequest) -> Option<u64> {
        self.commits
            .iter()
            .find(|c| &c.request == request)
            .map(|c| c.version)
    }

    /// The request held under `key`, if any.
    #[must_use]
    pub fn by_key(&self, key: (&str, &str)) -> Option<&LogicalRequest> {
        self.commits
            .iter()
            .map(|c| &c.request)
            .find(|r| r.key() == key)
    }

    /// Whether versions are contiguous from the base and strictly increasing.
    ///
    /// A world that fails this is not a world, so it is reported rather than compared.
    #[must_use]
    pub fn versions_are_contiguous(&self) -> bool {
        self.commits
            .iter()
            .enumerate()
            .all(|(i, c)| c.version == self.base_version + 1 + i as u64)
    }

    fn append(&self, request: LogicalRequest) -> Self {
        let mut next = self.clone();
        next.commits.push(Commit {
            version: self.next_version(),
            request,
        });
        next
    }
}

/// Why a history could not be replayed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ModelError {
    /// More than [`MAX_ATTEMPTS`] attempts were recorded.
    TooManyAttempts {
        /// Number recorded.
        count: usize,
    },
    /// `Unknown` expansion exceeded [`MAX_WORLDS`].
    TooManyWorlds {
        /// Number reached before aborting.
        count: usize,
    },
    /// A reported outcome cannot be reconciled with the world it claims to describe.
    Contradiction {
        /// Attempt that could not be reconciled.
        attempt_id: String,
        /// Which rule it broke.
        reason: Contradiction,
    },
}

/// The specific inconsistency behind a [`ModelError::Contradiction`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Contradiction {
    /// A durable acknowledgement named a version other than the request's stored one, i.e. the same
    /// logical request was committed twice.
    DuplicateCommit {
        /// Version first stored.
        stored: u64,
        /// Version reported by the retry.
        reported: u64,
    },
    /// A durable acknowledgement named an unexpected version for a new append.
    WrongAppendVersion {
        /// Version the next append would take.
        expected: u64,
        /// Version reported.
        reported: u64,
    },
    /// A durable acknowledgement arrived for a key that already holds a different payload.
    DigestDiffers {
        /// Version holding the first digest.
        stored: u64,
    },
    /// Two different requests were acknowledged at the same version.
    VersionCollision {
        /// The contested version.
        version: u64,
    },
}

/// External attempt history for one store instance.
#[derive(Clone, Debug, Default)]
pub struct AttemptHistory {
    attempts: Vec<Attempt>,
}

impl AttemptHistory {
    /// Record one attempt with its reported outcome.
    ///
    /// # Panics
    ///
    /// Panics on a duplicate `attempt_id`: identities must be distinct, and merging them would make
    /// retry assertions unsound.
    pub fn record(&mut self, attempt_id: &str, request: LogicalRequest, outcome: AttemptOutcome) {
        assert!(
            !self.attempts.iter().any(|a| a.attempt_id == attempt_id),
            "duplicate attempt id {attempt_id:?}: attempt identities must be distinct"
        );
        self.attempts.push(Attempt {
            attempt_id: attempt_id.to_owned(),
            request,
            outcome,
        });
    }

    /// Recorded attempts, in submission order.
    #[must_use]
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// Replay the history into every legal world it permits.
    ///
    /// # Errors
    ///
    /// [`ModelError::TooManyAttempts`], [`ModelError::TooManyWorlds`], or a
    /// [`ModelError::Contradiction`] when the observations cannot describe any single world.
    pub fn legal_worlds(&self, base_version: u64) -> Result<Vec<World>, ModelError> {
        if self.attempts.len() > MAX_ATTEMPTS {
            return Err(ModelError::TooManyAttempts {
                count: self.attempts.len(),
            });
        }

        let mut worlds = vec![World::empty(base_version)];
        for attempt in &self.attempts {
            let mut next: Vec<World> = Vec::new();
            // An acknowledgement is EXISTENTIAL over the candidate worlds: it constrains which of
            // them can still be true, it does not demand that every one of them agrees. A branch that
            // cannot accommodate the ack is pruned. Only when no branch survives does the history
            // become a contradiction - `?` here was a real bug, since it aborted the whole history on
            // the first incompatible branch and so turned existential into universal.
            let mut first_rejection: Option<ModelError> = None;
            for world in &worlds {
                match attempt.outcome {
                    AttemptOutcome::NotCommitted => {
                        // The world is unchanged. This says nothing about later attempts.
                        next.push(world.clone());
                    }
                    AttemptOutcome::Durable { version } => {
                        match apply_durable(world, attempt, version) {
                            Ok(next_world) => next.push(next_world),
                            Err(error) => {
                                if first_rejection.is_none() {
                                    first_rejection = Some(error);
                                }
                            }
                        }
                    }
                    AttemptOutcome::Unknown => {
                        // Either nothing happened, or the request appended - and only when its key is
                        // new. An existing key cannot append a second time.
                        next.push(world.clone());
                        if world.by_key(attempt.request.key()).is_none() {
                            next.push(world.append(attempt.request.clone()));
                        }
                    }
                }
            }
            worlds = dedupe(next);
            if worlds.len() > MAX_WORLDS {
                return Err(ModelError::TooManyWorlds {
                    count: worlds.len(),
                });
            }
            if worlds.is_empty() {
                // Every branch was pruned by this attempt, so the observations cannot describe any
                // single world. Report the first rejection: it names the rule that failed, and it is
                // the one a reader most likely needs, since all branches failed the same rule here.
                return Err(first_rejection.unwrap_or(ModelError::Contradiction {
                    attempt_id: attempt.attempt_id.clone(),
                    reason: Contradiction::WrongAppendVersion {
                        expected: 0,
                        reported: 0,
                    },
                }));
            }
        }

        worlds.sort_by(|a, b| {
            a.commits
                .len()
                .cmp(&b.commits.len())
                .then_with(|| a.commits.cmp(&b.commits))
        });
        Ok(worlds)
    }

    /// Whether `candidate` is one of the worlds this history permits.
    ///
    /// # Errors
    ///
    /// As [`Self::legal_worlds`].
    pub fn accepts(&self, base_version: u64, candidate: &World) -> Result<bool, ModelError> {
        if !candidate.versions_are_contiguous() {
            return Ok(false);
        }
        let worlds = self.legal_worlds(base_version)?;
        Ok(worlds.iter().any(|w| w == candidate))
    }
}

fn apply_durable(world: &World, attempt: &Attempt, version: u64) -> Result<World, ModelError> {
    // Already stored: the acknowledgement must name exactly that version.
    if let Some(stored) = world.version_of(&attempt.request) {
        return if stored == version {
            Ok(world.clone())
        } else {
            Err(ModelError::Contradiction {
                attempt_id: attempt.attempt_id.clone(),
                reason: Contradiction::DuplicateCommit {
                    stored,
                    reported: version,
                },
            })
        };
    }

    // Same key, different payload: cannot append, so a durable ack contradicts the history.
    if let Some(existing) = world.by_key(attempt.request.key()) {
        let stored = world
            .version_of(existing)
            .expect("a request found by key is in the world");
        return Err(ModelError::Contradiction {
            attempt_id: attempt.attempt_id.clone(),
            reason: Contradiction::DigestDiffers { stored },
        });
    }

    // New key: append at the next version, which must be the one reported.
    let expected = world.next_version();
    if expected != version {
        return Err(ModelError::Contradiction {
            attempt_id: attempt.attempt_id.clone(),
            reason: Contradiction::WrongAppendVersion {
                expected,
                reported: version,
            },
        });
    }
    Ok(world.append(attempt.request.clone()))
}

/// Drop worlds that describe the same commit sequence, so convergence does not inflate the bound.
fn dedupe(worlds: Vec<World>) -> Vec<World> {
    let mut out: Vec<World> = Vec::new();
    for world in worlds {
        if !out.iter().any(|existing| {
            existing.base_version == world.base_version && existing.commits == world.commits
        }) {
            out.push(world);
        }
    }
    out
}

/// Detect two distinct requests holding the same version inside one candidate world.
///
/// [`AttemptHistory::accepts`] cannot see this because a candidate with a version collision fails
/// contiguity, but a caller may want the reason rather than a bare `false`.
#[must_use]
pub fn find_version_collision(world: &World) -> Option<Contradiction> {
    let mut seen: BTreeMap<u64, &LogicalRequest> = BTreeMap::new();
    for commit in &world.commits {
        match seen.get(&commit.version) {
            Some(existing) if *existing != &commit.request => {
                return Some(Contradiction::VersionCollision {
                    version: commit.version,
                });
            }
            Some(_) => {}
            None => {
                seen.insert(commit.version, &commit.request);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn req(request_id: &str) -> LogicalRequest {
        LogicalRequest::new("principal-a", request_id, digest(1))
    }

    fn world_of(base: u64, entries: &[(u64, LogicalRequest)]) -> World {
        World {
            base_version: base,
            commits: entries
                .iter()
                .map(|(version, request)| Commit {
                    version: *version,
                    request: request.clone(),
                })
                .collect(),
        }
    }

    // ---- positive controls: the two legal histories review called out ----

    /// POSITIVE: a first attempt that did not commit, then a durable retry. `NotCommitted` is a fact
    /// about the attempt, not a ban on the key, so this history is legal.
    #[test]
    fn not_committed_then_durable_retry_is_legal() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::NotCommitted);
        history.record("a2", req("q"), AttemptOutcome::Durable { version: 1 });

        let worlds = history.legal_worlds(0).expect("legal history");
        assert_eq!(worlds.len(), 1, "exactly one world");
        assert_eq!(worlds[0].commits.len(), 1);
        assert_eq!(worlds[0].commits[0].version, 1);
        assert_eq!(worlds[0].commits[0].request, req("q"));

        assert!(history.accepts(0, &world_of(0, &[(1, req("q"))])).unwrap());
        assert!(
            !history.accepts(0, &World::empty(0)).unwrap(),
            "the commit is mandatory"
        );
    }

    /// POSITIVE: a durable commit, then a retry carrying a DIFFERENT payload that the store correctly
    /// refuses. The refusal is not a violation, and the world still holds only the first digest.
    #[test]
    fn changed_payload_refused_after_durable_is_legal() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });

        let mut changed = req("q");
        changed.payload_digest = digest(9);
        history.record("a2", changed.clone(), AttemptOutcome::NotCommitted);

        let worlds = history.legal_worlds(0).expect("legal history");
        assert_eq!(worlds.len(), 1);
        assert_eq!(
            worlds[0].commits.len(),
            1,
            "the refused attempt appended nothing"
        );
        assert_eq!(worlds[0].commits[0].request.payload_digest, digest(1));

        assert!(history.accepts(0, &world_of(0, &[(1, req("q"))])).unwrap());
        // The refused digest must not appear in any accepted world.
        assert!(
            !history
                .accepts(0, &world_of(0, &[(1, req("q")), (2, changed)]))
                .unwrap()
        );
    }

    // ---- exact retry ----

    /// An exact retry acknowledged at the stored version appends nothing.
    #[test]
    fn exact_durable_retry_does_not_append() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q"), AttemptOutcome::Durable { version: 1 });

        let worlds = history.legal_worlds(0).expect("legal");
        assert_eq!(worlds.len(), 1);
        assert_eq!(worlds[0].commits.len(), 1);
    }

    // ---- negatives ----

    /// A retry acknowledged at a second version is a silent duplicate commit: no legal world exists.
    #[test]
    fn durable_retry_at_a_second_version_is_a_duplicate_commit() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q"), AttemptOutcome::Durable { version: 2 });

        let error = history.legal_worlds(0).expect_err("must contradict");
        assert!(
            matches!(
                error,
                ModelError::Contradiction {
                    reason: Contradiction::DuplicateCommit {
                        stored: 1,
                        reported: 2
                    },
                    ..
                }
            ),
            "got {error:?}"
        );
    }

    /// Two distinct keys acknowledged at the same version collide: versions are unique positions.
    #[test]
    fn two_distinct_keys_acked_at_one_version_is_a_collision() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q1"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q2"), AttemptOutcome::Durable { version: 1 });

        let error = history.legal_worlds(0).expect_err("must contradict");
        assert!(
            matches!(
                error,
                ModelError::Contradiction {
                    reason: Contradiction::WrongAppendVersion {
                        expected: 2,
                        reported: 1
                    },
                    ..
                }
            ),
            "got {error:?}"
        );
    }

    /// A durable ack for a key already holding another digest cannot append.
    #[test]
    fn durable_ack_with_a_changed_digest_contradicts() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });
        let mut changed = req("q");
        changed.payload_digest = digest(7);
        history.record("a2", changed, AttemptOutcome::Durable { version: 2 });

        let error = history.legal_worlds(0).expect_err("must contradict");
        assert!(
            matches!(
                error,
                ModelError::Contradiction {
                    reason: Contradiction::DigestDiffers { stored: 1 },
                    ..
                }
            ),
            "got {error:?}"
        );
    }

    /// An `Unknown` attempt cannot smuggle an unrelated request into a recovered candidate.
    #[test]
    fn unknown_does_not_admit_an_unsubmitted_request() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Unknown);

        // `q` may or may not be present.
        assert!(history.accepts(0, &World::empty(0)).unwrap());
        assert!(history.accepts(0, &world_of(0, &[(1, req("q"))])).unwrap());
        // A request nobody submitted is in no legal world.
        assert!(
            !history
                .accepts(0, &world_of(0, &[(1, req("other"))]))
                .unwrap()
        );
    }

    /// `Unknown` on an existing key can never append, so only the unchanged branch exists.
    #[test]
    fn unknown_on_an_existing_key_cannot_append() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q"), AttemptOutcome::Unknown);

        let worlds = history.legal_worlds(0).expect("legal");
        assert_eq!(worlds.len(), 1, "no branching for an existing key");
        assert_eq!(worlds[0].commits.len(), 1);
    }

    /// A hole in the recovered sequence is not a legal world.
    #[test]
    fn a_missing_middle_commit_is_not_a_legal_world() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q1"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q2"), AttemptOutcome::Durable { version: 2 });

        assert!(
            history
                .accepts(0, &world_of(0, &[(1, req("q1")), (2, req("q2"))]))
                .unwrap()
        );
        assert!(
            !history.accepts(0, &world_of(0, &[(1, req("q1"))])).unwrap(),
            "losing a durable commit is a hole, not a prefix"
        );
        // A world that keeps both but renumbers them is not contiguous.
        assert!(
            !history
                .accepts(0, &world_of(0, &[(1, req("q2")), (2, req("q1"))]))
                .unwrap()
        );
    }

    /// Multiplicity is preserved: the same request cannot occupy two versions in a candidate.
    #[test]
    fn duplicate_commit_inside_a_candidate_is_rejected() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });

        let duplicated = world_of(0, &[(1, req("q")), (2, req("q"))]);
        assert!(!history.accepts(0, &duplicated).unwrap());
    }

    /// A candidate claiming a different base than the observed one is not accepted.
    #[test]
    fn candidate_base_must_match_the_observed_base() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 3 });

        // With base 2, appending q at version 3 is exactly what the ack describes.
        assert!(history.accepts(2, &world_of(2, &[(3, req("q"))])).unwrap());
        // A candidate that keeps base 2 but renumbers the commit is not contiguous.
        assert!(!history.accepts(2, &world_of(2, &[(4, req("q"))])).unwrap());
        // Replaying the same history from base 0 cannot explain an ack at version 3.
        assert!(
            history.legal_worlds(0).is_err(),
            "a base of 0 makes the reported version unreachable"
        );
    }

    /// Same request id under two principals is two logical keys.
    #[test]
    fn same_request_id_under_two_principals_is_two_keys() {
        let a = LogicalRequest::new("principal-a", "r1", digest(1));
        let b = LogicalRequest::new("principal-b", "r1", digest(1));
        assert_ne!(a.key(), b.key());

        let mut history = AttemptHistory::default();
        history.record("a1", a.clone(), AttemptOutcome::Durable { version: 1 });
        history.record("a2", b.clone(), AttemptOutcome::Durable { version: 2 });

        assert!(history.accepts(0, &world_of(0, &[(1, a), (2, b)])).unwrap());
    }

    /// Bounds are reported, never silently truncated.
    #[test]
    fn too_many_attempts_is_an_error_not_a_truncation() {
        let mut history = AttemptHistory::default();
        for i in 0..=MAX_ATTEMPTS {
            history.record(
                &format!("a{i}"),
                LogicalRequest::new("principal-a", &format!("q{i}"), digest(1)),
                AttemptOutcome::Unknown,
            );
        }
        let error = history.legal_worlds(0).expect_err("must refuse");
        assert!(
            matches!(error, ModelError::TooManyAttempts { .. }),
            "got {error:?}"
        );
    }

    // ---- existential branching: an ack must PRUNE incompatible worlds, not abort the history ----

    /// POSITIVE: base 1, `Unknown(a)`, then `Durable(b, version 3)`.
    ///
    /// The `Unknown` branches into "a absent" and "a at 2". In the absent branch `b` appends at 2,
    /// not 3, so that branch is incompatible and must be pruned - but the branch where `a` took
    /// version 2 makes `b` land exactly on 3, which is consistent. The history is therefore legal
    /// and `[a@2, b@3]` must be accepted.
    #[test]
    fn unknown_branch_makes_a_later_ack_consistent_rather_than_aborting() {
        let a = req("a");
        let b = req("b");
        let mut history = AttemptHistory::default();
        history.record("t1", a.clone(), AttemptOutcome::Unknown);
        history.record("t2", b.clone(), AttemptOutcome::Durable { version: 3 });

        assert!(
            history
                .accepts(1, &world_of(1, &[(2, a.clone()), (3, b.clone())]))
                .unwrap(),
            "an Unknown branch that consumes version 2 makes the version-3 ack legal"
        );
        // And the incompatible world is genuinely not accepted.
        assert!(
            !history.accepts(1, &world_of(1, &[(2, b.clone())])).unwrap(),
            "b alone would be version 2, so it cannot satisfy an ack at 3"
        );
    }

    /// POSITIVE: `Unknown(a)`, then `Durable(b, version 2)`, candidate `[b@2]`.
    ///
    /// Without `a`, `b` appends at 2 - consistent. With `a` present, `b` would be 3 - inconsistent,
    /// so that branch is pruned. The empty-branch world is the answer.
    #[test]
    fn ack_prunes_the_branch_where_the_unknown_consumed_the_version() {
        let a = req("a");
        let b = req("b");
        let mut history = AttemptHistory::default();
        history.record("t1", a.clone(), AttemptOutcome::Unknown);
        history.record("t2", b.clone(), AttemptOutcome::Durable { version: 2 });

        // Base 1: with `a` absent, `b` appends at version 2 exactly as acknowledged.
        assert!(
            history.accepts(1, &world_of(1, &[(2, b.clone())])).unwrap(),
            "with a absent, b lands on version 2 as acknowledged"
        );
        // With `a` present it would occupy 2 and push `b` to 3, so that branch is pruned.
        assert!(
            !history
                .accepts(1, &world_of(1, &[(2, a.clone()), (3, b.clone())]))
                .unwrap(),
            "with a present, b would be version 3 and the ack at 2 is impossible"
        );
    }

    /// NEGATIVE: when NO branch can satisfy the observations, the history is contradictory. Pruning
    /// must not degrade into "accept anything".
    #[test]
    fn all_branches_incompatible_is_still_an_error() {
        let a = req("a");
        let b = req("b");
        let mut history = AttemptHistory::default();
        history.record("t1", a.clone(), AttemptOutcome::Unknown);
        // No branch puts b at version 5: without a it is 2, with a it is 3.
        history.record("t2", b.clone(), AttemptOutcome::Durable { version: 5 });

        let error = history.legal_worlds(1).expect_err("no branch fits");
        assert!(
            matches!(
                error,
                ModelError::Contradiction {
                    reason: Contradiction::WrongAppendVersion { .. },
                    ..
                }
            ),
            "got {error:?}"
        );
    }

    // ---- the four root probe counterexamples, as named regressions ----
    //
    // Root ran these against the v1 set-based model and recorded PROBE_EXIT=1 with all four verdicts
    // opposite the expected ones (p1-4-root-review/attempt-oracle-v1/probe.log). They are reproduced
    // here so the corrected model is tied to those exact observations rather than to my own taste.

    /// PROBE 1: legal refusal then successful retry. Expected accept=true; v1 said false.
    #[test]
    fn probe_1_refusal_then_retry_is_accepted() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::NotCommitted);
        history.record("a2", req("q"), AttemptOutcome::Durable { version: 1 });
        assert!(
            history.accepts(0, &world_of(0, &[(1, req("q"))])).unwrap(),
            "a refused first attempt followed by a durable retry must be legal"
        );
    }

    /// PROBE 2: legal changed-payload rejection. Expected accept=true; v1 said false.
    #[test]
    fn probe_2_changed_payload_rejection_is_accepted() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Durable { version: 1 });
        let mut changed = req("q");
        changed.payload_digest = digest(2);
        history.record("a2", changed, AttemptOutcome::NotCommitted);
        assert!(
            history.accepts(0, &world_of(0, &[(1, req("q"))])).unwrap(),
            "a correctly refused changed-payload retry must not invalidate the history"
        );
    }

    /// PROBE 3: unsubmitted request in the recovered set. Expected accept=false; v1 said true.
    #[test]
    fn probe_3_unsubmitted_recovered_request_is_rejected() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::Unknown);
        let mut stranger = req("q");
        stranger.request_id = "never-submitted".to_owned();
        assert!(
            !history.accepts(0, &world_of(0, &[(1, stranger)])).unwrap(),
            "a request nobody submitted must not be recoverable"
        );
    }

    /// PROBE 4: two requests acknowledged at one version. Expected accept=false; v1 said true.
    #[test]
    fn probe_4_version_collision_is_rejected() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q1"), AttemptOutcome::Durable { version: 1 });
        history.record("a2", req("q2"), AttemptOutcome::Durable { version: 1 });
        assert!(
            history.legal_worlds(0).is_err(),
            "two distinct requests cannot occupy one version"
        );
    }

    /// Plain `NotCommitted` with no later success leaves the world empty.
    #[test]
    fn not_committed_alone_leaves_the_world_unchanged() {
        let mut history = AttemptHistory::default();
        history.record("a1", req("q"), AttemptOutcome::NotCommitted);

        let worlds = history.legal_worlds(0).expect("legal");
        assert_eq!(worlds.len(), 1);
        assert!(worlds[0].commits.is_empty());
    }
}
