//! Legal-recovered-state oracle derived from **externally observed** facts only.
//!
//! The oracle must never call production replay, fold or recovery to decide what "should" have
//! happened: that would make an implementation its own judge. Instead the test observes, from the
//! outside, which requests were acknowledged and which versions were published, and derives the
//! set of request sets a correct store is allowed to recover.
//!
//! Two properties of this module are load-bearing and were both established by earlier mistakes:
//!
//! * legal states are **whole-transaction prefixes** of the submission order, not arbitrary
//!   subsets of unacknowledged attempts. An arbitrary-subset oracle accepts a log hole that no
//!   chain can represent, so it cannot detect a lost middle transaction;
//! * acknowledgements and mere **observations** are different facts. A version published as
//!   `Durable` may never disappear; a root merely *observed* while the commit result was unknown
//!   may legally vanish, and must never be the source of an acknowledgement.

use std::collections::BTreeSet;

/// One externally observed outcome of a submission.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Ack {
    /// The caller was told the write is durable. Must never disappear.
    Durable,
    /// The attempt was submitted but no durable acknowledgement was received.
    ///
    /// It may have committed; recovery may legitimately contain it or not.
    InFlight,
}

/// Externally observed history of one store instance.
///
/// Built by the test harness from the outside: submission order, which requests received a
/// durable acknowledgement, and which versions were published to readers.
#[derive(Clone, Debug, Default)]
pub struct Observation {
    /// Requests in the order they were submitted. Index is the submission position.
    submitted: Vec<String>,
    /// Per submitted request, what the caller was told.
    acks: Vec<Ack>,
    /// Versions published to readers with a `Durable` label.
    durable_versions: BTreeSet<u64>,
    /// Versions merely observed while the commit result was unknown. No recovery duty.
    observed_only: BTreeSet<u64>,
}

impl Observation {
    /// Record a submission that has not yet been acknowledged.
    ///
    /// # Panics
    ///
    /// Panics when `request` was already submitted. The prefix oracle models *unique candidate
    /// transactions*, not caller attempts: a retry of the same request id and a rejection are not
    /// new prefix elements. Allowing a duplicate would let the oracle appear to cover idempotence
    /// while actually treating a retry as an independent commit, so the limitation is enforced
    /// rather than documented and hoped for.
    pub fn submit(&mut self, request: impl Into<String>) {
        let request = request.into();
        assert!(
            !self.submitted.contains(&request),
            "duplicate request id {request:?}: this oracle models unique candidate transactions, \
             not attempts. Use a distinct attempt id for a retry, or extend the oracle to model \
             attempts explicitly."
        );
        self.submitted.push(request);
        self.acks.push(Ack::InFlight);
    }

    /// Record that the caller received a durable acknowledgement for `request`.
    ///
    /// # Panics
    ///
    /// Panics when `request` was never submitted: an acknowledgement for an unknown request is a
    /// harness bug, not a store behaviour, and silently ignoring it would weaken every later
    /// assertion.
    pub fn ack_durable(&mut self, request: &str) {
        let idx = self
            .submitted
            .iter()
            .position(|r| r == request)
            .unwrap_or_else(|| panic!("acknowledged request {request:?} was never submitted"));
        self.acks[idx] = Ack::Durable;
    }

    /// Record that a version was published to readers as `Durable`.
    pub fn publish_durable(&mut self, version: u64) {
        self.durable_versions.insert(version);
    }

    /// Record that a version was merely observed while the result was unknown.
    ///
    /// Such an observation imposes **no** recovery duty: it may vanish after a crash, and it can
    /// never justify a durable acknowledgement.
    pub fn observe_unverified(&mut self, version: u64) {
        if !self.durable_versions.contains(&version) {
            self.observed_only.insert(version);
        }
    }

    /// Submission order, for diagnostics.
    #[must_use]
    pub fn order(&self) -> &[String] {
        &self.submitted
    }

    /// Highest index (inclusive) that must be present in every legal recovered state.
    ///
    /// Because commits form a single chain, everything up to and including the last durable
    /// acknowledgement must be present; a later request may be present or absent.
    #[must_use]
    pub fn mandatory_prefix(&self) -> usize {
        let last_durable = self.acks.iter().rposition(|a| *a == Ack::Durable);
        last_durable.map_or(0, |i| i + 1)
    }
}

/// The set of request sets a correct store may recover, as exact prefixes of submission order.
#[derive(Clone, Debug)]
pub struct AllowedStates {
    prefixes: Vec<BTreeSet<String>>,
}

impl AllowedStates {
    /// Derive the allowed recovered request sets from `obs`.
    ///
    /// Every legal state is `order[..k]` for some `k` at least as large as the mandatory prefix.
    /// Requests are unique in the modelled history; the harness enforces that when submitting.
    #[must_use]
    pub fn from_observation(obs: &Observation) -> Self {
        let order = obs.order();
        let mandatory = obs.mandatory_prefix();
        let prefixes = (mandatory..=order.len())
            .map(|k| order[..k].iter().cloned().collect::<BTreeSet<_>>())
            .collect();
        Self { prefixes }
    }

    /// Whether `recovered` is one of the legal states.
    #[must_use]
    pub fn allows(&self, recovered: &BTreeSet<String>) -> bool {
        self.prefixes.iter().any(|p| p == recovered)
    }

    /// How many legal states exist. Zero would mean an impossible observation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prefixes.len()
    }

    /// Whether no legal state exists (an observation that cannot come from a correct store).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }
}

/// A violation found by [`check`], reported with the property it broke.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Violation {
    /// A durably acknowledged request is missing from the recovered state.
    DurabilityLost(String),
    /// The recovered request set is not a legal prefix of the submission order.
    NotAPrefix(BTreeSet<String>),
    /// A version published as `Durable` is missing after recovery.
    PublishedDurableLost(u64),
    /// The recovered root version is lower than a version already published as durable.
    VersionRegressed {
        /// Version the readers had already been told about.
        published: u64,
        /// Version actually recovered.
        recovered: u64,
    },
}

impl Violation {
    /// A short stable name, for assertion messages.
    #[must_use]
    pub fn property(&self) -> &'static str {
        match self {
            Self::DurabilityLost(_) => "P-DATA-LOSS",
            Self::NotAPrefix(_) => "P-PREFIX",
            Self::PublishedDurableLost(_) => "P-DUR-VIS",
            Self::VersionRegressed { .. } => "P-ROOT",
        }
    }
}

/// What a recovery attempt actually produced, as observed from the outside.
#[derive(Clone, Debug)]
pub struct Recovered {
    /// Requests whose transactions are present in the recovered committed chain.
    pub requests: BTreeSet<String>,
    /// The root version the store recovered, if any.
    pub version: Option<u64>,
    /// Whether recovery reported a hard failure (corruption) rather than a clean state.
    pub failed: bool,
}

/// Check one recovery outcome against the observation.
///
/// Empty return means no violation. The four properties are kept distinct on purpose: a
/// durability loss and a visibility regression have different operator responses, and collapsing
/// them into "something is wrong" would hide which invariant the store actually broke.
#[must_use]
pub fn check(obs: &Observation, recovered: &Recovered) -> Vec<Violation> {
    let mut out = Vec::new();

    if recovered.failed {
        // A hard failure may only be legal when nothing was durably acknowledged AND nothing was
        // published as durable. The earlier version returned here, so a store that published v2
        // and then failed recovery looked clean: the publication guarantee has to survive the
        // failure path too, which is exactly the case a corruption test exercises.
        for (idx, ack) in obs.acks.iter().enumerate() {
            if *ack == Ack::Durable {
                out.push(Violation::DurabilityLost(obs.submitted[idx].clone()));
            }
        }
        for &published in &obs.durable_versions {
            out.push(Violation::PublishedDurableLost(published));
        }
        return out;
    }

    let allowed = AllowedStates::from_observation(obs);
    if !allowed.allows(&recovered.requests) {
        out.push(Violation::NotAPrefix(recovered.requests.clone()));
    }

    let recovered_version = recovered.version.unwrap_or(0);
    for &published in &obs.durable_versions {
        if recovered_version < published {
            out.push(Violation::PublishedDurableLost(published));
        }
    }

    out
}

pub mod attempts;

#[cfg(test)]
mod tests;
