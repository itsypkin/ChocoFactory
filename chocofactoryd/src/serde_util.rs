//! Shared serde helpers. `deserialize_map_rejecting_duplicate_keys`
//! started out private to `workflow_def.rs`; extracted here (P1-8 review
//! round 2) so `global_config.rs` can reuse the same duplicate-key guard
//! for its own `roles:` block instead of silently losing data the way a
//! plain `HashMap`/`IndexMap` deserialization would.

use std::fmt;

use indexmap::IndexMap;
use serde::Deserialize;

/// `serde_yaml`'s map deserialization (like most `Deserialize` map impls)
/// just inserts each key as it's read, so a YAML mapping with a repeated
/// key — a copy-pasted stage/role name — silently keeps only the last
/// entry instead of erroring. That's exactly the kind of authoring typo
/// this exists to catch at load time, so entries are read one at a time
/// here and a repeat key is rejected instead of silently overwriting.
pub fn deserialize_map_rejecting_duplicate_keys<'de, D, T>(
    deserializer: D,
) -> Result<IndexMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = IndexMap<String, T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map with unique keys")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut result = IndexMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if result.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!("duplicate key '{key}'")));
                }
            }
            Ok(result)
        }
    }

    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}
