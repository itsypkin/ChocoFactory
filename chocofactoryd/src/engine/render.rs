use super::*;

/// Substitutes a stage's earlier-captured values into an inline `command:`
/// (P2-3, §5.1).
///
/// A `script_file` is passed through untouched: §5.1 scopes templating to
/// `command:` and `prompt_file`, and rewriting an executable's contents on
/// the way to running it would be a different and much larger promise.
///
/// Rendered once here, on stage entry, rather than per attempt — a `poll`
/// re-runs the same command on every interval, and the payload cannot change
/// while the stage is current (captures land only on a transition), so
/// re-rendering would do identical work and invite the two to disagree.
/// Renders `command`'s templates, plus every placeholder that fell back to
/// an empty string doing so (#60) — the caller is responsible for surfacing
/// those (see `record_unresolved_template_note`), since this free function
/// has no access to `self`/the pool to do it itself.
pub(super) fn render_command(
    command: &ShellCommand,
    payload: &Value,
    stage_name: &str,
) -> Result<(ShellCommand, Vec<String>), EngineError> {
    match command {
        ShellCommand::Inline(line) => {
            let (rendered, unresolved) =
                template::render(line, payload).map_err(|err| EngineError::Template {
                    stage: stage_name.to_string(),
                    reason: err.to_string(),
                })?;
            Ok((ShellCommand::Inline(rendered), unresolved))
        }
        ShellCommand::ScriptFile(path) => Ok((ShellCommand::ScriptFile(path.clone()), Vec::new())),
    }
}

/// The largest an `env:` value may be once rendered (#101). Linux rejects a
/// single environment string over 128 KiB with `E2BIG`, which would fail
/// every lap of the stage the same way.
pub(super) const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

pub(super) struct RenderedEnv {
    pub(super) pairs: Vec<(String, String)>,
    pub(super) unresolved: Vec<String>,
    pub(super) truncated: Vec<String>,
}

/// Renders each `env:` value like a command template (#101). Values over
/// `MAX_ENV_VALUE_BYTES` are cut at a UTF-8 boundary and end in a suffix
/// saying how much was kept.
pub(super) fn render_env(
    env: &IndexMap<String, String>,
    payload: &Value,
    stage_name: &str,
) -> Result<RenderedEnv, EngineError> {
    let mut out = RenderedEnv {
        pairs: Vec::with_capacity(env.len()),
        unresolved: Vec::new(),
        truncated: Vec::new(),
    };
    for (name, template_text) in env {
        let (mut value, unresolved) =
            template::render(template_text, payload).map_err(|err| EngineError::Template {
                stage: stage_name.to_string(),
                reason: err.to_string(),
            })?;
        out.unresolved.extend(unresolved);
        if value.len() > MAX_ENV_VALUE_BYTES {
            let total = value.len();
            // The suffix's own length depends on the digits of `kept`, so
            // settle it by iterating; two rounds always suffice.
            let mut kept = MAX_ENV_VALUE_BYTES;
            let suffix = loop {
                let suffix = format!("\n[truncated by chocofactory: {kept} of {total} bytes]");
                let budget = MAX_ENV_VALUE_BYTES - suffix.len();
                let mut cut = budget.min(total);
                while !value.is_char_boundary(cut) {
                    cut -= 1;
                }
                if cut == kept {
                    value.truncate(cut);
                    break suffix;
                }
                kept = cut;
            };
            value.push_str(&suffix);
            out.truncated.push(name.clone());
        }
        out.pairs.push((name.clone(), value));
    }
    Ok(out)
}

impl WorkflowEngine {
    pub(super) async fn render_stage_command(
        &self,
        entry: &StageEntry<'_>,
        command: &ShellCommand,
        env: &IndexMap<String, String>,
    ) -> Result<(ShellCommand, Vec<(String, String)>), EngineError> {
        let StageEntry {
            task_id,
            definition,
            stage_name,
            payload,
            ..
        } = *entry;
        let (command, mut unresolved) = render_command(command, payload, stage_name)?;
        let (env, env_unresolved, truncated) = self
            .stage_environment(task_id, definition, stage_name, env, payload)
            .await?;
        unresolved.extend(env_unresolved);
        self.record_unresolved_template_note(task_id, stage_name, &unresolved)
            .await;
        self.record_env_truncated_note(task_id, stage_name, &truncated)
            .await;
        Ok((command, env))
    }

    /// The stage's rendered `env:` followed by the engine's `CHOCO_*`
    /// variables (so the engine's values win), plus what rendering left
    /// unresolved and the names it truncated. Computed on entry and never
    /// stored.
    pub(super) async fn stage_environment(
        &self,
        task_id: &str,
        definition: &WorkflowDefinition,
        stage_name: &str,
        env: &IndexMap<String, String>,
        payload: &Value,
    ) -> Result<(Vec<(String, String)>, Vec<String>, Vec<String>), EngineError> {
        let rendered = render_env(env, payload, stage_name)?;
        let mut pairs = rendered.pairs;
        pairs.extend(self.engine_env(task_id, definition, stage_name).await?);
        Ok((pairs, rendered.unresolved, rendered.truncated))
    }

    /// The variables the engine itself sets on a `shell`/`poll` command
    /// (#101): the task, workflow and stage, and the distinct `role=model`
    /// pairs the task's sessions ran on.
    async fn engine_env(
        &self,
        task_id: &str,
        definition: &WorkflowDefinition,
        stage_name: &str,
    ) -> Result<Vec<(String, String)>, EngineError> {
        let sessions = sessions::list_for_task(&self.pool, task_id).await?;
        let mut pairs: Vec<(String, String)> = sessions
            .into_iter()
            .map(|s| {
                let model = if s.model.is_empty() {
                    "default".to_string()
                } else {
                    s.model
                };
                (s.role, model)
            })
            .collect();
        pairs.sort();
        pairs.dedup();
        let role_models = pairs
            .iter()
            .map(|(role, model)| format!("{role}={model}"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(vec![
            ("CHOCO_TASK_ID".to_string(), task_id.to_string()),
            ("CHOCO_WORKFLOW".to_string(), definition.name.clone()),
            ("CHOCO_STAGE".to_string(), stage_name.to_string()),
            ("CHOCO_ROLE_MODELS".to_string(), role_models),
        ])
    }

    /// Records that an `env:` value was cut to `MAX_ENV_VALUE_BYTES`,
    /// best-effort like `record_unresolved_template_note`. No-op when
    /// nothing was truncated.
    pub(super) async fn record_env_truncated_note(
        &self,
        task_id: &str,
        stage_name: &str,
        names: &[String],
    ) {
        if names.is_empty() {
            return;
        }
        tracing::warn!(
            task_id,
            stage = stage_name,
            ?names,
            "stage env value exceeded the cap; truncated"
        );
        match events::append_for_task(
            &self.pool,
            task_id,
            EventType::EnvTruncated,
            json!({
                "stage": stage_name,
                "message": format!(
                    "stage '{stage_name}' truncated env variable(s) {} to {MAX_ENV_VALUE_BYTES} bytes",
                    names.join(", ")
                ),
                "env_truncated": names,
            }),
        )
        .await
        {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, %err,
                "failed to record an env-truncated event"
            ),
        }
    }

    /// Records every placeholder a stage's template fell back to an empty
    /// string for (#60), best-effort — logged loudly on failure, never
    /// propagated, same pattern as every other event append in this file.
    /// No-op when nothing was unresolved, so a caller can call this
    /// unconditionally after every render. Task-scoped (`append_for_task`,
    /// no `session_id`): a template renders before any turn/session
    /// exists, whether it's an `agent_turn`'s prompt or a `shell`/`poll`
    /// stage's `command:`.
    pub(super) async fn record_unresolved_template_note(
        &self,
        task_id: &str,
        stage_name: &str,
        placeholders: &[String],
    ) {
        if placeholders.is_empty() {
            return;
        }
        tracing::warn!(
            task_id,
            stage = stage_name,
            ?placeholders,
            "stage template referenced a value that isn't there yet; rendered as empty"
        );
        match events::append_for_task(
            &self.pool,
            task_id,
            EventType::TemplateUnresolved,
            json!({ "stage": stage_name, "placeholders": placeholders }),
        )
        .await
        {
            Ok(_) => self.events_notify.notify_waiters(),
            Err(err) => tracing::error!(
                task_id, stage = stage_name, %err,
                "failed to record a template-unresolved event"
            ),
        }
    }
}
