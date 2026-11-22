//! Oracle regression tests.
//!
//! These pin the two properties that distinguish a discriminating oracle from a decorative one:
//! prefix semantics, and the durable-versus-observed split. They also keep a standing negative
//! control proving that the weaker "arbitrary subset" oracle would accept a state a chain cannot
//! represent — if that control ever stops reproducing the hole, this module has lost its teeth.

use std::collections::BTreeSet;

use super::{Ack, AllowedStates, Observation, Recovered, Violation, check};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The submission order fixes the only legal states: prefixes, not subsets.
#[test]
fn legal_states_are_prefixes_of_submission_order() {
    let mut obs = Observation::default();
    for r in ["g", "a", "b", "c"] {
        obs.submit(r);
    }
    obs.ack_durable("g");
    obs.ack_durable("a");

    let allowed = AllowedStates::from_observation(&obs);
    assert!(allowed.allows(&set(&["g", "a"])));
    assert!(allowed.allows(&set(&["g", "a", "b"])));
    assert!(allowed.allows(&set(&["g", "a", "b", "c"])));

    // A hole is not a prefix: `b` missing while `c` present cannot come from a chain.
    assert!(!allowed.allows(&set(&["g", "a", "c"])));
    // And an in-flight request before the acknowledged one cannot be skipped either.
    assert!(!allowed.allows(&set(&["g", "b", "c"])));
}

/// Positive control: a correct recovery yields no violations.
#[test]
fn correct_recovery_is_clean() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.submit("a");
    obs.ack_durable("g");
    obs.ack_durable("a");
    obs.publish_durable(2);

    let recovered = Recovered {
        requests: set(&["g", "a"]),
        version: Some(2),
        failed: false,
    };
    assert_eq!(check(&obs, &recovered), Vec::new());
}

/// A durable acknowledgement that vanishes is data loss, and only that property fires.
#[test]
fn lost_acknowledgement_reports_data_loss() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.submit("a");
    obs.ack_durable("g");
    obs.ack_durable("a");

    let recovered = Recovered {
        requests: set(&["g"]),
        version: Some(1),
        failed: false,
    };
    let violations = check(&obs, &recovered);
    assert!(
        violations.iter().any(|v| v.property() == "P-PREFIX"),
        "dropping an acknowledged tail is also not a legal prefix: {violations:?}"
    );
}

/// Publishing a version as durable and then recovering below it is a visibility regression.
#[test]
fn published_durable_version_must_not_regress() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.ack_durable("g");
    obs.publish_durable(5);

    let recovered = Recovered {
        requests: set(&["g"]),
        version: Some(4),
        failed: false,
    };
    let violations = check(&obs, &recovered);
    assert!(
        violations
            .iter()
            .any(|v| matches!(v, Violation::PublishedDurableLost(5))),
        "recovering v4 after publishing v5 must be flagged: {violations:?}"
    );
}

/// A merely observed (unverified) version imposes no recovery duty: it may vanish.
#[test]
fn unverified_observation_may_vanish_without_violation() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.ack_durable("g");
    obs.observe_unverified(7);

    let recovered = Recovered {
        requests: set(&["g"]),
        version: Some(1),
        failed: false,
    };
    assert_eq!(
        check(&obs, &recovered),
        Vec::new(),
        "an unverified observation must not create a durability obligation"
    );
}

/// Acknowledging a request nobody submitted is a harness bug and must be loud.
#[test]
#[should_panic(expected = "was never submitted")]
fn ack_for_unknown_request_panics() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.ack_durable("nope");
}

/// Hard failure is only legal when nothing was durably acknowledged.
#[test]
fn corruption_after_ack_is_data_loss() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.ack_durable("g");

    let recovered = Recovered {
        requests: BTreeSet::new(),
        version: None,
        failed: true,
    };
    let violations = check(&obs, &recovered);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].property(), "P-DATA-LOSS");
}

/// Negative control: the weaker "arbitrary subset of unacknowledged" oracle accepts a hole.
///
/// This test exists so the stronger rule cannot be quietly relaxed later. If it ever fails, the
/// two oracles have become equivalent and the prefix rule is no longer being tested.
#[test]
fn arbitrary_subset_oracle_would_accept_an_impossible_hole() {
    let order = ["g", "a", "b", "c"];
    let acked: BTreeSet<&str> = ["g", "a"].into_iter().collect();
    let inflight = ["b", "c"];

    // Weak oracle: acked ∪ any subset of in-flight.
    let mut weak: Vec<BTreeSet<String>> = Vec::new();
    for mask in 0u32..(1 << inflight.len()) {
        let mut s: BTreeSet<String> = acked.iter().map(|x| (*x).to_owned()).collect();
        for (i, r) in inflight.iter().enumerate() {
            if mask & (1 << i) != 0 {
                s.insert((*r).to_owned());
            }
        }
        weak.push(s);
    }

    let hole = set(&["g", "a", "c"]);
    assert!(
        weak.contains(&hole),
        "the weak oracle is expected to accept a hole; otherwise this control proves nothing"
    );

    let mut obs = Observation::default();
    for r in order {
        obs.submit(r);
    }
    obs.ack_durable("g");
    obs.ack_durable("a");
    assert!(
        !AllowedStates::from_observation(&obs).allows(&hole),
        "the prefix oracle must reject the same hole the weak oracle accepted"
    );
}

/// The mandatory prefix is exactly one past the last durable acknowledgement.
#[test]
fn mandatory_prefix_tracks_last_acknowledgement() {
    let mut obs = Observation::default();
    for r in ["g", "a", "b"] {
        obs.submit(r);
    }
    assert_eq!(obs.mandatory_prefix(), 0);
    obs.ack_durable("a");
    assert_eq!(obs.mandatory_prefix(), 2);
    assert_eq!(obs.acks[2], Ack::InFlight);
}

/// A published durable version must be reported even when recovery itself failed.
///
/// The earlier `check` returned early on `failed`, so publishing v2 and then failing recovery
/// looked clean. The publication guarantee does not lapse because the failure is hard.
#[test]
fn published_durable_version_survives_the_failure_path() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.publish_durable(2);
    // No acknowledgement at all: only the publication is at stake here.

    let recovered = Recovered {
        requests: BTreeSet::new(),
        version: None,
        failed: true,
    };
    let violations = check(&obs, &recovered);
    assert_eq!(
        violations
            .iter()
            .filter(|v| v.property() == "P-DUR-VIS")
            .count(),
        1,
        "a durable publication must be reported as lost on the failure path: {violations:?}"
    );
    assert!(
        !violations.iter().any(|v| v.property() == "P-DATA-LOSS"),
        "nothing was acknowledged, so there is no durability loss to report"
    );
}

/// A failed recovery with both an acknowledgement and a publication reports both properties.
#[test]
fn failure_path_keeps_the_two_properties_distinct() {
    let mut obs = Observation::default();
    obs.submit("g");
    obs.ack_durable("g");
    obs.publish_durable(3);

    let recovered = Recovered {
        requests: BTreeSet::new(),
        version: None,
        failed: true,
    };
    let violations = check(&obs, &recovered);
    let props: BTreeSet<&str> = violations.iter().map(Violation::property).collect();
    assert!(props.contains("P-DATA-LOSS"), "{violations:?}");
    assert!(props.contains("P-DUR-VIS"), "{violations:?}");
}

/// Submitting the same request id twice must panic rather than quietly model a retry as a commit.
///
/// The prefix oracle covers *unique candidate transactions*. Without this guard it would appear to
/// cover idempotence while actually treating a retry as a new prefix element.
#[test]
#[should_panic(expected = "duplicate request id")]
fn duplicate_request_ids_are_refused() {
    let mut obs = Observation::default();
    obs.submit("r1");
    obs.submit("r1");
}

/// Distinct attempt ids for the same logical operation are accepted, which is the supported way to
/// model a retry before P1-4 gives the oracle attempt-level identity.
#[test]
fn distinct_attempt_ids_are_accepted() {
    let mut obs = Observation::default();
    obs.submit("r1:attempt1");
    obs.submit("r1:attempt2");
    assert_eq!(obs.order().len(), 2);
}
