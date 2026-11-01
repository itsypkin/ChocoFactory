//! Global config (design §5.5, Q8; P1-8 LLD §2.2): this user's machine-wide
//! defaults for a role's `cli`/`model`/system prompt — the least-specific
//! of the three layers `role_config::resolve` merges (workflow-def and
//! task-level config are more specific). Lives at
//! `~/.config/chocofactory/config.yaml` (`config_root`); a missing file is
//! a normal, supported state (no global defaults), not an error.
//!
//! A role here may also carry `claude_settings` (#346): Claude Code
//! settings for choco's agents, the less specific of that field's two layers
//! (the workflow file's role wins, key by key). The keys in
//! [`WORKFLOW_ONLY_CLAUDE_SETTINGS`] may only come from a workflow file.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::adapter::{Isolation, Registry, UnknownCliError};
use crate::claude_settings::{ClaudeSettings, ShapeError};
use crate::fileref::{self, FileRefError};

/// Shape mirrors the workflow-def `roles:` block deliberately (keyed by
/// role name, same fields) so #17/P2-6 can extend both in lockstep.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalConfig {
    pub roles: HashMap<String, GlobalRoleConfig>,
    /// The server pool's cap on agent turns running at once. `None`: no
    /// limit (the key is absent, null or `0`).
    pub max_agents: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalRoleConfig {
    pub cli: Option<String>,
    pub model: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
    /// Claude Code settings for this role's agents (#346). Empty when the
    /// key is missing, `null` or `{}`.
    pub claude_settings: ClaudeSettings,
}

/// `claude_settings` keys that run code or change what an agent loads, so
/// only a workflow file (which travels with its prompts and is reviewed with
/// them) may set them; config.yaml may not. Matched exactly, top level only.
pub const WORKFLOW_ONLY_CLAUDE_SETTINGS: [&str; 10] = [
    "hooks",
    "disableAllHooks",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "pluginConfigs",
    "outputStyle",
    "claudeMdExcludes",
    "enableAllProjectMcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
];

/// A config.yaml role set `claude_settings` keys only a workflow file may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowOnlyKeyError {
    pub role: String,
    /// Sorted.
    pub keys: Vec<String>,
}

impl fmt::Display for WorkflowOnlyKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "role '{}' sets claude_settings '{}' in the global config, which only a workflow file may set",
            self.role,
            self.keys.join("', '")
        )
    }
}

impl std::error::Error for WorkflowOnlyKeyError {}

/// Refuses a config.yaml role's `claude_settings` that holds a
/// workflow-only key, listing every offending key. Names keys, never values.
pub fn check_workflow_only_claude_settings(
    role: &str,
    settings: &ClaudeSettings,
) -> Result<(), WorkflowOnlyKeyError> {
    let mut keys: Vec<String> = WORKFLOW_ONLY_CLAUDE_SETTINGS
        .iter()
        .filter(|key| settings.contains_key(key))
        .map(|key| key.to_string())
        .collect();
    keys.sort();
    if keys.is_empty() {
        Ok(())
    } else {
        Err(WorkflowOnlyKeyError {
            role: role.to_string(),
            keys,
        })
    }
}

impl GlobalConfig {
    /// `$HOME/.config/chocofactory/config.yaml`, or `None` if `$HOME`
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
            Err(err) => {
                return Err(GlobalConfigError::Io {
                    path: path.to_path_buf(),
                    source: err,
                });
            }
        };
        let parsed: RawGlobalConfig =
            serde_yaml::from_str(&raw).map_err(|source| GlobalConfigError::Yaml {
                path: path.to_path_buf(),
                source,
            })?;
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
                let claude_settings =
                    ClaudeSettings::from_yaml(role.claude_settings).map_err(|problem| {
                        GlobalConfigError::InvalidClaudeSettings {
                            path: path.to_path_buf(),
                            role: name.clone(),
                            problem,
                        }
                    })?;
                Ok((
                    name,
                    GlobalRoleConfig {
                        cli: role.cli,
                        model: role.model,
                        system_prompt_file,
                        claude_settings,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(GlobalConfig {
            roles,
            max_agents: parsed.max_agents.filter(|n| *n > 0),
        })
    }
}

/// Daemon-start check: every `cli:` in the global config at `path` must
/// name an adapter in `registry`, a role with a `cli` and a `model` must
/// have a model its adapter accepts, a role's `claude_settings` must hold no
/// workflow-only key, and a role with a `cli` must have an adapter that takes
/// its `claude_settings`. config.yaml carries no isolation, so only checks
/// that hold for every isolation run here. A missing file or `None` path is fine; a
/// file that can't be loaded logs a warning and passes, as it always has
/// (the error still surfaces when a turn needs the config).
pub fn check_global_roles(
    path: Option<&Path>,
    registry: &Registry,
) -> Result<(), StartupRoleError> {
    let Some(path) = path else {
        return Ok(());
    };
    let config = match GlobalConfig::load(path) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "could not load the global config; skipping its cli check");
            return Ok(());
        }
    };
    let mut roles: Vec<(&String, &GlobalRoleConfig)> = config.roles.iter().collect();
    roles.sort_by_key(|(name, _)| *name);
    for (name, role) in roles {
        check_workflow_only_claude_settings(name, &role.claude_settings)
            .map_err(StartupRoleError::WorkflowOnlyClaudeSettings)?;
        if let Some(cli) = &role.cli {
            let adapter = registry
                .lookup(Some(name), cli)
                .map_err(StartupRoleError::UnknownCli)?;
            adapter
                .validate_role(
                    name,
                    &Isolation::InheritOperatorConfig,
                    &role.claude_settings,
                )
                .map_err(StartupRoleError::RoleRejected)?;
            if let Some(model) = &role.model {
                adapter
                    .validate_model(name, model)
                    .map_err(StartupRoleError::RoleRejected)?;
            }
        }
    }
    Ok(())
}

/// Why `check_global_roles` refused the daemon's start.
#[derive(Debug)]
pub enum StartupRoleError {
    UnknownCli(UnknownCliError),
    /// A role's adapter can't run its model string (a bad level suffix) or
    /// its `claude_settings`.
    RoleRejected(String),
    /// A role's `claude_settings` holds a key only a workflow file may set.
    WorkflowOnlyClaudeSettings(WorkflowOnlyKeyError),
}

impl std::fmt::Display for StartupRoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupRoleError::UnknownCli(err) => write!(f, "{err}"),
            StartupRoleError::RoleRejected(message) => write!(f, "{message}"),
            StartupRoleError::WorkflowOnlyClaudeSettings(err) => write!(f, "{err}"),
        }
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
    /// A non-negative integer; anything else (`-1`, `two`, `1.5`) is a parse
    /// error, never "no limit".
    #[serde(default)]
    max_agents: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawGlobalRoleConfig {
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
    #[serde(default)]
    claude_settings: Option<serde_yaml::Value>,
}

#[derive(Debug)]
pub enum GlobalConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Yaml {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    InvalidFileReference {
        role: String,
        value: String,
    },
    MissingReferencedFile {
        role: String,
        path: PathBuf,
    },
    /// A role's `claude_settings` is not a JSON-representable mapping (#346).
    InvalidClaudeSettings {
        path: PathBuf,
        role: String,
        problem: ShapeError,
    },
}

impl fmt::Display for GlobalConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlobalConfigError::Io { path, source } => {
                write!(
                    f,
                    "failed to read global config {}: {source}",
                    path.display()
                )
            }
            GlobalConfigError::Yaml { path, source } => {
                write!(
                    f,
                    "failed to parse global config {}: {source}",
                    path.display()
                )
            }
            GlobalConfigError::InvalidFileReference { role, value } => write!(
                f,
                "role '{role}' references system_prompt_file '{value}', which is an absolute path or escapes the global config's directory"
            ),
            GlobalConfigError::InvalidClaudeSettings {
                path,
                role,
                problem,
            } => write!(
                f,
                "{}: role '{role}': claude_settings {problem}",
                path.display()
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
                "chocofactoryd-global-config-test-{}",
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
        assert!(matches!(err, GlobalConfigError::Yaml { .. }));
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
        assert!(matches!(err, GlobalConfigError::Yaml { .. }));
        assert!(err.to_string().contains("duplicate key"));
    }

    fn claude_registry() -> Registry {
        Registry::single(std::sync::Arc::new(crate::adapter::ClaudeAdapter::new()))
    }

    #[test]
    fn check_global_roles_rejects_an_unknown_cli_naming_the_role() {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", "roles:\n  coder:\n    cli: cluade\n");
        let err = check_global_roles(Some(&path), &claude_registry()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "role 'coder' uses cli 'cluade', which this daemon doesn't know; known CLIs: claude"
        );
    }

    #[test]
    fn check_global_roles_rejects_a_bad_claude_level_naming_only_that_role() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  a_ok:\n    cli: claude\n    model: claude-opus-5-5:max\n  coder:\n    cli: claude\n    model: x:auto\n",
        );
        let err = check_global_roles(Some(&path), &claude_registry()).unwrap_err();
        assert!(matches!(err, StartupRoleError::RoleRejected(_)));
        let message = err.to_string();
        assert!(
            message.contains("`coder`") && message.contains("x:auto"),
            "{message}"
        );
        assert!(!message.contains("a_ok"), "{message}");
        let ok = dir.write(
            "ok.yaml",
            "roles:\n  a_ok:\n    cli: claude\n    model: claude-opus-5-5:max\n",
        );
        assert!(check_global_roles(Some(&ok), &claude_registry()).is_ok());
    }

    #[test]
    fn check_global_roles_reports_the_first_bad_role_in_sorted_order() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "roles:\n  zed:\n    cli: nope\n  alpha:\n    cli: nada\n",
        );
        let err = check_global_roles(Some(&path), &claude_registry()).unwrap_err();
        let StartupRoleError::UnknownCli(err) = err else {
            panic!("expected an unknown cli");
        };
        assert_eq!(err.role.as_deref(), Some("alpha"));
    }

    #[test]
    fn check_global_roles_accepts_what_startup_has_always_accepted() {
        let dir = TempDir::new();
        let registry = claude_registry();
        assert!(check_global_roles(None, &registry).is_ok());
        assert!(check_global_roles(Some(&dir.path.join("missing.yaml")), &registry).is_ok());
        let ok = dir.write("ok.yaml", "roles:\n  coder:\n    cli: claude\n");
        assert!(check_global_roles(Some(&ok), &registry).is_ok());
        let model_only = dir.write("m.yaml", "roles:\n  coder:\n    model: opus\n");
        assert!(check_global_roles(Some(&model_only), &registry).is_ok());
        let malformed = dir.write("bad.yaml", "roles: [not, a, map");
        assert!(check_global_roles(Some(&malformed), &registry).is_ok());
    }

    #[test]
    fn max_agents_absent_null_and_zero_mean_no_limit() {
        let dir = TempDir::new();
        for body in ["roles: {}\n", "max_agents:\n", "max_agents: 0\n", ""] {
            let path = dir.write("config.yaml", body);
            assert_eq!(
                GlobalConfig::load(&path).unwrap().max_agents,
                None,
                "{body:?}"
            );
        }
        let path = dir.write("config.yaml", "max_agents: 3\n");
        assert_eq!(GlobalConfig::load(&path).unwrap().max_agents, Some(3));
    }

    #[test]
    fn max_agents_that_is_not_a_non_negative_integer_is_an_error_naming_the_file() {
        let dir = TempDir::new();
        for body in ["max_agents: two\n", "max_agents: -1\n", "max_agents: 1.5\n"] {
            let path = dir.write("config.yaml", body);
            let err = GlobalConfig::load(&path).unwrap_err();
            assert!(matches!(err, GlobalConfigError::Yaml { .. }), "{body:?}");
            let text = err.to_string();
            assert!(text.contains(&path.display().to_string()), "{text}");
            assert!(text.starts_with("failed to parse global config "), "{text}");
        }
    }

    // ---- claude_settings (#346) ----

    fn settings_config(dir: &TempDir, settings_yaml: &str) -> PathBuf {
        dir.write(
            "config.yaml",
            &format!("roles:\n  coder:\n    claude_settings:{settings_yaml}\n"),
        )
    }

    #[test]
    fn claude_settings_missing_null_and_empty_load_as_nothing_extra() {
        let dir = TempDir::new();
        for body in [
            "roles:\n  coder:\n    cli: claude\n",
            "roles:\n  coder:\n    claude_settings:\n",
            "roles:\n  coder:\n    claude_settings: {}\n",
        ] {
            let path = dir.write("config.yaml", body);
            let config = GlobalConfig::load(&path).unwrap();
            assert!(config.roles["coder"].claude_settings.is_empty(), "{body:?}");
        }
    }

    #[test]
    fn claude_settings_load_as_a_json_object() {
        let dir = TempDir::new();
        let path = settings_config(&dir, " {attribution: {commit: ''}, env: {A: b}}");
        let config = GlobalConfig::load(&path).unwrap();
        assert_eq!(
            config.roles["coder"].claude_settings.keys_sorted(),
            vec!["attribution", "env"]
        );
    }

    #[test]
    fn claude_settings_of_the_wrong_shape_are_a_load_error_naming_path_role_and_field() {
        let dir = TempDir::new();
        for (value, kind) in [
            (" [str-secret-1]", "a list"),
            (" str-secret-1", "a string"),
            (" 4242", "a number"),
            (" true", "a boolean"),
        ] {
            let path = settings_config(&dir, value);
            let err = GlobalConfig::load(&path).unwrap_err();
            assert!(matches!(
                err,
                GlobalConfigError::InvalidClaudeSettings { .. }
            ));
            let text = err.to_string();
            assert_eq!(
                text,
                format!(
                    "{}: role 'coder': claude_settings must be a mapping of Claude Code settings, not {kind}",
                    path.display()
                )
            );
            assert!(!text.contains("str-secret-1") && !text.contains("4242"));
        }
        for (value, expected) in [
            (" {1: x}", "has a mapping key that is not a string"),
            (" {~: x}", "has a mapping key that is not a string"),
            (
                " {env: {true: x}}",
                "has a mapping key that is not a string",
            ),
            (" !foo {a: 1}", "must be a mapping"),
            (" {env: {A: !t x}}", "has a YAML tag"),
            (" {x: .nan}", "has a number JSON cannot hold"),
            (" {x: .inf}", "has a number JSON cannot hold"),
        ] {
            let path = settings_config(&dir, value);
            let text = GlobalConfig::load(&path).unwrap_err().to_string();
            assert!(
                text.contains("role 'coder': claude_settings"),
                "{value}: {text}"
            );
            assert!(text.contains(expected), "{value}: {text}");
        }
    }

    #[test]
    fn a_duplicate_key_inside_claude_settings_is_a_load_error() {
        let dir = TempDir::new();
        let path = settings_config(&dir, " {env: {A: a}, env: {B: b}}");
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(err, GlobalConfigError::Yaml { .. }), "{err}");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn every_workflow_only_key_is_refused_alone_naming_the_role_and_the_key() {
        let dir = TempDir::new();
        // The list is pinned by value, so deleting or misspelling an entry of
        // the constant fails here and not only in a loop that reads it.
        let expected = [
            "hooks",
            "disableAllHooks",
            "enabledPlugins",
            "extraKnownMarketplaces",
            "pluginConfigs",
            "outputStyle",
            "claudeMdExcludes",
            "enableAllProjectMcpServers",
            "enabledMcpjsonServers",
            "disabledMcpjsonServers",
        ];
        for key in expected {
            let path = settings_config(&dir, &format!(" {{{key}: {{}}}}"));
            let err = check_global_roles(Some(&path), &claude_registry()).unwrap_err();
            assert!(matches!(
                err,
                StartupRoleError::WorkflowOnlyClaudeSettings(_)
            ));
            assert_eq!(
                err.to_string(),
                format!(
                    "role 'coder' sets claude_settings '{key}' in the global config, which only a workflow file may set"
                )
            );
        }
    }

    #[test]
    fn a_mixed_settings_block_is_refused_naming_only_the_workflow_only_key() {
        let dir = TempDir::new();
        let path = settings_config(
            &dir,
            " {attribution: {commit: ''}, hooks: {SessionStart: []}, outputStyle: x}",
        );
        let err = check_global_roles(Some(&path), &claude_registry()).unwrap_err();
        let message = err.to_string();
        assert_eq!(
            message,
            "role 'coder' sets claude_settings 'hooks', 'outputStyle' in the global config, which only a workflow file may set"
        );
        assert!(!message.contains("attribution"));
        // Matched exactly, top level only: a nested or differently cased name is fine.
        let ok = settings_config(
            &dir,
            " {Hooks: {}, env: {hooks: x}, attribution: {hooks: y}}",
        );
        assert!(check_global_roles(Some(&ok), &claude_registry()).is_ok());
    }

    #[test]
    fn daemon_start_refuses_claude_settings_on_an_omp_role_but_takes_any_env_on_a_claude_one() {
        let dir = TempDir::new();
        let registry = Registry::new(vec![
            std::sync::Arc::new(crate::adapter::ClaudeAdapter::new()),
            std::sync::Arc::new(crate::adapter::OmpAdapter::new("/nonexistent")),
        ]);
        let omp = dir.write(
            "omp.yaml",
            "roles:\n  coder:\n    cli: omp\n    claude_settings:\n      attribution: {commit: x}\n",
        );
        let err = check_global_roles(Some(&omp), &registry).unwrap_err();
        assert!(matches!(err, StartupRoleError::RoleRejected(_)));
        assert_eq!(
            err.to_string(),
            "role 'coder' sets claude_settings, which a role on cli omp can't take; claude_settings is only for claude roles"
        );
        // config.yaml carries no isolation, so a chat role's `env: x` (fine
        // for an inheriting role) must not stop the daemon.
        let chat = dir.write(
            "chat.yaml",
            "roles:\n  chat:\n    cli: claude\n    claude_settings:\n      env: x\n",
        );
        assert!(check_global_roles(Some(&chat), &registry).is_ok());
    }

    #[test]
    fn an_unreadable_config_names_the_file() {
        let dir = TempDir::new();
        // A directory at the path makes the read fail without chmod.
        let path = dir.path.join("config.yaml");
        std::fs::create_dir(&path).unwrap();
        let err = GlobalConfig::load(&path).unwrap_err();
        assert!(matches!(err, GlobalConfigError::Io { .. }));
        assert!(err.to_string().contains(&path.display().to_string()));
    }
}
