//! Canonical rules bytes: what the engine is allowed to derive, as data.
//!
//! A rules *digest* is an opaque comparison value. It cannot tell a reader whether
//! `instantiates` is cardinality-limited, so the profile and its full template set must
//! travel as bytes and the digest must be reproducible from them. That is why this
//! module exists rather than a constant table of hashes.
//!
//! Two profiles, one layout. A profile that differs only by name would be
//! indistinguishable after hashing, so the encoding carries every field that changes
//! behaviour: per-relation transitivity/symmetry/acyclicity/layer rule, whether
//! definition edges imply `depends-on`, and the twelve profile-global validation rules
//! with their parameters and enforcement.
//!
//! `engine_version` is deliberately absent: the build that computes a result is not a
//! property of the rules, and mixing them would make every upgrade look like a
//! migration.

use sage_db_types::relation::{RelationId, RuleProfile};

use crate::wire::{WriteError, Writer};
use sage_db_types::ledger::SharedLedger;

/// Layout version of the rules encoding itself.
///
/// Distinct from the format major and from the profile id: this is the version of the
/// *bytes*, so a future change can be detected before it is misinterpreted.
pub const RULES_FORMAT_VERSION: u16 = 1;

/// Number of relations the engine can evaluate.
const RELATION_COUNT: u16 = 7;

/// Number of profile-global validation rules.
const VALIDATION_COUNT: u16 = 12;

/// A validation rule's enforcement state.
///
/// `Off` is encoded explicitly rather than by omitting the rule, so "this profile
/// turns the rule off" and "this reader does not know the rule" stay distinguishable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Enforcement {
    /// The rule is not applied under this profile.
    Off,
    /// The rule is applied.
    On,
}

impl Enforcement {
    const fn as_u16(self) -> u16 {
        match self {
            Self::Off => 0,
            Self::On => 1,
        }
    }
}

/// The profile-global rules, in their fixed numeric order.
///
/// The order and the ids are part of the encoding; renaming a rule is free, changing
/// its id is a format change.
pub const VALIDATION_IDS: [u16; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

/// Everything that makes one profile behave differently from another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CanonicalRules {
    /// Which fixed template set this is.
    pub profile: RuleProfile,
}

impl CanonicalRules {
    /// Rules for `profile`.
    #[must_use]
    pub const fn new(profile: RuleProfile) -> Self {
        Self { profile }
    }

    /// Encode the canonical bytes.
    ///
    /// # Errors
    ///
    /// [`WriteError`] for an unrepresentable length, a budget refusal, or an
    /// allocation failure.
    pub fn encode(&self, ledger: &mut dyn SharedLedger) -> Result<Vec<u8>, WriteError> {
        // The size is fixed by construction, so admit it EXACTLY once here and mark the
        // writer prepaid; nothing below charges again. The buffer is reserved with the
        // same fallible path the writer uses, so an allocator refusal is typed here too
        // rather than an abort inside `with_capacity`.
        // One admission, one allocation path. The size is charged here and handed to
        // the writer as prepaid CREDIT; the writer then charges nothing per append and
        // reserves through the same fallible, admit-before-reserve path every other
        // caller uses. Reserving here as well would double-reserve and desynchronize
        // the writer's admitted-capacity accounting from the real buffer.
        let len = encoded_len(self.profile);
        ledger.try_charge(len as u64)?;
        ledger.checkpoint()?;
        let mut buf = Vec::new();
        let mut w = Writer::prepaid(&mut buf, ledger, len);

        w.u16(RULES_FORMAT_VERSION)?;
        w.u16(profile_id(self.profile))?;
        w.str(self.profile.as_str())?;

        w.u16(RELATION_COUNT)?;
        for relation in RelationId::ALL {
            let spec = relation.spec(self.profile);
            w.u16(relation.get() as u16)?;
            w.str(relation.as_str())?;
            w.u8(u8::from(spec.is_definition))?;
            w.u8(u8::from(spec.transitive))?;
            w.u8(u8::from(spec.symmetric))?;
            w.u8(u8::from(spec.acyclic))?;
            w.u16(layer_rule_id(spec.layer_rule))?;
            w.u8(u8::from(spec.implies_dependency))?;
        }

        w.u16(VALIDATION_COUNT)?;
        for (id, (enforcement, params)) in validation_rules(self.profile).into_iter().enumerate() {
            w.u16(VALIDATION_IDS[id])?;
            w.u16(enforcement.as_u16())?;
            w.blob(params)?;
        }

        Ok(buf)
    }

    /// The undomained content hash of the canonical bytes, which is also the blob name.
    ///
    /// # Errors
    ///
    /// As [`Self::encode`].
    pub fn blob_ref(&self, ledger: &mut dyn SharedLedger) -> Result<[u8; 32], WriteError> {
        let bytes = self.encode(ledger)?;
        Ok(crate::digest::raw_sha256(&bytes))
    }

    /// The domain-separated digest that travels in a manifest and in a query envelope.
    ///
    /// # Errors
    ///
    /// As [`Self::encode`].
    pub fn digest(&self, ledger: &mut dyn SharedLedger) -> Result<[u8; 32], WriteError> {
        let bytes = self.encode(ledger)?;
        Ok(crate::digest::digest(crate::digest::Domain::Rules, &bytes))
    }
}

fn profile_id(profile: RuleProfile) -> u16 {
    match profile {
        RuleProfile::LegacyV1 => 0,
        RuleProfile::FormalV1 => 1,
    }
}

fn layer_rule_id(rule: sage_db_types::relation::LayerRule) -> u16 {
    use sage_db_types::relation::LayerRule;
    match rule {
        LayerRule::Unrestricted => 0,
        LayerRule::TargetAtMostSource => 1,
        LayerRule::TargetBelowSource => 2,
    }
}

/// The twelve validation rules with their parameters, in id order.
///
/// Parameters are `&'static [u8]` literals: no temporary `Vec` is built while admitting
/// a charge, so nothing is allocated before the budget is consulted. The layouts are
/// fixed by the accepted table (storage-digests §5 row 7 is `u8 | u16LE | u8`, for
/// example), and keeping them as literals makes them reviewable at a glance.
fn validation_rules(profile: RuleProfile) -> [(Enforcement, &'static [u8]); 12] {
    let formal = matches!(profile, RuleProfile::FormalV1);
    let on = Enforcement::On;
    let off = Enforcement::Off;

    // Parameter blocks, named so the layout is not inferred from a byte list.
    const R1_RELATIONS: &[u8] = &[0, 1, 3]; // is-a, part-of, depends-on
    const R1: &[u8] = &[3, 0, 0, 0, 0, 0, 1, 0, 3, 0];
    const R2_RELATIONS: &[u8] = &[5, 6]; // contradicts, analogous-to
    const R2: &[u8] = &[2, 0, 0, 0, 5, 0, 6, 0];
    const R3_RELATIONS: &[u8] = &[0, 1, 2]; // definition relations
    const R3: &[u8] = &[3, 0, 0, 0, 0, 0, 1, 0, 2, 0];
    const V6_RELATIONS: &[u8] = &[0, 1, 2, 3];
    const V6: &[u8] = &[4, 0, 0, 0, 0, 0, 1, 0, 2, 0, 3, 0];
    const V4: &[u8] = &[1, 0, 0, 0, 2, 0]; // max_targets u32 = 1, relation u16 = 2
    const V5: &[u8] = &[3, 2, 0, 1]; // layer u8 = 3, relation u16LE = 2, min u8 = 1
    const ANNOTATION: &[u8] = &[3, 0, 0, 0, 0, 0, 1, 0, 2, 0];
    const VISIT_ORDER: &[u8] = &[
        0x22, 0, 0, 0, b'r', b'e', b'l', b'a', b't', b'i', b'o', b'n', b'_', b'i', b'd', b',',
        b't', b'a', b'r', b'g', b'e', b't', b'_', b'i', b'd', b',', b'a', b's', b's', b'e', b'r',
        b't', b'i', b'o', b'n', b'_', b'i', b'd',
    ];

    let _ = (R1_RELATIONS, R2_RELATIONS, R3_RELATIONS, V6_RELATIONS);

    [
        (on, R1),
        (on, R2),
        (if formal { on } else { off }, R3),
        (on, &[]),
        (on, &[]),
        (if formal { on } else { off }, V4),
        (if formal { on } else { off }, V5),
        (on, V6),
        (if formal { on } else { off }, V6),
        (on, ANNOTATION),
        (on, VISIT_ORDER),
        (on, &[]),
    ]
}

/// The exact encoded size, computed without allocating.
fn encoded_len(profile: RuleProfile) -> usize {
    let name = profile.as_str().len() + 4;
    let relations = RelationId::ALL
        .iter()
        .map(|r| 2 + 4 + r.as_str().len() + 4 + 2 + 1)
        .sum::<usize>();
    let rules = validation_rules(profile)
        .iter()
        .map(|(_, params)| 2 + 2 + 4 + params.len())
        .sum::<usize>();
    2 + 2 + name + 2 + relations + 2 + rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::{BudgetError, BudgetUnit};
    use sage_db_types::query::ControlStop;

    struct Open;

    impl SharedLedger for Open {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    fn bytes(profile: RuleProfile) -> Vec<u8> {
        let mut led = Open;
        CanonicalRules::new(profile)
            .encode(&mut led)
            .expect("encode")
    }

    /// Values independently produced by the accepted D10 python generator, which was
    /// written from the specification and not from this code. Reproducing them is the
    /// only evidence that this encoder implements the contract rather than itself.
    #[test]
    fn both_profiles_reproduce_the_accepted_vectors() {
        let legacy = bytes(RuleProfile::LegacyV1);
        let formal = bytes(RuleProfile::FormalV1);

        assert_eq!(legacy.len(), 381, "legacy length");
        assert_eq!(formal.len(), 381, "formal length");

        assert_eq!(
            hex(&crate::digest::raw_sha256(&legacy)),
            "b6f92f3c3806be34b9343cae9524e8b7796395942e2d7d21e99ad1b960a2ec16",
            "legacy rules_ref"
        );
        assert_eq!(
            hex(&crate::digest::digest(
                crate::digest::Domain::Rules,
                &legacy
            )),
            "b4f0366e9ba49f5984d0f10e142c8ce6854d5b321ebe89065878156b88aef741",
            "legacy rules_digest"
        );

        assert_eq!(
            hex(&crate::digest::raw_sha256(&formal)),
            "04342325f9c6733f4dc100ee8d310b1d862f82f894b23589c303d87f25654f88",
            "formal rules_ref"
        );
        assert_eq!(
            hex(&crate::digest::digest(
                crate::digest::Domain::Rules,
                &formal
            )),
            "19fe486994acf70283c1055ffa8a27057fe7b764146b1c9c7ac0b3a3f3176ded",
            "formal rules_digest"
        );
    }

    #[test]
    fn the_two_profiles_are_not_a_name_change() {
        let legacy = bytes(RuleProfile::LegacyV1);
        let formal = bytes(RuleProfile::FormalV1);
        assert_eq!(legacy.len(), formal.len(), "same shape");
        let differing = legacy.iter().zip(&formal).filter(|(a, b)| a != b).count();
        assert!(
            differing > 10,
            "profiles must differ in behaviour, not just the name byte; got {differing}"
        );
    }

    struct Metered {
        remaining: u64,
        spent: u64,
        cancelled: bool,
    }

    impl SharedLedger for Metered {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.remaining {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.remaining));
            }
            self.remaining -= bytes;
            self.spent += bytes;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            if self.cancelled {
                Err(ControlStop::Cancelled)
            } else {
                Ok(())
            }
        }
    }

    /// The exact size is charged exactly once: no pre-charge plus per-field charge.
    #[test]
    fn the_exact_budget_is_charged_once() {
        let mut led = Metered {
            remaining: 381,
            spent: 0,
            cancelled: false,
        };
        let bytes = CanonicalRules::new(RuleProfile::FormalV1)
            .encode(&mut led)
            .expect("exact budget must be sufficient");
        assert_eq!(bytes.len(), 381);
        assert_eq!(
            led.spent, 381,
            "charged {} instead of exactly 381",
            led.spent
        );
        assert_eq!(led.remaining, 0);
    }

    /// One byte short is refused, and the refusal is a budget arm.
    #[test]
    fn one_byte_short_is_a_budget_refusal() {
        let mut led = Metered {
            remaining: 380,
            spent: 0,
            cancelled: false,
        };
        let err = CanonicalRules::new(RuleProfile::FormalV1)
            .encode(&mut led)
            .expect_err("380 cannot cover 381 bytes");
        assert!(
            matches!(err, WriteError::Budget(_)),
            "a short budget must not read as a data verdict: {err:?}"
        );
        assert_eq!(led.spent, 0, "a refused admission charges nothing");
    }

    /// A cancelled attempt must not produce rules bytes at all.
    #[test]
    fn a_cancelled_attempt_encodes_nothing() {
        let mut led = Metered {
            remaining: 1000,
            spent: 0,
            cancelled: true,
        };
        let err = CanonicalRules::new(RuleProfile::LegacyV1)
            .encode(&mut led)
            .expect_err("cancelled ledger must stop the encode");
        assert_eq!(err, WriteError::Stopped(ControlStop::Cancelled));
    }

    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(bytes(RuleProfile::FormalV1), bytes(RuleProfile::FormalV1));
    }

    #[test]
    fn engine_version_is_absent_from_the_bytes() {
        // A build identifier must never influence rules identity; if it ever appears,
        // every upgrade would look like a rule migration.
        let found = bytes(RuleProfile::FormalV1)
            .windows(6)
            .any(|w| w == b"engine");
        assert!(!found, "rules bytes must not mention the engine version");
    }

    fn hex(bytes: &[u8; 32]) -> String {
        use core::fmt::Write as _;
        let mut s = String::with_capacity(64);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}
