//! Workflow definition loader (design §5.1, §5.2). Parses a workflow's YAML
//! file into an in-memory graph and validates it at load time; the graph
//! itself is inert data — driving it through `workflow_state` is the
//! engine's job (P1-7), not this module's.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;

/// A parsed, validated workflow definition. `stages` preserves the YAML
/// file's declaration order because that order carries meaning: the first
/// stage declared is the graph's entry point (the format has no separate
/// `start:` field — see §5.1's examples, where `coding`/`chatting` are both
/// simply the first stage listed).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDefinition {
    pub name: String,
    pub roles: HashMap<String, RoleDef>,
    pub stages: IndexMap<String, StageDef>,
    /// Opt-in (§5.2, §5.5 Q7, issue #58): when set, the engine forks a
    /// dedicated `git worktree` for each task using this definition instead
    /// of running stages directly in the task's configured repo. `chat.yaml`
    /// leaves this unset — a chat task has no repo to fork.
    pub worktree: bool,
}

impl WorkflowDefinition {
    /// The workflow's entry stage: the first one declared in `stages:`.
    /// Safe to unwrap the `Option` after a successful `load`/`parse`, since
    /// validation rejects definitions with zero stages.
    pub fn start_stage(&self) -> &str {
        self.stages
            .get_index(0)
            .map(|(name, _)| name.as_str())
            .expect("validated definitions have at least one stage")
    }

    /// Reads and parses the definition file at `path`, resolving any
    /// `prompt_file`/`system_prompt_file`/`script_file` references relative
    /// to `path`'s parent directory, then validates the result.
    pub fn load(path: &Path) -> Result<Self, WorkflowDefError> {
        let raw = fs::read_to_string(path).map_err(WorkflowDefError::Io)?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&raw, base_dir)
    }

    /// Parses already-read YAML `source`, resolving file references
    /// relative to `base_dir`. Split out from `load` so tests can exercise
    /// parsing/validation without touching the filesystem for the
    /// definition file itself (resolved paths are still checked for
    /// existence against the real filesystem).
    pub fn parse(source: &str, base_dir: &Path) -> Result<Self, WorkflowDefError> {
        let raw: RawDefinition = serde_yaml::from_str(source).map_err(WorkflowDefError::Yaml)?;

        let roles = raw
            .roles
            .into_iter()
            .map(|(name, role)| -> Result<_, WorkflowDefError> {
                let system_prompt_file = role
                    .system_prompt_file
                    .map(|rel| {
                        resolve_file(base_dir, &rel, RefOwner::Role(&name), "system_prompt_file")
                    })
                    .transpose()?;
                Ok((
                    name,
                    RoleDef {
                        cli: role.cli,
                        model: role.model,
                        system_prompt_file,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        if raw.stages.is_empty() {
            return Err(WorkflowDefError::NoStages);
        }

        let stages = raw
            .stages
            .into_iter()
            .map(|(name, stage)| -> Result<_, WorkflowDefError> {
                let stage_def = stage.resolve(base_dir, &name)?;
                Ok((name, stage_def))
            })
            .collect::<Result<IndexMap<_, _>, _>>()?;

        let definition = WorkflowDefinition {
            name: raw.name,
            roles,
            stages,
            worktree: raw.worktree,
        };

        definition.validate()?;
        Ok(definition)
    }

    fn validate(&self) -> Result<(), WorkflowDefError> {
        for (stage_name, stage) in &self.stages {
            if let StageKind::AgentTurn { role, .. } = &stage.kind
                && !self.roles.contains_key(role)
            {
                return Err(WorkflowDefError::UnknownRole {
                    stage: stage_name.clone(),
                    role: role.clone(),
                });
            }

            if matches!(stage.kind, StageKind::Terminal) && !stage.on.is_empty() {
                return Err(WorkflowDefError::TerminalStageHasTransitions {
                    stage: stage_name.clone(),
                });
            }

            for target in stage.on.values() {
                if !self.stages.contains_key(target) {
                    return Err(WorkflowDefError::UnknownStageTarget {
                        stage: stage_name.clone(),
                        target: target.clone(),
                    });
                }
            }

            if let Some(guard) = &stage.loop_guard {
                if !stage.on.contains_key(&guard.on) {
                    return Err(WorkflowDefError::UnknownLoopGuardOutcome {
                        stage: stage_name.clone(),
                        outcome: guard.on.clone(),
                    });
                }
                if !self.stages.contains_key(&guard.then) {
                    return Err(WorkflowDefError::UnknownLoopGuardTarget {
                        stage: stage_name.clone(),
                        target: guard.then.clone(),
                    });
                }
            }

            // A shell stage always concludes with one of exactly two
            // outcomes (§5.2), and `error` is legitimately optional — a
            // workflow may want a failed command to park the task for a
            // human rather than route anywhere. `done` isn't: a stage that
            // can't act on the success path is a typo every time, and
            // without this the mistake only surfaces at runtime as an
            // `UnknownOutcome` from a detached runner, long after the
            // definition was loaded. Same shape as the `MissingTimeoutOutcome`
            // rule for `poll` below.
            if matches!(stage.kind, StageKind::Shell { .. }) && !stage.on.contains_key("done") {
                return Err(WorkflowDefError::MissingShellDoneOutcome {
                    stage: stage_name.clone(),
                });
            }

            if let StageKind::Poll {
                timeout, outcomes, ..
            } = &stage.kind
            {
                for outcome in outcomes {
                    if !stage.on.contains_key(&outcome.then) {
                        return Err(WorkflowDefError::UnknownPollOutcome {
                            stage: stage_name.clone(),
                            outcome: outcome.then.clone(),
                        });
                    }
                    if let Err(reason) = Regex::new(&outcome.pattern) {
                        return Err(WorkflowDefError::InvalidPollPattern {
                            stage: stage_name.clone(),
                            pattern: outcome.pattern.clone(),
                            reason: reason.to_string(),
                        });
                    }
                }
                if timeout.is_some() && !stage.on.contains_key("timeout") {
                    return Err(WorkflowDefError::MissingTimeoutOutcome {
                        stage: stage_name.clone(),
                    });
                }
            }

            // An `agent_turn` with an empty `on:` is chat's open-ended shape
            // (§5.4): it never concludes, so the engine spawns no turn
            // watcher for it and there is no moment at which a capture could
            // be taken. Declaring one is therefore dead config that would do
            // nothing — exactly the silent no-op #45 exists to remove, so it
            // is rejected here rather than ignored.
            if let StageKind::AgentTurn {
                capture: Some(_), ..
            } = &stage.kind
                && stage.on.is_empty()
            {
                return Err(WorkflowDefError::CaptureOnOpenEndedTurn {
                    stage: stage_name.clone(),
                });
            }

            // A human's reply is free text they typed, not a command's
            // structured stdout (#59) — `capture: json` would either fail
            // to parse (falling back to text with a warning nobody
            // watching the timeline is likely to notice) or, worse,
            // silently succeed on a reply that happens to look like JSON.
            // Rejected at load time rather than left to degrade quietly at
            // run time.
            if let StageKind::HumanGate {
                capture: Some(Capture::Json),
            } = &stage.kind
            {
                return Err(WorkflowDefError::HumanGateCaptureMustBeText {
                    stage: stage_name.clone(),
                });
            }

            self.validate_templates(stage_name, stage)?;
        }

        if !self.sink_reachable_from_start() {
            return Err(WorkflowDefError::NoReachableSink);
        }

        Ok(())
    }

    /// Checks every `{{ stages.<name>.<field> }}` reference this stage would
    /// render at run time (P2-3, §5.1).
    ///
    /// Only what the *definition* can know is checked: that the reference
    /// parses, and that the stage it names exists and captures something at
    /// all. Whether that stage's captured JSON actually carries the field is
    /// a run-time question — the shape isn't known until the command runs.
    ///
    /// Checking it here at all follows the same reasoning as
    /// `MissingShellDoneOutcome` above: a mistyped stage name is a typo every
    /// time, and left to run time it surfaces from a detached runner as a
    /// parked task, long after the definition was loaded.
    fn validate_templates(
        &self,
        stage_name: &str,
        stage: &StageDef,
    ) -> Result<(), WorkflowDefError> {
        for (field, source) in templatable_sources(stage_name, stage)? {
            let references = crate::template::references(&source).map_err(|err| {
                WorkflowDefError::InvalidTemplate {
                    stage: stage_name.to_string(),
                    field,
                    reason: err.to_string(),
                }
            })?;

            for reference in references {
                // `task` is always valid — it's payload the engine seeds
                // itself in `start_task` (P2-7a), not a stage's `capture:`,
                // so there's no stage to look up and no capture to require.
                let referenced_stage = match reference.root {
                    crate::template::Root::Task => continue,
                    crate::template::Root::Stage(stage) => stage,
                };
                let Some(target) = self.stages.get(&referenced_stage) else {
                    return Err(WorkflowDefError::UnknownTemplateStage {
                        stage: stage_name.to_string(),
                        field,
                        placeholder: reference.placeholder,
                        referenced: referenced_stage,
                    });
                };
                if !declares_capture(&target.kind) {
                    return Err(WorkflowDefError::TemplateStageCapturesNothing {
                        stage: stage_name.to_string(),
                        field,
                        placeholder: reference.placeholder,
                        referenced: referenced_stage,
                    });
                }
            }
        }
        Ok(())
    }

    /// A "sink" is a stage with an empty `on:` map — nowhere else to go.
    /// `terminal` stages are always sinks, but so is any stage that simply
    /// has no outgoing transitions declared, which is how the built-in chat
    /// workflow (§5.4) stays open indefinitely on purpose: a single
    /// `agent_turn` stage with `on: {}`. What's actually a bug is a graph
    /// that can *never* come to rest anywhere — every reachable stage keeps
    /// handing off to another one forever.
    fn sink_reachable_from_start(&self) -> bool {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![self.start_stage().to_string()];

        while let Some(name) = stack.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some(stage) = self.stages.get(&name) else {
                continue;
            };
            if stage.on.is_empty() {
                return true;
            }
            for target in stage.on.values() {
                stack.push(target.clone());
            }
            if let Some(guard) = &stage.loop_guard {
                stack.push(guard.then.clone());
            }
        }

        false
    }
}

/// A role's settings as declared in a workflow definition's `roles:`
/// block. `cli`/`model` are optional here (unlike the fully-resolved role
/// config the engine actually runs with, `role_config::ResolvedRoleConfig`)
/// — a workflow-def is only the *middle* of three layers (global config →
/// workflow-def → task-level override, P1-8 LLD §2.3); leaving a role
/// partially specified here is what lets it fall through to a global
/// default instead.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleDef {
    pub cli: Option<String>,
    pub model: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageDef {
    pub kind: StageKind,
    /// Outcome name -> next stage name.
    pub on: IndexMap<String, String>,
    pub loop_guard: Option<LoopGuard>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StageKind {
    AgentTurn {
        role: String,
        /// Absent for stages like chat's, which just relay live human
        /// input into the session rather than running a templated prompt.
        prompt_file: Option<PathBuf>,
        /// What to keep of the turn's reply (X-3/#45). `json` additionally
        /// makes the reply's reserved `outcome` key drive this stage's `on:`
        /// transition, which is how a reviewer's structured verdict routes
        /// the graph (§5.2).
        capture: Option<Capture>,
    },
    Shell {
        command: ShellCommand,
        capture: Option<Capture>,
        /// How long the command may run before it's killed and the stage
        /// emits `error`. §5.2 gives `timeout` only to `poll`, but a
        /// `shell` stage has no reaper of any kind behind it — unlike an
        /// `agent_turn`, which the idle reaper eventually force-closes —
        /// so without this a hung command parks its task until the daemon
        /// restarts. Optional: `None` means run to completion, however
        /// long that takes.
        timeout: Option<Duration>,
    },
    Poll {
        command: ShellCommand,
        capture: Option<Capture>,
        /// How long to wait between the end of one attempt and the start
        /// of the next.
        interval: Duration,
        /// How long to keep polling before giving up and emitting
        /// `timeout`. Unlike `Shell`'s field of the same name this is a
        /// budget for the whole loop rather than a per-command kill —
        /// though it doubles as the latter, since each attempt is capped
        /// at whatever is left of it.
        timeout: Option<Duration>,
        outcomes: Vec<PollOutcome>,
    },
    HumanGate {
        /// Keeps the human's reply that resumed this gate under
        /// `payload.stages.<this stage>` (#59), so a later stage — most
        /// often the coder a loop-guard escalation routes back to — can
        /// template `{{ stages.<gate>.… }}` and see what they said, instead
        /// of the redirect being silently discarded. `Capture::Json` is
        /// rejected at load time (`validate`) — a human's reply is free
        /// text, not a command's structured stdout.
        capture: Option<Capture>,
    },
    Terminal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShellCommand {
    Inline(String),
    ScriptFile(PathBuf),
}

/// What to do with what a stage produced — a `shell`/`poll` stage's stdout,
/// or an `agent_turn`'s reply (§5.1). Absent entirely, the output is simply
/// not retained — only a stage that says what it wants captured writes into
/// `workflow_state.payload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Json,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PollOutcome {
    /// Regex matched against the poll command's stdout.
    pub pattern: String,
    /// Outcome name looked up in the stage's `on:` map when `pattern` matches.
    pub then: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LoopGuard {
    pub on: String,
    pub max: u32,
    pub then: String,
}

#[derive(Debug, Deserialize)]
struct RawDefinition {
    name: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
    )]
    roles: IndexMap<String, RawRole>,
    #[serde(deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys")]
    stages: IndexMap<String, RawStage>,
    #[serde(default)]
    worktree: bool,
}

#[derive(Debug, Deserialize)]
struct RawRole {
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStage {
    #[serde(flatten)]
    kind: RawStageKind,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
    )]
    on: IndexMap<String, String>,
    #[serde(default)]
    loop_guard: Option<LoopGuard>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawStageKind {
    AgentTurn {
        role: String,
        #[serde(default)]
        prompt_file: Option<String>,
        #[serde(default)]
        capture: Option<Capture>,
    },
    Shell {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        script_file: Option<String>,
        #[serde(default)]
        capture: Option<Capture>,
        #[serde(default)]
        timeout: Option<String>,
    },
    Poll {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        script_file: Option<String>,
        #[serde(default)]
        capture: Option<Capture>,
        interval: String,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        outcomes: Vec<RawPollOutcome>,
    },
    HumanGate {
        #[serde(default)]
        capture: Option<Capture>,
    },
    Terminal,
}

#[derive(Debug, Deserialize)]
struct RawPollOutcome {
    #[serde(rename = "match")]
    pattern: String,
    then: String,
}

impl<'de> Deserialize<'de> for Capture {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "json" => Ok(Capture::Json),
            "text" => Ok(Capture::Text),
            other => Err(serde::de::Error::custom(format!(
                "unsupported capture kind '{other}' (expected 'json' or 'text')"
            ))),
        }
    }
}

impl RawStage {
    fn resolve(self, base_dir: &Path, stage_name: &str) -> Result<StageDef, WorkflowDefError> {
        let kind = match self.kind {
            RawStageKind::AgentTurn {
                role,
                prompt_file,
                capture,
            } => StageKind::AgentTurn {
                role,
                prompt_file: prompt_file
                    .map(|rel| {
                        resolve_file(base_dir, &rel, RefOwner::Stage(stage_name), "prompt_file")
                    })
                    .transpose()?,
                capture,
            },
            RawStageKind::Shell {
                command,
                script_file,
                capture,
                timeout,
            } => {
                let resolved_command = resolve_command(base_dir, stage_name, command, script_file)?;
                StageKind::Shell {
                    command: resolved_command,
                    capture,
                    timeout: timeout
                        .map(|value| {
                            parse_duration(&value).map_err(|value| {
                                WorkflowDefError::InvalidDuration {
                                    stage: stage_name.to_string(),
                                    field: "timeout",
                                    value,
                                }
                            })
                        })
                        .transpose()?,
                }
            }
            RawStageKind::Poll {
                command,
                script_file,
                capture,
                interval,
                timeout,
                outcomes,
            } => StageKind::Poll {
                command: resolve_command(base_dir, stage_name, command, script_file)?,
                capture,
                interval: parse_duration(&interval).map_err(|value| {
                    WorkflowDefError::InvalidDuration {
                        stage: stage_name.to_string(),
                        field: "interval",
                        value,
                    }
                })?,
                timeout: timeout
                    .map(|value| {
                        parse_duration(&value).map_err(|value| WorkflowDefError::InvalidDuration {
                            stage: stage_name.to_string(),
                            field: "timeout",
                            value,
                        })
                    })
                    .transpose()?,
                outcomes: outcomes
                    .into_iter()
                    .map(|o| PollOutcome {
                        pattern: o.pattern,
                        then: o.then,
                    })
                    .collect(),
            },
            RawStageKind::HumanGate { capture } => StageKind::HumanGate { capture },
            RawStageKind::Terminal => StageKind::Terminal,
        };

        Ok(StageDef {
            kind,
            on: self.on,
            loop_guard: self.loop_guard,
        })
    }
}

/// Resolves the `command:`/`script_file:` pair that `shell` and `poll`
/// stages both take: exactly one of the two, an inline shell line or a path
/// to an executable resolved relative to the definition file.
///
/// Shared rather than duplicated per kind so the two can't drift — a
/// `poll` that accepted a `script_file` the loader resolved differently
/// from `shell`'s would be a silent trap. The `…ShellCommand` error
/// variants are named for the *field pair*, not the `shell` kind, so they
/// read correctly for a `poll` stage too.
fn resolve_command(
    base_dir: &Path,
    stage_name: &str,
    command: Option<String>,
    script_file: Option<String>,
) -> Result<ShellCommand, WorkflowDefError> {
    match (command, script_file) {
        (Some(command), None) => Ok(ShellCommand::Inline(command)),
        (None, Some(script_file)) => Ok(ShellCommand::ScriptFile(resolve_file(
            base_dir,
            &script_file,
            RefOwner::Stage(stage_name),
            "script_file",
        )?)),
        (Some(_), Some(_)) => Err(WorkflowDefError::AmbiguousShellCommand {
            stage: stage_name.to_string(),
        }),
        (None, None) => Err(WorkflowDefError::MissingShellCommand {
            stage: stage_name.to_string(),
        }),
    }
}

/// Whether a stage keeps anything in `workflow_state.payload` — i.e. whether
/// `{{ stages.<this stage>.… }}` could ever resolve against it.
fn declares_capture(kind: &StageKind) -> bool {
    matches!(
        kind,
        StageKind::AgentTurn {
            capture: Some(_),
            ..
        } | StageKind::Shell {
            capture: Some(_),
            ..
        } | StageKind::Poll {
            capture: Some(_),
            ..
        } | StageKind::HumanGate {
            capture: Some(_),
            ..
        }
    )
}

/// The text a stage renders templates into (§5.1): an inline `command:`, or
/// an `agent_turn`'s `prompt_file` contents.
///
/// A `script_file` is deliberately absent — §5.1 scopes templating to
/// `command:` and `prompt_file`, and a script is an executable artifact in
/// its own right rather than a string the engine composes.
///
/// The prompt file is read here so its references are validated at load time
/// too. The engine re-reads it when the turn actually runs, so a file edited
/// in between isn't re-validated; that's the same staleness every
/// `prompt_file` already has and not worth a cache.
fn templatable_sources(
    stage_name: &str,
    stage: &StageDef,
) -> Result<Vec<(&'static str, String)>, WorkflowDefError> {
    let source = match &stage.kind {
        StageKind::Shell {
            command: ShellCommand::Inline(command),
            ..
        }
        | StageKind::Poll {
            command: ShellCommand::Inline(command),
            ..
        } => ("command", command.clone()),
        StageKind::AgentTurn {
            prompt_file: Some(path),
            ..
        } => (
            "prompt_file",
            // Not `WorkflowDefError::Io`, whose Display says "failed to read
            // workflow definition" — the definition read fine; it's a file it
            // points at that didn't, and the reader needs the stage and the
            // path to find it. Existence is already checked by `resolve_file`,
            // so what reaches here is a directory, a permissions problem, or
            // non-UTF-8 content.
            fs::read_to_string(path).map_err(|err| WorkflowDefError::UnreadableReferencedFile {
                owner: format!("stage '{stage_name}'"),
                field: "prompt_file",
                path: path.clone(),
                reason: err.to_string(),
            })?,
        ),
        _ => return Ok(Vec::new()),
    };
    Ok(vec![source])
}

#[derive(Clone, Copy)]
enum RefOwner<'a> {
    Role(&'a str),
    Stage(&'a str),
}

/// Thin wrapper over `fileref::resolve_relative` (the traversal guard
/// itself — reject absolute/`..` paths, then check existence — lives there
/// so `global_config.rs` can reuse it) that attaches this loader's own
/// error type and owner/field labels.
fn resolve_file(
    base_dir: &Path,
    relative: &str,
    owner: RefOwner<'_>,
    field: &'static str,
) -> Result<PathBuf, WorkflowDefError> {
    let owner_label = || match owner {
        RefOwner::Role(name) => format!("role '{name}'"),
        RefOwner::Stage(name) => format!("stage '{name}'"),
    };

    crate::fileref::resolve_relative(base_dir, relative).map_err(|err| match err {
        crate::fileref::FileRefError::Escapes => WorkflowDefError::InvalidFileReference {
            owner: owner_label(),
            field,
            value: relative.to_string(),
        },
        crate::fileref::FileRefError::Missing(path) => WorkflowDefError::MissingReferencedFile {
            owner: owner_label(),
            field,
            path,
        },
    })
}

/// Parses durations in the `<integer><unit>` shape used by §5.1's examples
/// (`30s`, `5m`, `1h`) — deliberately not pulling in a duration-parsing
/// crate for a three-suffix format this small.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let mut chars = s.chars();
    let unit = chars.next_back().ok_or_else(|| s.to_string())?;
    let digits = chars.as_str();
    let amount: u64 = digits.parse().map_err(|_| s.to_string())?;
    let multiplier: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return Err(s.to_string()),
    };
    let secs = amount
        .checked_mul(multiplier)
        .ok_or_else(|| s.to_string())?;
    // Zero is never what anyone meant: as a shell `timeout:` it elapses on
    // the first poll, so the command is killed before it can do anything;
    // as a poll `interval:` it's a busy loop. Rejecting it at load time
    // beats either behaviour at runtime.
    if secs == 0 {
        return Err(s.to_string());
    }
    Ok(Duration::from_secs(secs))
}

#[derive(Debug)]
pub enum WorkflowDefError {
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
    NoStages,
    UnknownRole {
        stage: String,
        role: String,
    },
    UnknownStageTarget {
        stage: String,
        target: String,
    },
    UnknownLoopGuardOutcome {
        stage: String,
        outcome: String,
    },
    UnknownLoopGuardTarget {
        stage: String,
        target: String,
    },
    NoReachableSink,
    MissingReferencedFile {
        owner: String,
        field: &'static str,
        path: PathBuf,
    },
    InvalidFileReference {
        owner: String,
        field: &'static str,
        value: String,
    },
    UnreadableReferencedFile {
        owner: String,
        field: &'static str,
        path: PathBuf,
        reason: String,
    },
    AmbiguousShellCommand {
        stage: String,
    },
    MissingShellCommand {
        stage: String,
    },
    MissingShellDoneOutcome {
        stage: String,
    },
    InvalidDuration {
        stage: String,
        field: &'static str,
        value: String,
    },
    UnknownPollOutcome {
        stage: String,
        outcome: String,
    },
    MissingTimeoutOutcome {
        stage: String,
    },
    InvalidPollPattern {
        stage: String,
        pattern: String,
        reason: String,
    },
    TerminalStageHasTransitions {
        stage: String,
    },
    CaptureOnOpenEndedTurn {
        stage: String,
    },
    HumanGateCaptureMustBeText {
        stage: String,
    },
    InvalidTemplate {
        stage: String,
        field: &'static str,
        reason: String,
    },
    UnknownTemplateStage {
        stage: String,
        field: &'static str,
        placeholder: String,
        referenced: String,
    },
    TemplateStageCapturesNothing {
        stage: String,
        field: &'static str,
        placeholder: String,
        referenced: String,
    },
}

impl fmt::Display for WorkflowDefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowDefError::Io(err) => write!(f, "failed to read workflow definition: {err}"),
            WorkflowDefError::Yaml(err) => write!(f, "failed to parse workflow definition: {err}"),
            WorkflowDefError::NoStages => write!(f, "workflow definition has no stages"),
            WorkflowDefError::UnknownRole { stage, role } => {
                write!(f, "stage '{stage}' references unknown role '{role}'")
            }
            WorkflowDefError::UnknownStageTarget { stage, target } => write!(
                f,
                "stage '{stage}' has an 'on:' transition to unknown stage '{target}'"
            ),
            WorkflowDefError::UnknownLoopGuardOutcome { stage, outcome } => write!(
                f,
                "stage '{stage}' has a loop_guard on outcome '{outcome}', which is not in its 'on:' map"
            ),
            WorkflowDefError::UnknownLoopGuardTarget { stage, target } => write!(
                f,
                "stage '{stage}' has a loop_guard 'then' target of unknown stage '{target}'"
            ),
            WorkflowDefError::NoReachableSink => write!(
                f,
                "no stage reachable from the workflow's start stage ever stops (every path keeps transitioning forever)"
            ),
            WorkflowDefError::MissingReferencedFile { owner, field, path } => write!(
                f,
                "{owner} references {field} '{}', which does not exist",
                path.display()
            ),
            WorkflowDefError::InvalidFileReference {
                owner,
                field,
                value,
            } => write!(
                f,
                "{owner} references {field} '{value}', which is an absolute path or escapes the workflow definition's directory"
            ),
            WorkflowDefError::UnreadableReferencedFile {
                owner,
                field,
                path,
                reason,
            } => write!(
                f,
                "{owner} references {field} '{}', which exists but could not be read: {reason}",
                path.display()
            ),
            WorkflowDefError::AmbiguousShellCommand { stage } => write!(
                f,
                "stage '{stage}' sets both 'command' and 'script_file'; only one is allowed"
            ),
            WorkflowDefError::MissingShellCommand { stage } => write!(
                f,
                "stage '{stage}' sets neither 'command' nor 'script_file'; exactly one is required"
            ),
            WorkflowDefError::MissingShellDoneOutcome { stage } => write!(
                f,
                "stage '{stage}' is a shell stage but has no 'done' key in its 'on:' map"
            ),
            WorkflowDefError::InvalidDuration {
                stage,
                field,
                value,
            } => write!(
                f,
                "stage '{stage}' has an invalid {field} '{value}' (expected e.g. '30s', '5m', '1h')"
            ),
            WorkflowDefError::UnknownPollOutcome { stage, outcome } => write!(
                f,
                "stage '{stage}' has a poll outcome '{outcome}', which is not in its 'on:' map"
            ),
            WorkflowDefError::MissingTimeoutOutcome { stage } => write!(
                f,
                "stage '{stage}' sets a poll 'timeout' but has no 'timeout' key in its 'on:' map"
            ),
            WorkflowDefError::InvalidPollPattern {
                stage,
                pattern,
                reason,
            } => write!(
                f,
                "stage '{stage}' has an invalid poll outcome pattern '{pattern}': {reason}"
            ),
            WorkflowDefError::TerminalStageHasTransitions { stage } => write!(
                f,
                "stage '{stage}' is a terminal stage but declares 'on:' transitions, which can never run"
            ),
            WorkflowDefError::CaptureOnOpenEndedTurn { stage } => write!(
                f,
                "agent_turn stage '{stage}' declares 'capture:' but has an empty 'on:' map, so it \
                 never concludes and the capture could never be taken"
            ),
            WorkflowDefError::HumanGateCaptureMustBeText { stage } => write!(
                f,
                "human_gate stage '{stage}' declares 'capture: json', but a human's reply is free \
                 text, not structured data — only 'capture: text' is supported"
            ),
            WorkflowDefError::InvalidTemplate {
                stage,
                field,
                reason,
            } => write!(
                f,
                "stage '{stage}' has an invalid {field} template: {reason}"
            ),
            WorkflowDefError::UnknownTemplateStage {
                stage,
                field,
                placeholder,
                referenced,
            } => write!(
                f,
                "stage '{stage}' has {placeholder} in its {field}, but '{referenced}' is not a \
                 stage in this workflow"
            ),
            WorkflowDefError::TemplateStageCapturesNothing {
                stage,
                field,
                placeholder,
                referenced,
            } => write!(
                f,
                "stage '{stage}' has {placeholder} in its {field}, but stage '{referenced}' \
                 declares no 'capture:' so it stores nothing to reference"
            ),
        }
    }
}

impl std::error::Error for WorkflowDefError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("chocofactoryd-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path.join(name);
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(contents.as_bytes()).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parses_the_built_in_chat_workflow() {
        let dir = TempDir::new();
        dir.write("chat-system.md", "You are a helpful assistant.");
        let yaml = r#"
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
    system_prompt_file: chat-system.md

stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        assert_eq!(def.name, "chat");
        assert_eq!(def.start_stage(), "chatting");
        assert_eq!(def.roles["chat"].cli.as_deref(), Some("claude"));
        assert!(def.roles["chat"].system_prompt_file.is_some());

        let StageKind::AgentTurn {
            role, prompt_file, ..
        } = &def.stages["chatting"].kind
        else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(role, "chat");
        assert!(prompt_file.is_none());
    }

    #[test]
    fn accepts_a_role_that_omits_cli_and_model() {
        // P1-8 LLD §2.4: a workflow-def role is the middle of three
        // resolution layers, so it must be allowed to leave cli/model
        // unset and fall through to global config instead of being
        // required to fully specify them.
        let dir = TempDir::new();
        let yaml = r#"
name: chat
roles:
  chat: {}

stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        assert_eq!(def.roles["chat"].cli, None);
        assert_eq!(def.roles["chat"].model, None);
    }

    fn coding_task_yaml() -> &'static str {
        r#"
name: coding-task
roles:
  coder:
    cli: claude
    model: sonnet
    system_prompt_file: coder-system.md
  reviewer:
    cli: claude
    model: sonnet
    system_prompt_file: reviewer-system.md

stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: internal_review }

  internal_review:
    kind: agent_turn
    role: reviewer
    prompt_file: reviewer-turn.md
    on:
      approved: open_pr
      changes_requested: coding
    loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }

  escalate_to_human:
    kind: human_gate
    on: { resumed: coding }

  open_pr:
    kind: shell
    command: "gh pr create --fill --json url,number"
    capture: json
    on: { done: checks_polling, error: escalate_to_human }

  checks_polling:
    kind: poll
    command: "gh pr checks 1 --json state -q '.[].state' | sort -u"
    interval: 30s
    timeout: 5m
    outcomes:
      - match: "^SUCCESS$"
        then: green
      - match: "FAILURE|ERROR"
        then: red
    on:
      green: awaiting_human_review
      red: coding
      timeout: awaiting_human_review

  awaiting_human_review:
    kind: poll
    command: "gh pr view 1 --json reviewDecision -q .reviewDecision"
    interval: 60s
    outcomes:
      - match: "APPROVED"
        then: approved
      - match: "CHANGES_REQUESTED"
        then: changes_requested
    on:
      approved: done
      changes_requested: coding

  done:
    kind: terminal
"#
    }

    fn write_coding_task_prompts(dir: &TempDir) {
        dir.write("coder-system.md", "coder system prompt");
        dir.write("reviewer-system.md", "reviewer system prompt");
        dir.write("coder-turn.md", "coder turn prompt");
        dir.write("reviewer-turn.md", "reviewer turn prompt");
    }

    #[test]
    fn parses_the_full_coding_task_workflow() {
        let dir = TempDir::new();
        write_coding_task_prompts(&dir);

        let def = WorkflowDefinition::parse(coding_task_yaml(), &dir.path).unwrap();
        assert_eq!(def.start_stage(), "coding");
        assert_eq!(def.stages.len(), 7);

        let StageKind::Poll {
            interval,
            timeout,
            outcomes,
            ..
        } = &def.stages["checks_polling"].kind
        else {
            panic!("expected poll stage");
        };
        assert_eq!(*interval, Duration::from_secs(30));
        assert_eq!(*timeout, Some(Duration::from_secs(300)));
        assert_eq!(outcomes.len(), 2);

        let guard = def.stages["internal_review"].loop_guard.as_ref().unwrap();
        assert_eq!(guard.on, "changes_requested");
        assert_eq!(guard.max, 3);
        assert_eq!(guard.then, "escalate_to_human");

        let StageKind::Shell {
            command,
            capture,
            timeout,
        } = &def.stages["open_pr"].kind
        else {
            panic!("expected shell stage");
        };
        assert!(matches!(command, ShellCommand::Inline(_)));
        assert_eq!(*capture, Some(Capture::Json));
        assert_eq!(*timeout, None);
    }

    #[test]
    fn resolves_prompt_files_relative_to_the_definition_dir() {
        let dir = TempDir::new();
        write_coding_task_prompts(&dir);

        let def = WorkflowDefinition::parse(coding_task_yaml(), &dir.path).unwrap();
        let StageKind::AgentTurn { prompt_file, .. } = &def.stages["coding"].kind else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(
            prompt_file.as_ref().unwrap(),
            &dir.path.join("coder-turn.md")
        );
    }

    #[test]
    fn rejects_an_on_transition_to_an_unknown_stage() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  only:
    kind: human_gate
    on: { done: nowhere }
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::UnknownStageTarget { stage, target }
                if stage == "only" && target == "nowhere"
        ));
    }

    #[test]
    fn rejects_an_agent_turn_stage_with_an_unknown_role() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  chatting:
    kind: agent_turn
    role: ghost
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::UnknownRole { stage, role }
                if stage == "chatting" && role == "ghost"
        ));
    }

    #[test]
    fn rejects_a_definition_that_never_reaches_a_stable_stage() {
        let dir = TempDir::new();
        let yaml = r#"
name: loops-forever
stages:
  a:
    kind: human_gate
    on: { resumed: b }
  b:
    kind: human_gate
    on: { resumed: a }
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(err, WorkflowDefError::NoReachableSink));
    }

    #[test]
    fn accepts_a_terminal_reachable_only_via_a_loop_guard_escape_hatch() {
        let dir = TempDir::new();
        let yaml = r#"
name: guarded
stages:
  a:
    kind: human_gate
    on: { resumed: a }
    loop_guard: { on: resumed, max: 3, then: done }
  done:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    #[test]
    fn rejects_a_loop_guard_on_an_outcome_absent_from_the_on_map() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  a:
    kind: human_gate
    on: { resumed: done }
    loop_guard: { on: changes_requested, max: 3, then: done }
  done:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::UnknownLoopGuardOutcome { stage, outcome }
                if stage == "a" && outcome == "changes_requested"
        ));
    }

    #[test]
    fn rejects_a_loop_guard_then_target_of_an_unknown_stage() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  a:
    kind: human_gate
    on: { resumed: a }
    loop_guard: { on: resumed, max: 3, then: nowhere }
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::UnknownLoopGuardTarget { stage, target }
                if stage == "a" && target == "nowhere"
        ));
    }

    #[test]
    fn rejects_a_missing_prompt_file() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  chatting:
    kind: agent_turn
    role: chat
    prompt_file: does-not-exist.md
    on: {}
roles:
  chat:
    cli: claude
    model: sonnet
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::MissingReferencedFile { field, .. } if field == "prompt_file"
        ));
    }

    #[test]
    fn rejects_a_shell_stage_missing_both_command_and_script_file() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::MissingShellCommand { stage } if stage == "run"
        ));
    }

    #[test]
    fn rejects_a_shell_stage_with_both_command_and_script_file() {
        let dir = TempDir::new();
        dir.write("deploy.sh", "#!/bin/sh\necho hi\n");
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    command: "echo hi"
    script_file: deploy.sh
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::AmbiguousShellCommand { stage } if stage == "run"
        ));
    }

    #[test]
    fn parses_a_shell_stage_with_capture_text_and_a_timeout() {
        let dir = TempDir::new();
        let yaml = r#"
name: shelly
stages:
  run:
    kind: shell
    command: "echo hi"
    capture: text
    timeout: 5m
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::Shell {
            capture, timeout, ..
        } = &def.stages["run"].kind
        else {
            panic!("expected shell stage");
        };
        assert_eq!(*capture, Some(Capture::Text));
        assert_eq!(*timeout, Some(Duration::from_secs(300)));
    }

    /// `command:`/`script_file:` resolution is shared with `shell` (P2-2),
    /// so a poll stage must reach the same three answers.
    #[test]
    fn parses_a_poll_stage_with_a_script_file_and_capture() {
        let dir = TempDir::new();
        dir.write("check.sh", "#!/bin/sh\necho SUCCESS\n");
        let yaml = r#"
name: pollster
stages:
  waiting:
    kind: poll
    script_file: check.sh
    capture: text
    interval: 30s
    outcomes:
      - match: "SUCCESS"
        then: green
    on: { green: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::Poll {
            command, capture, ..
        } = &def.stages["waiting"].kind
        else {
            panic!("expected poll stage");
        };
        assert_eq!(
            *command,
            ShellCommand::ScriptFile(dir.path.join("check.sh"))
        );
        assert_eq!(*capture, Some(Capture::Text));
    }

    #[test]
    fn parses_a_poll_stages_inline_command() {
        let dir = TempDir::new();
        let yaml = r#"
name: pollster
stages:
  waiting:
    kind: poll
    command: "gh pr checks 1"
    interval: 30s
    outcomes:
      - match: "SUCCESS"
        then: green
    on: { green: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::Poll {
            command, capture, ..
        } = &def.stages["waiting"].kind
        else {
            panic!("expected poll stage");
        };
        assert_eq!(*command, ShellCommand::Inline("gh pr checks 1".to_string()));
        assert_eq!(*capture, None);
    }

    #[test]
    fn rejects_a_poll_stage_missing_both_command_and_script_file() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    interval: 30s
    on: { green: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::MissingShellCommand { stage } if stage == "waiting"
        ));
    }

    #[test]
    fn rejects_a_poll_stage_with_both_command_and_script_file() {
        let dir = TempDir::new();
        dir.write("check.sh", "#!/bin/sh\necho SUCCESS\n");
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "echo SUCCESS"
    script_file: check.sh
    interval: 30s
    on: { green: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::AmbiguousShellCommand { stage } if stage == "waiting"
        ));
    }

    #[test]
    fn rejects_an_unsupported_capture_kind() {
        let dir = TempDir::new();
        let yaml = r#"
name: shelly
stages:
  run:
    kind: shell
    command: "echo hi"
    capture: yaml
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            err.to_string().contains("expected 'json' or 'text'"),
            "got {err}"
        );
    }

    /// A zero timeout would kill every command before it could run, and a
    /// zero poll interval is a busy loop — neither is ever intended.
    #[test]
    fn rejects_a_zero_duration() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    command: "true"
    timeout: 0s
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidDuration { stage, field, value }
                if stage == "run" && field == "timeout" && value == "0s"
        ));
    }

    #[test]
    fn rejects_an_invalid_shell_timeout() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    command: "true"
    timeout: eventually
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidDuration { stage, field, value }
                if stage == "run" && field == "timeout" && value == "eventually"
        ));
    }

    /// Without a `done` edge a successful command has nowhere to go, and
    /// the mistake would otherwise only surface at runtime — long after the
    /// definition was loaded — as a task silently parked mid-workflow.
    #[test]
    fn rejects_a_shell_stage_with_no_done_outcome() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    command: "true"
    on: { error: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::MissingShellDoneOutcome { stage } if stage == "run"
        ));
    }

    /// `error` stays optional, though: a workflow may deliberately want a
    /// failed command to park the task for a human.
    #[test]
    fn accepts_a_shell_stage_with_only_a_done_outcome() {
        let dir = TempDir::new();
        let yaml = r#"
name: fine
stages:
  run:
    kind: shell
    command: "true"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    #[test]
    fn rejects_an_invalid_duration() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: soon
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidDuration { stage, field, value }
                if stage == "waiting" && field == "interval" && value == "soon"
        ));
    }

    #[test]
    fn rejects_a_definition_with_no_stages() {
        let dir = TempDir::new();
        let yaml = "name: empty\nstages: {}\n";
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(err, WorkflowDefError::NoStages));
    }

    #[test]
    fn rejects_a_definition_with_a_duplicate_stage_key() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  done:
    kind: terminal
  done:
    kind: human_gate
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(err, WorkflowDefError::Yaml(_)));
        assert!(err.to_string().contains("duplicate key"));
    }

    #[test]
    fn rejects_a_definition_with_a_duplicate_role_key() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
roles:
  chat:
    cli: claude
    model: sonnet
  chat:
    cli: codex
    model: opus
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(err, WorkflowDefError::Yaml(_)));
        assert!(err.to_string().contains("duplicate key"));
    }

    #[test]
    fn rejects_a_stage_with_a_duplicate_on_outcome_key() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  a:
    kind: human_gate
    on:
      done: b
      done: c
  b:
    kind: terminal
  c:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(err, WorkflowDefError::Yaml(_)));
        assert!(err.to_string().contains("duplicate key"));
    }

    #[test]
    fn load_reads_from_disk_and_resolves_relative_to_the_file_location() {
        let dir = TempDir::new();
        write_coding_task_prompts(&dir);
        let def_path = dir.write("workflow.yaml", coding_task_yaml());

        let def = WorkflowDefinition::load(&def_path).unwrap();
        assert_eq!(def.name, "coding-task");
    }

    #[test]
    fn load_surfaces_io_errors_for_a_missing_definition_file() {
        let dir = TempDir::new();
        let err = WorkflowDefinition::load(&dir.path.join("nope.yaml")).unwrap_err();
        assert!(matches!(err, WorkflowDefError::Io(_)));
    }

    #[test]
    fn rejects_a_duration_with_a_non_ascii_unit_instead_of_panicking() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: "10°"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidDuration { stage, field, .. }
                if stage == "waiting" && field == "interval"
        ));
    }

    #[test]
    fn rejects_a_duration_that_would_overflow_instead_of_panicking() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: "9999999999999999h"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidDuration { stage, field, .. }
                if stage == "waiting" && field == "interval"
        ));
    }

    #[test]
    fn rejects_an_absolute_prompt_file_path() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    prompt_file: /etc/passwd
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidFileReference { field, .. } if field == "prompt_file"
        ));
    }

    #[test]
    fn rejects_a_prompt_file_path_that_escapes_the_definition_dir() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
roles:
  chat:
    cli: claude
    model: sonnet
stages:
  chatting:
    kind: agent_turn
    role: chat
    prompt_file: "../../../../etc/passwd"
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidFileReference { field, .. } if field == "prompt_file"
        ));
    }

    #[test]
    fn rejects_a_role_system_prompt_file_path_that_escapes_the_definition_dir() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
roles:
  chat:
    cli: claude
    model: sonnet
    system_prompt_file: "../../../../etc/passwd"
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidFileReference { field, .. } if field == "system_prompt_file"
        ));
    }

    #[test]
    fn rejects_a_script_file_path_that_escapes_the_definition_dir() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  run:
    kind: shell
    script_file: "../../../../etc/passwd"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidFileReference { field, .. } if field == "script_file"
        ));
    }

    #[test]
    fn rejects_a_terminal_stage_with_on_transitions() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  done:
    kind: terminal
    on: { resumed: done }
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::TerminalStageHasTransitions { stage } if stage == "done"
        ));
    }

    #[test]
    fn rejects_a_poll_outcome_not_present_in_the_on_map() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: 30s
    outcomes:
      - match: "^SUCCESS$"
        then: succeeded
    on: { success: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::UnknownPollOutcome { stage, outcome }
                if stage == "waiting" && outcome == "succeeded"
        ));
    }

    #[test]
    fn rejects_a_poll_timeout_with_no_timeout_key_in_on_map() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: 30s
    timeout: 5m
    outcomes:
      - match: "^SUCCESS$"
        then: success
    on: { success: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::MissingTimeoutOutcome { stage } if stage == "waiting"
        ));
    }

    #[test]
    fn rejects_an_invalid_poll_pattern_regex() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  waiting:
    kind: poll
    command: "true"
    interval: 30s
    outcomes:
      - match: "("
        then: success
    on: { success: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::InvalidPollPattern { stage, pattern, .. }
                if stage == "waiting" && pattern == "("
        ));
    }

    /// The loader gap #45 closes: `capture:` under an `agent_turn` used to be
    /// swallowed by the flattened, internally-tagged `RawStageKind` rather
    /// than parsed or rejected.
    #[test]
    fn parses_capture_on_an_agent_turn() {
        let dir = TempDir::new();
        let yaml = r#"
name: reviewed
roles:
  reviewer: { cli: claude }
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::AgentTurn { capture, .. } = &def.stages["review"].kind else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(*capture, Some(Capture::Json));
    }

    #[test]
    fn an_agent_turn_without_capture_parses_as_none() {
        let dir = TempDir::new();
        let yaml = r#"
name: chat
roles:
  chat: { cli: claude }
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::AgentTurn { capture, .. } = &def.stages["chatting"].kind else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(*capture, None);
    }

    /// An open-ended turn never concludes, so no watcher runs and the capture
    /// could never be taken — dead config, rejected rather than ignored.
    #[test]
    fn rejects_capture_on_an_open_ended_agent_turn() {
        let dir = TempDir::new();
        let yaml = r#"
name: chat
roles:
  chat: { cli: claude }
stages:
  chatting:
    kind: agent_turn
    role: chat
    capture: json
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::CaptureOnOpenEndedTurn { stage } if stage == "chatting"),
            "got {err}"
        );
    }

    #[test]
    fn accepts_a_template_reference_to_a_capturing_stage() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  open_pr:
    kind: shell
    command: "gh pr create --fill --json url,number"
    capture: json
    on: { done: report }
  report:
    kind: shell
    command: "echo pr {{ stages.open_pr.number }} at {{ stages.open_pr.url }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    #[test]
    fn parses_capture_text_on_a_human_gate() {
        let dir = TempDir::new();
        let yaml = r#"
name: gated
stages:
  gate:
    kind: human_gate
    capture: text
    on: { resumed: done }
  done:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::HumanGate { capture } = &def.stages["gate"].kind else {
            panic!("expected human_gate stage");
        };
        assert_eq!(*capture, Some(Capture::Text));
    }

    #[test]
    fn rejects_capture_json_on_a_human_gate() {
        let dir = TempDir::new();
        let yaml = r#"
name: gated
stages:
  gate:
    kind: human_gate
    capture: json
    on: { resumed: done }
  done:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::HumanGateCaptureMustBeText { stage } if stage == "gate"),
            "got {err}"
        );
    }

    #[test]
    fn accepts_a_template_reference_to_a_capturing_human_gate() {
        let dir = TempDir::new();
        let yaml = r#"
name: gated
stages:
  gate:
    kind: human_gate
    capture: text
    on: { resumed: coding }
  coding:
    kind: shell
    command: "echo {{ stages.gate }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    #[test]
    fn accepts_a_template_reference_in_a_prompt_file() {
        let dir = TempDir::new();
        dir.write("coder.md", "Address: {{ stages.review.comments }}\n");
        let yaml = r#"
name: templated
roles:
  coder: { cli: claude }
  reviewer: { cli: claude }
stages:
  review:
    kind: agent_turn
    role: reviewer
    capture: json
    on: { changes_requested: coding }
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder.md
    on: {}
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// The same reasoning as `MissingShellDoneOutcome`: a mistyped stage name
    /// is a typo every time, and left to run time it only surfaces as a
    /// parked task long after the definition was loaded.
    #[test]
    fn rejects_a_template_reference_to_an_unknown_stage() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  report:
    kind: shell
    command: "echo {{ stages.open_pr.number }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::UnknownTemplateStage { stage, referenced, .. }
                    if stage == "report" && referenced == "open_pr"
            ),
            "got {err}"
        );
    }

    #[test]
    fn rejects_a_template_reference_to_a_stage_that_captures_nothing() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  open_pr:
    kind: shell
    command: "gh pr create --fill"
    on: { done: report }
  report:
    kind: shell
    command: "echo {{ stages.open_pr.number }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::TemplateStageCapturesNothing { stage, referenced, .. }
                    if stage == "report" && referenced == "open_pr"
            ),
            "got {err}"
        );
    }

    #[test]
    fn rejects_malformed_template_syntax() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  report:
    kind: shell
    command: "echo {{ stages.open_pr.number"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::InvalidTemplate { stage, field, .. }
                    if stage == "report" && *field == "command"
            ),
            "got {err}"
        );
    }

    #[test]
    fn rejects_a_template_reading_an_unknown_namespace() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  report:
    kind: shell
    command: "echo {{ bogus.id }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::InvalidTemplate { stage, .. } if stage == "report"),
            "got {err}"
        );
    }

    /// P2-7a: `task` is always a valid root — it names no stage, so it
    /// skips both `UnknownTemplateStage` and `TemplateStageCapturesNothing`
    /// entirely, unlike every `stages.<stage>` reference.
    #[test]
    fn accepts_a_template_reading_the_task_root() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  report:
    kind: shell
    command: "echo {{ task.input }} {{ task.title }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// Belt-and-suspenders on `Root::Task => continue`: load-time validation
    /// doesn't inspect the field path at all for a `task` reference, so even
    /// a field that's never `input`/`title` — which would fail at render
    /// time (P2-7a) — is accepted here.
    #[test]
    fn accepts_an_unknown_task_field_at_load_time() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  report:
    kind: shell
    command: "echo {{ task.nonexistent_field }}"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// A `poll` command is templated on the same terms as a `shell` one —
    /// §5.1's own example polls `gh pr checks {{ stages.open_pr.number }}`.
    #[test]
    fn validates_template_references_in_a_poll_command() {
        let dir = TempDir::new();
        let yaml = r#"
name: templated
stages:
  checks:
    kind: poll
    command: "gh pr checks {{ stages.nope.number }}"
    interval: 30s
    outcomes:
      - match: "SUCCESS"
        then: green
    on: { green: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::UnknownTemplateStage { referenced, .. } if referenced == "nope"
            ),
            "got {err}"
        );
    }

    /// A `script_file` is an executable in its own right, not a string the
    /// engine composes, so §5.1 leaves its contents alone — including
    /// anything that merely looks like a placeholder.
    #[test]
    fn does_not_template_a_script_file() {
        let dir = TempDir::new();
        dir.write("run.sh", "#!/bin/sh\necho '{{ stages.nope.field }}'\n");
        let yaml = r#"
name: scripted
stages:
  run:
    kind: shell
    script_file: run.sh
    on: { done: finished }
  finished:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }
}
