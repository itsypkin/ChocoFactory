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
                    return Err(WorkflowDefError::UnknownStageTarget {
                        stage: stage_name.clone(),
                        target: guard.then.clone(),
                    });
                }
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
        }

        if !self.sink_reachable_from_start() {
            return Err(WorkflowDefError::NoReachableSink);
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

#[derive(Debug, Clone, PartialEq)]
pub struct RoleDef {
    pub cli: String,
    pub model: String,
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
    },
    Shell {
        command: ShellCommand,
        capture: Option<Capture>,
    },
    Poll {
        command: String,
        interval: Duration,
        timeout: Option<Duration>,
        outcomes: Vec<PollOutcome>,
    },
    HumanGate,
    Terminal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShellCommand {
    Inline(String),
    ScriptFile(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Json,
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
    #[serde(default)]
    roles: HashMap<String, RawRole>,
    stages: IndexMap<String, RawStage>,
}

#[derive(Debug, Deserialize)]
struct RawRole {
    cli: String,
    model: String,
    #[serde(default)]
    system_prompt_file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStage {
    #[serde(flatten)]
    kind: RawStageKind,
    #[serde(default)]
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
    },
    Shell {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        script_file: Option<String>,
        #[serde(default)]
        capture: Option<Capture>,
    },
    Poll {
        command: String,
        interval: String,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        outcomes: Vec<RawPollOutcome>,
    },
    HumanGate,
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
            other => Err(serde::de::Error::custom(format!(
                "unsupported capture kind '{other}' (expected 'json')"
            ))),
        }
    }
}

impl RawStage {
    fn resolve(self, base_dir: &Path, stage_name: &str) -> Result<StageDef, WorkflowDefError> {
        let kind = match self.kind {
            RawStageKind::AgentTurn { role, prompt_file } => StageKind::AgentTurn {
                role,
                prompt_file: prompt_file
                    .map(|rel| {
                        resolve_file(base_dir, &rel, RefOwner::Stage(stage_name), "prompt_file")
                    })
                    .transpose()?,
            },
            RawStageKind::Shell {
                command,
                script_file,
                capture,
            } => {
                let resolved_command = match (command, script_file) {
                    (Some(command), None) => ShellCommand::Inline(command),
                    (None, Some(script_file)) => ShellCommand::ScriptFile(resolve_file(
                        base_dir,
                        &script_file,
                        RefOwner::Stage(stage_name),
                        "script_file",
                    )?),
                    (Some(_), Some(_)) => {
                        return Err(WorkflowDefError::AmbiguousShellCommand {
                            stage: stage_name.to_string(),
                        });
                    }
                    (None, None) => {
                        return Err(WorkflowDefError::MissingShellCommand {
                            stage: stage_name.to_string(),
                        });
                    }
                };
                StageKind::Shell {
                    command: resolved_command,
                    capture,
                }
            }
            RawStageKind::Poll {
                command,
                interval,
                timeout,
                outcomes,
            } => StageKind::Poll {
                command,
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
            RawStageKind::HumanGate => StageKind::HumanGate,
            RawStageKind::Terminal => StageKind::Terminal,
        };

        Ok(StageDef {
            kind,
            on: self.on,
            loop_guard: self.loop_guard,
        })
    }
}

#[derive(Clone, Copy)]
enum RefOwner<'a> {
    Role(&'a str),
    Stage(&'a str),
}

/// Joins `relative` onto `base_dir` and fails fast if the result doesn't
/// exist on disk — a typo'd prompt/script path is exactly the kind of
/// malformed definition this loader should catch at load time rather than
/// leaving to fail deep inside a running task.
///
/// Rejects absolute paths and `..` components up front: without this, a
/// `prompt_file`/`system_prompt_file`/`script_file` value could walk
/// straight out of the definition's directory (e.g. `/etc/passwd` or
/// `../../../../etc/passwd`), contradicting "resolved relative to the
/// definition dir" and letting the daemon read/execute arbitrary files a
/// workflow author (or generator) points it at.
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

    let rel_path = Path::new(relative);
    let escapes = rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes {
        return Err(WorkflowDefError::InvalidFileReference {
            owner: owner_label(),
            field,
            value: relative.to_string(),
        });
    }

    let resolved = base_dir.join(rel_path);
    if !resolved.is_file() {
        return Err(WorkflowDefError::MissingReferencedFile {
            owner: owner_label(),
            field,
            path: resolved,
        });
    }
    Ok(resolved)
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
    AmbiguousShellCommand {
        stage: String,
    },
    MissingShellCommand {
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
            WorkflowDefError::AmbiguousShellCommand { stage } => write!(
                f,
                "stage '{stage}' sets both 'command' and 'script_file'; only one is allowed"
            ),
            WorkflowDefError::MissingShellCommand { stage } => write!(
                f,
                "stage '{stage}' is a shell stage but sets neither 'command' nor 'script_file'"
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
                std::env::temp_dir().join(format!("chokofactoryd-test-{}", uuid::Uuid::new_v4()));
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
        assert_eq!(def.roles["chat"].cli, "claude");
        assert!(def.roles["chat"].system_prompt_file.is_some());

        let StageKind::AgentTurn { role, prompt_file } = &def.stages["chatting"].kind else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(role, "chat");
        assert!(prompt_file.is_none());
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

        let StageKind::Shell { command, capture } = &def.stages["open_pr"].kind else {
            panic!("expected shell stage");
        };
        assert!(matches!(command, ShellCommand::Inline(_)));
        assert_eq!(*capture, Some(Capture::Json));
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
    kind: terminal
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
}
