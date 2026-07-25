//! Role config resolution (design §5.5, Q8; P1-8 LLD §2.3): merges a
//! role's `cli`/`model`/system prompt from three layers, most specific
//! wins independently per field — task-level `config.roles.<name>` →
//! workflow-def `roles:` block → global config. All three layers are
//! keyed by role name with the same shape; this function is generic over
//! `role_name` (it never assumes a particular name), which is exactly
//! what lets #17/P2-6 resolve several roles on one task by calling this
//! once per role, without changing this module at all.

use std::fmt;

use serde_json::Value;

use crate::adapter::RoleConfig;
use crate::global_config::GlobalConfig;
use crate::workflow_def::RoleDef;

#[derive(Debug)]
pub struct ResolvedRoleConfig {
    pub cli: String,
    pub model: String,
    pub role_config: RoleConfig,
}

#[derive(Debug)]
pub enum RoleConfigError {
    /// `cli`/`model` had no value in any of the three layers.
    MissingField { role: String, field: &'static str },
    /// A resolved `system_prompt_file` (workflow-def or global layer)
    /// couldn't be read — both loaders already checked it exists at their
    /// own load time, so this is only reachable via a TOCTOU (the file
    /// removed in between) rather than an authoring mistake.
    SystemPromptIo(std::io::Error),
}

impl fmt::Display for RoleConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoleConfigError::MissingField { role, field } => write!(
                f,
                "role '{role}' has no '{field}' from task config, workflow definition, or global config"
            ),
            RoleConfigError::SystemPromptIo(err) => {
                write!(f, "failed to read resolved system prompt file: {err}")
            }
        }
    }
}

impl std::error::Error for RoleConfigError {}

/// Resolves `role_name`'s final `cli`/`model`/system prompt against the
/// three layers, plus `cwd` (task-wide, not per-role — passed straight
/// through, not resolved here).
pub fn resolve(
    role_name: &str,
    role_def: &RoleDef,
    global: &GlobalConfig,
    task_config: &Value,
    cwd: std::path::PathBuf,
) -> Result<ResolvedRoleConfig, RoleConfigError> {
    let task_role = task_config.get("roles").and_then(|r| r.get(role_name));
    let global_role = global.roles.get(role_name);

    let cli = resolve_string_field(
        task_role,
        "cli",
        role_def.cli.as_deref(),
        global_role.and_then(|g| g.cli.as_deref()),
    )
    .ok_or_else(|| RoleConfigError::MissingField {
        role: role_name.to_string(),
        field: "cli",
    })?;

    let model = resolve_string_field(
        task_role,
        "model",
        role_def.model.as_deref(),
        global_role.and_then(|g| g.model.as_deref()),
    )
    .ok_or_else(|| RoleConfigError::MissingField {
        role: role_name.to_string(),
        field: "model",
    })?;

    let system_prompt = if let Some(text) = task_role
        .and_then(|r| r.get("system_prompt"))
        .and_then(Value::as_str)
    {
        Some(text.to_string())
    } else if let Some(path) = &role_def.system_prompt_file {
        Some(std::fs::read_to_string(path).map_err(RoleConfigError::SystemPromptIo)?)
    } else if let Some(path) = global_role.and_then(|g| g.system_prompt_file.as_ref()) {
        Some(std::fs::read_to_string(path).map_err(RoleConfigError::SystemPromptIo)?)
    } else {
        None
    };

    Ok(ResolvedRoleConfig {
        cli,
        model: model.clone(),
        role_config: RoleConfig {
            cwd,
            model: Some(model),
            system_prompt,
        },
    })
}

/// `task_role`/`workflow_def_value`/`global_value` in decreasing
/// precedence, matching the wrong-type-or-absent-means-not-set convention
/// `engine.rs` already uses for `task.config.get("cwd")`.
fn resolve_string_field(
    task_role: Option<&Value>,
    key: &str,
    workflow_def_value: Option<&str>,
    global_value: Option<&str>,
) -> Option<String> {
    task_role
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .or(workflow_def_value)
        .or(global_value)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn role_def(cli: Option<&str>, model: Option<&str>) -> RoleDef {
        RoleDef {
            cli: cli.map(str::to_string),
            model: model.map(str::to_string),
            system_prompt_file: None,
        }
    }

    #[test]
    fn task_level_wins_over_workflow_def_and_global() {
        let mut global = GlobalConfig::default();
        global.roles.insert(
            "chat".to_string(),
            crate::global_config::GlobalRoleConfig {
                cli: Some("global-cli".to_string()),
                model: Some("global-model".to_string()),
                system_prompt_file: None,
            },
        );
        let def = role_def(Some("def-cli"), Some("def-model"));
        let task_config = json!({ "roles": { "chat": { "model": "haiku" } } });

        let resolved = resolve("chat", &def, &global, &task_config, "/cwd".into()).unwrap();
        assert_eq!(resolved.cli, "def-cli"); // no task-level override, workflow-def wins
        assert_eq!(resolved.model, "haiku"); // task-level override wins
    }

    #[test]
    fn workflow_def_wins_over_global_when_task_is_silent() {
        let mut global = GlobalConfig::default();
        global.roles.insert(
            "chat".to_string(),
            crate::global_config::GlobalRoleConfig {
                cli: Some("global-cli".to_string()),
                model: Some("global-model".to_string()),
                system_prompt_file: None,
            },
        );
        let def = role_def(Some("def-cli"), None);
        let task_config = json!({});

        let resolved = resolve("chat", &def, &global, &task_config, "/cwd".into()).unwrap();
        assert_eq!(resolved.cli, "def-cli");
        assert_eq!(resolved.model, "global-model");
    }

    #[test]
    fn global_fills_in_when_task_and_workflow_def_are_both_silent() {
        let mut global = GlobalConfig::default();
        global.roles.insert(
            "chat".to_string(),
            crate::global_config::GlobalRoleConfig {
                cli: Some("global-cli".to_string()),
                model: Some("global-model".to_string()),
                system_prompt_file: None,
            },
        );
        let def = role_def(None, None);
        let task_config = json!({});

        let resolved = resolve("chat", &def, &global, &task_config, "/cwd".into()).unwrap();
        assert_eq!(resolved.cli, "global-cli");
        assert_eq!(resolved.model, "global-model");
    }

    #[test]
    fn missing_field_in_every_layer_is_an_error() {
        let def = role_def(None, None);
        let task_config = json!({});

        let err = resolve(
            "chat",
            &def,
            &GlobalConfig::default(),
            &task_config,
            "/cwd".into(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            RoleConfigError::MissingField { field, .. } if field == "cli"
        ));
    }

    #[test]
    fn task_level_system_prompt_is_inline_text_not_a_file() {
        let def = role_def(Some("cli"), Some("model"));
        let task_config = json!({ "roles": { "chat": { "system_prompt": "be nice" } } });

        let resolved = resolve(
            "chat",
            &def,
            &GlobalConfig::default(),
            &task_config,
            "/cwd".into(),
        )
        .unwrap();
        assert_eq!(
            resolved.role_config.system_prompt.as_deref(),
            Some("be nice")
        );
    }

    #[test]
    fn resolving_two_different_role_names_never_cross_contaminates() {
        // Directly de-risks #17: proves `resolve` reads whatever
        // `role_name` it's given, not a name baked into its own logic.
        let mut global = GlobalConfig::default();
        global.roles.insert(
            "coder".to_string(),
            crate::global_config::GlobalRoleConfig {
                cli: Some("coder-global-cli".to_string()),
                model: None,
                system_prompt_file: None,
            },
        );
        global.roles.insert(
            "reviewer".to_string(),
            crate::global_config::GlobalRoleConfig {
                cli: Some("reviewer-global-cli".to_string()),
                model: None,
                system_prompt_file: None,
            },
        );
        let coder_def = role_def(None, Some("coder-model"));
        let reviewer_def = role_def(None, Some("reviewer-model"));
        let task_config = json!({
            "roles": {
                "coder": { "model": "coder-task-model" },
                "reviewer": { "model": "reviewer-task-model" }
            }
        });

        let coder = resolve("coder", &coder_def, &global, &task_config, "/cwd".into()).unwrap();
        let reviewer = resolve(
            "reviewer",
            &reviewer_def,
            &global,
            &task_config,
            "/cwd".into(),
        )
        .unwrap();

        assert_eq!(coder.cli, "coder-global-cli");
        assert_eq!(coder.model, "coder-task-model");
        assert_eq!(reviewer.cli, "reviewer-global-cli");
        assert_eq!(reviewer.model, "reviewer-task-model");
    }
}
