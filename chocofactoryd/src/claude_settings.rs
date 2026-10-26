//! A role's `claude_settings` (#346): Claude Code settings the operator
//! writes *for choco's agents*, passed to `claude --settings` as one JSON
//! object.
//!
//! This module owns only the shape: a YAML mapping converted to a JSON
//! object, with every shape choco cannot represent refused. It never checks
//! a value against Claude Code's schema; Claude Code owns what a key means.
//! Errors name the problem and never carry a value, because a value can be
//! an `apiKeyHelper` command or a token.

use std::fmt;

use serde_json::{Map, Number, Value};

/// The merged settings for one role: a JSON object. Empty means "nothing
/// extra".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeSettings(Map<String, Value>);

/// Why a YAML value is not a usable `claude_settings`. Carries no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    /// The field is not a mapping; holds the kind it was.
    NotAMapping(&'static str),
    /// A mapping key (at any depth) that is not a string.
    NonStringKey,
    /// A YAML tag (`!foo`) anywhere in the value.
    Tag,
    /// `.nan` / `.inf`: JSON cannot hold it.
    NonFiniteNumber,
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::NotAMapping(kind) => {
                write!(f, "must be a mapping of Claude Code settings, not {kind}")
            }
            ShapeError::NonStringKey => {
                f.write_str("has a mapping key that is not a string; JSON keys are strings")
            }
            ShapeError::Tag => f.write_str("has a YAML tag (!...), which JSON cannot hold"),
            ShapeError::NonFiniteNumber => {
                f.write_str("has a number JSON cannot hold (.nan or .inf)")
            }
        }
    }
}

impl std::error::Error for ShapeError {}

impl ClaudeSettings {
    /// Converts the raw YAML of a `claude_settings` field. Missing and `null`
    /// are empty; anything but a mapping is refused.
    pub fn from_yaml(value: Option<serde_yaml::Value>) -> Result<Self, ShapeError> {
        match value {
            None | Some(serde_yaml::Value::Null) => Ok(Self::default()),
            Some(serde_yaml::Value::Mapping(mapping)) => {
                match convert(serde_yaml::Value::Mapping(mapping))? {
                    Value::Object(map) => Ok(Self(map)),
                    _ => unreachable!("a mapping converts to an object"),
                }
            }
            Some(other) => Err(ShapeError::NotAMapping(kind_name(&other))),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The top-level key names, sorted. Never a value.
    pub fn keys_sorted(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.0.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn map(&self) -> &Map<String, Value> {
        &self.0
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Replaces each top-level key of `self` that `layer` also sets (no deep
    /// merge: the layer's value wins whole, even when it is `null`).
    pub fn overlay(&mut self, layer: &ClaudeSettings) {
        for (key, value) in &layer.0 {
            self.0.insert(key.clone(), value.clone());
        }
    }

    /// Removes top-level keys whose value is `null`: Claude Code discards the
    /// whole `--settings` on one, so a `null` is "unset this key", not a value.
    pub fn drop_null_keys(&mut self) {
        self.0.retain(|_, value| !value.is_null());
    }
}

fn kind_name(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a list",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

fn convert(value: serde_yaml::Value) -> Result<Value, ShapeError> {
    Ok(match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::String(s) => Value::String(s),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else {
                let f = n.as_f64().ok_or(ShapeError::NonFiniteNumber)?;
                Value::Number(Number::from_f64(f).ok_or(ShapeError::NonFiniteNumber)?)
            }
        }
        serde_yaml::Value::Sequence(items) => {
            Value::Array(items.into_iter().map(convert).collect::<Result<_, _>>()?)
        }
        serde_yaml::Value::Mapping(mapping) => {
            let mut map = Map::new();
            for (key, value) in mapping {
                let serde_yaml::Value::String(key) = key else {
                    return Err(ShapeError::NonStringKey);
                };
                map.insert(key, convert(value)?);
            }
            Value::Object(map)
        }
        serde_yaml::Value::Tagged(_) => return Err(ShapeError::Tag),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<ClaudeSettings, ShapeError> {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        ClaudeSettings::from_yaml(Some(value))
    }

    #[test]
    fn missing_null_and_empty_are_empty() {
        assert!(ClaudeSettings::from_yaml(None).unwrap().is_empty());
        assert!(parse("~").unwrap().is_empty());
        assert!(parse("{}").unwrap().is_empty());
    }

    #[test]
    fn a_mapping_converts_with_integers_floats_and_nesting() {
        let settings = parse("a: 1\nb: 1.5\nc: [x, true, ~]\nd: {e: {f: g}}\nh: -3").unwrap();
        assert_eq!(
            Value::Object(settings.map().clone()),
            serde_json::json!({"a": 1, "b": 1.5, "c": ["x", true, null], "d": {"e": {"f": "g"}}, "h": -3})
        );
        assert_eq!(settings.keys_sorted(), vec!["a", "b", "c", "d", "h"]);
    }

    #[test]
    fn other_shapes_are_refused_without_their_value() {
        for (yaml, kind) in [
            ("[str-secret-1]", "a list"),
            ("str-secret-1", "a string"),
            ("4242", "a number"),
            ("true", "a boolean"),
        ] {
            let err = parse(yaml).unwrap_err();
            assert_eq!(err, ShapeError::NotAMapping(kind));
            let text = err.to_string();
            assert!(
                !text.contains("str-secret-1") && !text.contains("4242"),
                "{text}"
            );
        }
    }

    #[test]
    fn non_string_keys_tags_and_non_finite_numbers_are_refused_at_any_depth() {
        assert_eq!(parse("1: x"), Err(ShapeError::NonStringKey));
        assert_eq!(parse("~: x"), Err(ShapeError::NonStringKey));
        assert_eq!(parse("env: {true: x}"), Err(ShapeError::NonStringKey));
        assert_eq!(parse("env: [{1: x}]"), Err(ShapeError::NonStringKey));
        assert_eq!(
            parse("!foo {a: 1}"),
            Err(ShapeError::NotAMapping("a tagged value"))
        );
        assert_eq!(parse("env: {A: !t x}"), Err(ShapeError::Tag));
        assert_eq!(parse("x: .nan"), Err(ShapeError::NonFiniteNumber));
        assert_eq!(parse("x: [.inf]"), Err(ShapeError::NonFiniteNumber));
        assert_eq!(parse("x: -.inf"), Err(ShapeError::NonFiniteNumber));
    }

    #[test]
    fn overlay_replaces_whole_keys_and_nulls_are_then_dropped() {
        let mut base = parse("env: {A: a}\nattribution: {commit: c}").unwrap();
        base.overlay(&parse("env: {B: b}\nx: ~").unwrap());
        assert_eq!(base.keys_sorted(), vec!["attribution", "env", "x"]);
        assert_eq!(base.map()["env"], serde_json::json!({"B": "b"}));
        base.overlay(&parse("env: ~").unwrap());
        assert_eq!(base.map()["env"], Value::Null, "a null in the layer wins");
        base.drop_null_keys();
        assert_eq!(base.keys_sorted(), vec!["attribution"]);
    }
}
