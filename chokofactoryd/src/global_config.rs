//! Global config (design §5.5, Q8; P1-8 LLD §2.2): this user's machine-wide
//! defaults for a role's `cli`/`model`/system prompt — the least-specific
//! of the three layers `role_config::resolve` merges (workflow-def and
//! task-level config are more specific). Lives at
//! `~/.config/chokofactory/config.yaml` (`config_root`); a missing file is
//! a normal, supported state (no global defaults), not an error.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::fileref::{self, FileRefError};

/// Shape mirrors the workflow-def `roles:` block deliberately (keyed by
/// role name, same fields) so #17/P2-6 can extend both in lockstep.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalConfig {
    pub roles: HashMap<String, GlobalRoleConfig>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalRoleConfig {
    pub cli: Option<String>,
    pub model: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
}

impl GlobalConfig {
    /// `$HOME/.config/chokofactory/config.yaml`, or `None` if `$HOME`
    /// isn't set — the caller then just uses `default()`.
    pub fn default_path() -> Option<PathBuf> {
        crate::config_root::config_root().map(|root| root.join("config.yaml"))
    }

    /// Missing file ⇒ `Ok(default())` (no global config is normal); only a
    /// *malformed* file is an error. `system_prompt_file` values resolve
    /// relative to `path`'s own parent directory, reusing the same
    /// traversal guard the workflow loader uses for its file references —
    /// this is an operator-controlled file, not request input, but
    /// there's no reason to hold it to a lower bar.
    ///
    /// Attempts the read directly rather than checking `path.is_file()`
    /// first: a separate existence check followed by a read is a
    /// check-then-act race (the file could vanish in between), and it's
    /// entirely avoidable here since a failed read's `NotFound` already
    /// tells us exactly the same thing the check would have, atomically.
    pub fn load(path: &Path) -> Result<Self, GlobalConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => return Err(GlobalConfigError::Io(err)),
        };
        let parsed: RawGlobalConfig =
            serde_yaml::from_str(&raw).map_err(GlobalConfigError::Yaml)?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        let roles = parsed
            .roles
            .into_iter()
            .map(|(name, role)| -> Result<_, GlobalConfigError> {
                let system_prompt_file = role
                    .system_prompt_file
                    .map(|rel| {
                        fileref::resolve_relative(base_dir, &rel).map_err(|err| match err {
                            FileRefError::Escapes => GlobalConfigError::InvalidFileReference {
                                role: name.clone(),
                                value: rel.clone(),
                            },
                            FileRefError::Missing(path) => {
                                GlobalConfigError::MissingReferencedFile {
                                    role: name.clone(),
                                    path,
                                }
                            }
                        })
                    })
                    .transpose()?;
                Ok((
                    name,
                    GlobalRoleConfig {
                        cli: role.cli,
                        model: role.model,
                        system_prompt_file,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(GlobalConfig { roles })
    }
}

#[derive(Debug, Deserialize)]
struct RawGlobalConfig {
    // Same duplicate-key guard as the workflow loader's `roles:`/`stages:`
    // maps (`serde_util::deserialize_map_rejecting_duplicate_keys`) — a
    // plain `HashMap`/`IndexMap` deserialization silently keeps only the
    // last of two repeated `roles:` keys in a hand-edited config.yaml,
    // discarding the first without a trace. Caught in review round 2.
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
    )]
    roles: IndexMap<String, RawGlobalRoleConfig>,
}

#[derive(Debug, Deserialize)]
struct RawGlobalRoleConfig {
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
}

#[derive(Debug)]
pub enum GlobalConfigError {
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
    InvalidFileReference { role: String, value: String },
    MissingReferencedFile { role: String, path: PathBuf },
}

impl fmt::Display for GlobalConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlobalConfigError::Io(err) => write!(f, "failed to read global config: {err}"),
            GlobalConfigError::Yaml(err) => write!(f, "failed to parse global config: {err}"),
            GlobalConfigError::InvalidFileReference { role, value } => write!(
                f,
                "role '{role}' references system_prompt_file '{value}', which is an absolute path or escapes the global config's directory"
            ),
            GlobalConfigError::MissingReferencedFile { role, path } => write!(
                f,
                "role '{role}' references system_prompt_file '{}', which does not exist",
                path.display()
            ),
        }
    }
}

impl std::error::Error for GlobalConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "chokofactoryd-global-config-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = TempDir::new();
        let config = GlobalConfig::load(&dir.path.join("nope.yaml")).unwrap();
        assert_eq!(config, GlobalConfig::default());
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", "roles: [not, a, map]\n");
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(err, GlobalConfigError::Yaml(_)));
    }

    #[test]
    fn loads_per_role_cli_and_model_defaults() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  chat:\n    cli: claude\n    model: opus\n",
        );
        let config = GlobalConfig::load(&path).unwrap();
        assert_eq!(config.roles["chat"].cli.as_deref(), Some("claude"));
        assert_eq!(config.roles["chat"].model.as_deref(), Some("opus"));
        assert_eq!(config.roles["chat"].system_prompt_file, None);
    }

    #[test]
    fn resolves_system_prompt_file_relative_to_the_config_files_directory() {
        let dir = TempDir::new();
        dir.write("chat-system.md", "be nice");
        let path = dir.write(
            "config.yaml",
            "roles:\n  chat:\n    system_prompt_file: chat-system.md\n",
        );
        let config = GlobalConfig::load(&path).unwrap();
        assert_eq!(
            config.roles["chat"].system_prompt_file,
            Some(dir.path.join("chat-system.md"))
        );
    }

    #[test]
    fn rejects_a_system_prompt_file_that_escapes_the_config_directory() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  chat:\n    system_prompt_file: \"../../../../etc/passwd\"\n",
        );
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(
            err,
            GlobalConfigError::InvalidFileReference { .. }
        ));
    }

    #[test]
    fn rejects_a_system_prompt_file_that_does_not_exist() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  chat:\n    system_prompt_file: does-not-exist.md\n",
        );
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(
            err,
            GlobalConfigError::MissingReferencedFile { .. }
        ));
    }

    #[test]
    fn rejects_a_duplicate_role_key_instead_of_silently_dropping_the_first() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  chat:\n    model: opus\n  chat:\n    cli: claude\n",
        );
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(err, GlobalConfigError::Yaml(_)));
        assert!(err.to_string().contains("duplicate key"));
    }
}
