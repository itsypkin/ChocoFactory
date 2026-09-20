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

/// Distinguishes an absent field from one explicitly sent as `null` (issue
/// #88's `PATCH /projects/{id}`: `repo_path` omitted means "leave
/// unchanged", `repo_path: null` means "clear it"). Plain
/// `Option<Option<T>>` can't do this on its own — serde's derive treats a
/// missing `Option<T>` field the same as an explicit `null`, both landing
/// on `None` — so a field that needs the distinction is instead typed
/// `Option<Option<T>>` with `#[serde(default, deserialize_with =
/// "deserialize_some")]`: `default` supplies the outer `None` when the key
/// is missing entirely, and this function — only ever invoked when the key
/// *is* present — always wraps whatever it parses (including a JSON
/// `null`, which becomes `Some(None)`) in `Some`.
pub fn deserialize_some<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod deserialize_some_tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::deserialize_some;

    #[derive(Deserialize)]
    struct Body {
        #[serde(default, deserialize_with = "deserialize_some")]
        repo_path: Option<Option<String>>,
    }

    #[test]
    fn distinguishes_absent_null_and_present() {
        let absent: Body = serde_json::from_value(json!({})).unwrap();
        assert_eq!(absent.repo_path, None);

        let null: Body = serde_json::from_value(json!({ "repo_path": null })).unwrap();
        assert_eq!(null.repo_path, Some(None));

        let present: Body = serde_json::from_value(json!({ "repo_path": "/repo" })).unwrap();
        assert_eq!(present.repo_path, Some(Some("/repo".to_string())));
    }
}
