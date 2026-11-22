//! Validate the closed legacy checkpoint document shape without retaining JSON owners.

use core::{cell::RefCell, fmt, marker::PhantomData};

use sage_db_types::ledger::SharedLedger;
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};
use serde_json::admission::AdmissionContext;

use super::{Decoded, IngressError, LedgerAdmission, SharedPolicy};

struct Text;

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor;
        impl Visitor<'_> for TextVisitor {
            type Value = Text;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E: Error>(self, _: &str) -> Result<Text, E> {
                Ok(Text)
            }
        }
        deserializer.deserialize_str(TextVisitor)
    }
}

fn present_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Text>, D::Error> {
    Text::deserialize(deserializer).map(Some)
}

struct List<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ListVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for ListVisitor<T> {
            type Value = List<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<List<T>, A::Error> {
                while sequence.next_element::<T>()?.is_some() {}
                Ok(List(PhantomData))
            }
        }
        deserializer.deserialize_seq(ListVisitor(PhantomData))
    }
}

struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Object<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

struct Priority;

impl<'de> Deserialize<'de> for Priority {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PriorityVisitor;
        impl Visitor<'_> for PriorityVisitor {
            type Value = Priority;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a registered priority")
            }

            fn visit_str<E: Error>(self, value: &str) -> Result<Priority, E> {
                if matches!(value, "high" | "medium" | "low") {
                    Ok(Priority)
                } else {
                    Err(E::custom("invalid priority"))
                }
            }
        }
        deserializer.deserialize_str(PriorityVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontier {
    #[serde(rename = "id")]
    _id: Text,
    #[serde(rename = "reason")]
    _reason: Text,
    #[serde(rename = "priority")]
    _priority: Priority,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document<'a> {
    concept: &'a str,
    session: u32,
    #[serde(rename = "date")]
    _date: Text,
    #[serde(rename = "established")]
    _established: List<Text>,
    #[serde(rename = "exploredEdges")]
    _explored: List<Text>,
    #[serde(rename = "frontier")]
    _frontier: List<Object<Frontier>>,
    #[serde(default, deserialize_with = "present_text", rename = "nextEntryPoint")]
    _next: Option<Text>,
}

struct CheckpointSeed<'a> {
    slug: &'a str,
    session: u32,
}

impl<'de> DeserializeSeed<'de> for CheckpointSeed<'_> {
    type Value = Decoded<()>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let Object(document) = Object::<Document>::deserialize(deserializer)?;
        if document.concept != self.slug || document.session != self.session {
            return Err(D::Error::custom("checkpoint identity mismatch"));
        }
        Ok(Decoded::new((), 0))
    }
}

/// Check that a committed document is representable on the old checkpoint route.
///
/// The caller passes the canonical concept and session from the same verified
/// record. The check retains no JSON buffers or strings.
///
/// # Errors
/// Refuses excess input, incompatible content, or the caller's ledger refusal.
pub fn validate_legacy_checkpoint(
    content: &[u8],
    slug: &str,
    session: u32,
    body_ceiling: u64,
    ledger: &mut dyn SharedLedger,
) -> Result<(), IngressError> {
    let policy = RefCell::new(LedgerAdmission::new(ledger));
    let mut adapter = SharedPolicy(&policy);
    let context = AdmissionContext::new(&mut adapter);
    super::decode(
        content,
        body_ceiling,
        &policy,
        &context,
        CheckpointSeed { slug, session },
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::{ledger::BudgetError, query::ControlStop};

    struct Open;
    impl SharedLedger for Open {
        fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    #[test]
    fn checkpoint_projection_requires_identity_and_complete_typed_arrays() {
        let base = r#"{"concept":"agent","session":2,"date":"2026-11-21","established":["\u4e2d"],"exploredEdges":[],"frontier":[{"id":"a","reason":"b","priority":"low"}]}"#;
        assert!(validate_legacy_checkpoint(base.as_bytes(), "agent", 2, 1024, &mut Open).is_ok());
        assert!(validate_legacy_checkpoint(base.as_bytes(), "agent", 1, 1024, &mut Open).is_err());
        for value in [
            base.replace("\"low\"", "\"urgent\""),
            base.replace("\"low\"", r#"{"low":null}"#),
            base.replace(
                r#"{"id":"a","reason":"b","priority":"low"}"#,
                r#"["a","b","low"]"#,
            ),
            base.replace("\"established\":[\"\\u4e2d\"],", ""),
            base.replace("\"established\":[\"\\u4e2d\"]", "\"established\":[1]"),
            format!(
                "{}{},{}",
                &base[..base.len() - 1],
                ",\"nextEntryPoint\":null",
                "}"
            ),
        ] {
            assert!(
                validate_legacy_checkpoint(value.as_bytes(), "agent", 2, 1024, &mut Open).is_err(),
                "{value}"
            );
        }
    }
}
