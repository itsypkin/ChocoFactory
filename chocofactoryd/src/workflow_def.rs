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

use chocofactory_core::duration::parse_duration;

use crate::adapter::{Isolation, RoleTool};

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

        let worktree = raw.worktree;
        let roles = raw
            .roles
            .into_iter()
            .map(|(name, role)| -> Result<_, WorkflowDefError> {
                let isolation = role.isolation(&name)?;
                let disallowed_tools = role.disallowed_tools(&name)?;
                if role.read_only {
                    let missing: Vec<&str> = RoleTool::ALL
                        .iter()
                        .filter(|t| !disallowed_tools.contains(t))
                        .map(|t| t.name())
                        .collect();
                    if !missing.is_empty() {
                        return Err(WorkflowDefError::ReadOnlyRoleMissingTools {
                            role: name,
                            missing: missing.join(", "),
                        });
                    }
                    if !worktree {
                        return Err(WorkflowDefError::ReadOnlyRoleWithoutWorktree { role: name });
                    }
                }
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
                        isolation,
                        disallowed_tools,
                        read_only: role.read_only,
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

            if let Some(watch) = stage.watch() {
                validate_watch(stage_name, stage, watch)?;
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

            // #95. Each name is matched against the report's headings, so
            // a blank one would match every line and a duplicate would ask
            // twice for the same walk. Neither is anything an author meant,
            // and both would only show up as a reviewer being rejected for
            // a section it did write.
            if let StageKind::AgentTurn {
                report_sections, ..
            } = &stage.kind
            {
                // An open-ended turn (chat's shape) never concludes, so its
                // report is optional and purely informational — requiring
                // sections of it is dead config, rejected for the same
                // reason `capture:` is just above.
                if !report_sections.is_empty() && stage.on.is_empty() {
                    return Err(WorkflowDefError::ReportSectionsOnOpenEndedTurn {
                        stage: stage_name.clone(),
                    });
                }
                let mut seen: Vec<String> = Vec::new();
                for section in report_sections {
                    // Normalized, not merely trimmed (review of #95): a
                    // name like "##" or "1." is all decoration to the
                    // tool, leaving a section whose heading no report can
                    // ever carry.
                    let key = chocofactory_core::mcp::normalize_report_heading(section);
                    if key.is_empty() {
                        return Err(WorkflowDefError::EmptyReportSection {
                            stage: stage_name.clone(),
                        });
                    }
                    // Compared with the tool's own normalization, not a
                    // lookalike of it (review of #95): `Branches → tests`
                    // and `Branches -> tests` differ here but are one name
                    // there, and a stage declaring both would require a
                    // section whose heading can only ever be credited to
                    // one of them — rejecting every review it ever runs,
                    // for a section the reviewer did write.
                    if seen.contains(&key) {
                        return Err(WorkflowDefError::DuplicateReportSection {
                            stage: stage_name.clone(),
                            section: section.clone(),
                        });
                    }
                    seen.push(key);
                }
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
                ..
            } = &stage.kind
            {
                return Err(WorkflowDefError::HumanGateCaptureMustBeText {
                    stage: stage_name.clone(),
                });
            }

            if let StageKind::HumanGate { markers, .. } = &stage.kind {
                validate_markers(stage_name, stage, markers)?;
            }

            if let StageKind::Shell { env, .. } = &stage.kind {
                validate_env_names(stage_name, env)?;
            }

            self.validate_templates(stage_name, stage)?;
        }

        // A second pass, run only once every stage's own checks above have
        // passed — so every `on:` target and every `loop_guard.then` is
        // already known to name a real stage, and this can walk the graph
        // without re-deriving that. #106: a `loop_guard` whose `then:`
        // stage sits on every path back from the guarded outcome's target
        // to the guarded stage itself would have its count reset every lap
        // (workflow_state.loop_counters clears on arrival at `then:`), so
        // it could never accumulate past `max` and trip.
        for (stage_name, stage) in &self.stages {
            let Some(guard) = &stage.loop_guard else {
                continue;
            };
            // Already validated above: `guard.on` is a key of `stage.on`,
            // and `guard.then` names a real stage.
            let target = &stage.on[&guard.on];
            let escapes_every_lap = self.reaches(target, stage_name, None)
                && !self.reaches(target, stage_name, Some(&guard.then));
            if escapes_every_lap {
                return Err(WorkflowDefError::LoopGuardEscapeOnEveryLap {
                    stage: stage_name.clone(),
                    then: guard.then.clone(),
                    target: target.clone(),
                });
            }
        }

        if !self.sink_reachable_from_start() {
            return Err(WorkflowDefError::NoReachableSink);
        }

        Ok(())
    }

    /// DFS over `on:` edges only (never `loop_guard.then`, unlike
    /// [`Self::sink_reachable_from_start`] — that edge is exactly what's
    /// being asked about here, not a path to walk through). A zero-length
    /// path counts: `reaches(x, x, None)` is `true`. `avoiding`, when set,
    /// is never entered — including as `from` itself, so starting *on*
    /// the avoided stage never reaches anything.
    fn reaches(&self, from: &str, to: &str, avoiding: Option<&str>) -> bool {
        if Some(from) == avoiding {
            return false;
        }
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(name) = stack.pop() {
            if name == to {
                return true;
            }
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some(stage) = self.stages.get(&name) else {
                continue;
            };
            for target in stage.on.values() {
                if Some(target.as_str()) != avoiding {
                    stack.push(target.clone());
                }
            }
        }
        false
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
                    field: field.clone(),
                    reason: err.to_string(),
                }
            })?;

            for reference in references {
                // `task` is always valid — it's payload the engine seeds
                // itself in `start_task` (P2-7a), not a stage's `capture:`,
                // so there's no stage to look up and no capture to require.
                // Same for `arrival` (#112): it's engine-owned payload set
                // in `advance_from_stage`/`start_task`, not a stage's own
                // capture, and `template::parse_reference` has already
                // rejected any field but `from`/`outcome` at parse time.
                let referenced_stage = match reference.root {
                    crate::template::Root::Task => continue,
                    crate::template::Root::Arrival => continue,
                    crate::template::Root::Stage(stage) => stage,
                };
                let Some(target) = self.stages.get(&referenced_stage) else {
                    return Err(WorkflowDefError::UnknownTemplateStage {
                        stage: stage_name.to_string(),
                        field: field.clone(),
                        placeholder: reference.placeholder,
                        referenced: referenced_stage,
                    });
                };
                if !declares_capture(&target.kind) {
                    return Err(WorkflowDefError::TemplateStageCapturesNothing {
                        stage: stage_name.to_string(),
                        field: field.clone(),
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
    /// What this role's turns inherit from the operator's own CLI setup
    /// (#90). Unlike `cli`/`model`/the system prompt this is *not* one of
    /// the three resolution layers: only a workflow definition can set it,
    /// never task-level config or global config, because every setting it
    /// has loosens what an agent is exposed to.
    pub isolation: Isolation,
    /// Tools this role may not use (#172), adapter-neutral, deduplicated in
    /// order of first appearance. Like `isolation`, only a workflow
    /// definition can set it: a `roles.<name>.disallowed_tools` key in task
    /// config is ignored (as `inherit_operator_config` is), so a task creator
    /// cannot loosen enforcement.
    pub disallowed_tools: Vec<RoleTool>,
    /// The role must not change the task's worktree (#172): the engine
    /// snapshots it before the turn and parks the task as stuck if HEAD, the
    /// branch or `git status` differ afterwards. Workflow-definition only,
    /// like `disallowed_tools`; task-level keys are ignored. Loader rules: it
    /// requires all of `RoleTool::ALL` in `disallowed_tools` and a
    /// `worktree: true` workflow.
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageDef {
    pub kind: StageKind,
    /// Outcome name -> next stage name.
    pub on: IndexMap<String, String>,
    pub loop_guard: Option<LoopGuard>,
}

impl StageDef {
    /// The stage's watcher: every `poll`, and a `human_gate` that has one.
    pub fn watch(&self) -> Option<&Watch> {
        match &self.kind {
            StageKind::Poll { watch, .. } => Some(watch),
            StageKind::HumanGate { watch, .. } => watch.as_ref(),
            _ => None,
        }
    }
}

impl StageKind {
    /// The kind's name as YAML spells it.
    pub fn name(&self) -> &'static str {
        match self {
            StageKind::AgentTurn { .. } => "agent_turn",
            StageKind::Shell { .. } => "shell",
            StageKind::Poll { .. } => "poll",
            StageKind::HumanGate { .. } => "human_gate",
            StageKind::Terminal => "terminal",
        }
    }
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
        /// Sections this stage requires the turn's `report_outcome` summary
        /// to carry, in order (#95). The `report_outcome` tool rejects a
        /// report that leaves one out, which is what stops a reviewer
        /// reporting the moment it has enough to reject: there is no way to
        /// file a verdict without also filing the walks behind it.
        ///
        /// Empty (the default, and every stage that hasn't opted in) leaves
        /// the report checked only for its `outcome`, exactly as before.
        report_sections: Vec<String>,
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
        /// Environment variables for the command, in declaration order (#101).
        /// Values are templates, rendered once on stage entry and handed to
        /// the child as environment variables — never parsed by a shell.
        env: IndexMap<String, String>,
    },
    Poll {
        capture: Option<Capture>,
        watch: Watch,
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
        /// Verdict lines a reply through choco must carry (#175). Empty
        /// means the gate takes any reply and resumes on `resumed`.
        markers: Vec<ReplyMarker>,
        /// A watcher that runs while the gate waits, the same loop a `poll`
        /// stage runs (#175).
        watch: Option<Watch>,
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

/// The watcher loop a `poll` stage is, and a `human_gate` may run (#175).
#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub command: ShellCommand,
    /// As `Shell`'s field of the same name.
    pub env: IndexMap<String, String>,
    /// How long to wait between the end of one attempt and the start
    /// of the next.
    pub interval: Duration,
    /// How long to keep polling before giving up and emitting
    /// `timeout`. Unlike `Shell`'s field of the same name this is a
    /// budget for the whole loop rather than a per-command kill —
    /// though it doubles as the latter, since each attempt is capped
    /// at whatever is left of it.
    pub timeout: Option<Duration>,
    pub outcomes: Vec<PollOutcome>,
}

/// A line a reply to a gate must carry, and the outcome it chooses (#175).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyMarker {
    pub line: String,
    pub then: String,
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

/// `deny_unknown_fields`: a misspelled `read_only` (say `readonly`) must fail
/// the load, not silently leave a role unprotected (#172).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    #[serde(default)]
    cli: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
    /// Run this role's turns with the operator's full CLI setup (#90).
    #[serde(default)]
    inherit_operator_config: bool,
    /// Skills an isolated role may invoke (#90). Absent means none.
    #[serde(default)]
    skills: Option<Vec<String>>,
    /// Whether an isolated role may use auto-memory (#90). Absent means no.
    #[serde(default)]
    memory: Option<bool>,
    /// Neutral tool names this role may not use (#172).
    #[serde(default)]
    disallowed_tools: Vec<String>,
    /// The role must not change the task's worktree (#172).
    #[serde(default)]
    read_only: bool,
}

impl RawRole {
    fn disallowed_tools(&self, role: &str) -> Result<Vec<RoleTool>, WorkflowDefError> {
        let mut tools = Vec::new();
        for name in &self.disallowed_tools {
            let tool =
                RoleTool::from_name(name).ok_or_else(|| WorkflowDefError::UnknownRoleTool {
                    role: role.to_string(),
                    tool: name.clone(),
                })?;
            if !tools.contains(&tool) {
                tools.push(tool);
            }
        }
        Ok(tools)
    }

    /// `skills`/`memory` only describe an *isolated* role. Next to
    /// `inherit_operator_config: true` — where every skill and the memory are
    /// already available — either would be silently meaningless, and a role
    /// author who wrote `skills: []` expecting it to restrict something would
    /// never find out it didn't. Rejected instead.
    fn isolation(&self, role: &str) -> Result<Isolation, WorkflowDefError> {
        if self.inherit_operator_config {
            for (field, set) in [
                ("skills", self.skills.is_some()),
                ("memory", self.memory.is_some()),
            ] {
                if set {
                    return Err(WorkflowDefError::IsolationFieldWithInheritedConfig {
                        role: role.to_string(),
                        field,
                    });
                }
            }
            return Ok(Isolation::InheritOperatorConfig);
        }
        Ok(Isolation::Isolated {
            skills: self.skills.clone().unwrap_or_default(),
            memory: self.memory.unwrap_or(false),
        })
    }
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
        #[serde(default)]
        report_sections: Vec<String>,
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
        #[serde(
            default,
            deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
        )]
        env: IndexMap<String, String>,
    },
    Poll {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        script_file: Option<String>,
        #[serde(default)]
        capture: Option<Capture>,
        #[serde(
            default,
            deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
        )]
        env: IndexMap<String, String>,
        interval: String,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        outcomes: Vec<RawPollOutcome>,
    },
    HumanGate {
        #[serde(default)]
        capture: Option<Capture>,
        #[serde(default)]
        markers: Option<Vec<RawReplyMarker>>,
        #[serde(default)]
        watch: Option<RawWatch>,
    },
    Terminal,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatch {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    script_file: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_map_rejecting_duplicate_keys"
    )]
    env: IndexMap<String, String>,
    interval: String,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    outcomes: Vec<RawPollOutcome>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReplyMarker {
    line: String,
    then: String,
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
                report_sections,
            } => StageKind::AgentTurn {
                role,
                prompt_file: prompt_file
                    .map(|rel| {
                        resolve_file(base_dir, &rel, RefOwner::Stage(stage_name), "prompt_file")
                    })
                    .transpose()?,
                capture,
                report_sections,
            },
            RawStageKind::Shell {
                command,
                script_file,
                capture,
                timeout,
                env,
            } => {
                let resolved_command = resolve_command(base_dir, stage_name, command, script_file)?;
                StageKind::Shell {
                    command: resolved_command,
                    capture,
                    env,
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
                env,
            } => StageKind::Poll {
                capture,
                watch: resolve_watch(
                    base_dir,
                    stage_name,
                    WatchFields {
                        command,
                        script_file,
                        env,
                        interval,
                        timeout,
                        outcomes,
                    },
                    ("interval", "timeout"),
                )?,
            },
            RawStageKind::HumanGate {
                capture,
                markers,
                watch,
            } => StageKind::HumanGate {
                capture,
                markers: match markers {
                    None => Vec::new(),
                    // An explicit `markers: []` can't be told from "no markers"
                    // once resolved, so it is rejected here.
                    Some(list) if list.is_empty() => {
                        return Err(WorkflowDefError::EmptyReplyMarkers {
                            stage: stage_name.to_string(),
                        });
                    }
                    Some(list) => list
                        .into_iter()
                        .map(|m| ReplyMarker {
                            line: m.line,
                            then: m.then,
                        })
                        .collect(),
                },
                watch: watch
                    .map(|w| {
                        resolve_watch(
                            base_dir,
                            stage_name,
                            WatchFields {
                                command: w.command,
                                script_file: w.script_file,
                                env: w.env,
                                interval: w.interval,
                                timeout: w.timeout,
                                outcomes: w.outcomes,
                            },
                            ("watch.interval", "watch.timeout"),
                        )
                    })
                    .transpose()?,
            },
            RawStageKind::Terminal => StageKind::Terminal,
        };

        Ok(StageDef {
            kind,
            on: self.on,
            loop_guard: self.loop_guard,
        })
    }
}

struct WatchFields {
    command: Option<String>,
    script_file: Option<String>,
    env: IndexMap<String, String>,
    interval: String,
    timeout: Option<String>,
    outcomes: Vec<RawPollOutcome>,
}

/// Every check on a watcher's fields, for a `poll` and for a gate's `watch:`.
fn validate_watch(
    stage_name: &str,
    stage: &StageDef,
    watch: &Watch,
) -> Result<(), WorkflowDefError> {
    for outcome in &watch.outcomes {
        if !stage.on.contains_key(&outcome.then) {
            return Err(WorkflowDefError::UnknownPollOutcome {
                stage: stage_name.to_string(),
                outcome: outcome.then.clone(),
            });
        }
        if let Err(reason) = Regex::new(&outcome.pattern) {
            return Err(WorkflowDefError::InvalidPollPattern {
                stage: stage_name.to_string(),
                pattern: outcome.pattern.clone(),
                reason: reason.to_string(),
            });
        }
    }
    if watch.timeout.is_some() && !stage.on.contains_key("timeout") {
        return Err(WorkflowDefError::MissingTimeoutOutcome {
            stage: stage_name.to_string(),
        });
    }
    validate_env_names(stage_name, &watch.env)
}

fn validate_env_names(
    stage_name: &str,
    env: &IndexMap<String, String>,
) -> Result<(), WorkflowDefError> {
    for name in env.keys() {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(WorkflowDefError::InvalidEnvName {
                stage: stage_name.to_string(),
                name: name.clone(),
            });
        }
        if name.to_ascii_lowercase().starts_with("choco_") {
            return Err(WorkflowDefError::ReservedEnvName {
                stage: stage_name.to_string(),
                name: name.clone(),
            });
        }
    }
    Ok(())
}

/// Checks a gate's `markers:` (#175). An empty list is rejected earlier, at
/// resolve time, where it can still be told from an absent one.
fn validate_markers(
    stage_name: &str,
    stage: &StageDef,
    markers: &[ReplyMarker],
) -> Result<(), WorkflowDefError> {
    let mut seen: Vec<&str> = Vec::new();
    for marker in markers {
        let line = marker.line.as_str();
        let stage_name = stage_name.to_string();
        if line.is_empty() {
            return Err(WorkflowDefError::EmptyReplyMarkerLine { stage: stage_name });
        }
        if line != line.trim() {
            return Err(WorkflowDefError::ReplyMarkerLineHasSurroundingWhitespace {
                stage: stage_name,
                line: marker.line.clone(),
            });
        }
        if line.contains('\n') || line.contains('\r') {
            return Err(WorkflowDefError::ReplyMarkerLineHasNewline {
                stage: stage_name,
                line: marker.line.clone(),
            });
        }
        if seen.contains(&line) {
            return Err(WorkflowDefError::DuplicateReplyMarker {
                stage: stage_name,
                line: marker.line.clone(),
            });
        }
        seen.push(line);
        if !stage.on.contains_key(&marker.then) {
            return Err(WorkflowDefError::UnknownReplyMarkerOutcome {
                stage: stage_name,
                outcome: marker.then.clone(),
            });
        }
    }
    Ok(())
}

/// Resolves a watcher's raw fields. `fields` names the duration fields in
/// `InvalidDuration` errors: `interval`/`timeout` for a poll's flat keys,
/// `watch.interval`/`watch.timeout` for a gate's map.
fn resolve_watch(
    base_dir: &Path,
    stage_name: &str,
    raw: WatchFields,
    fields: (&'static str, &'static str),
) -> Result<Watch, WorkflowDefError> {
    let command = resolve_command(base_dir, stage_name, raw.command, raw.script_file)?;
    let interval =
        parse_duration(&raw.interval).map_err(|value| WorkflowDefError::InvalidDuration {
            stage: stage_name.to_string(),
            field: fields.0,
            value,
        })?;
    let timeout = raw
        .timeout
        .map(|value| {
            parse_duration(&value).map_err(|value| WorkflowDefError::InvalidDuration {
                stage: stage_name.to_string(),
                field: fields.1,
                value,
            })
        })
        .transpose()?;
    Ok(Watch {
        command,
        env: raw.env,
        interval,
        timeout,
        outcomes: raw
            .outcomes
            .into_iter()
            .map(|o| PollOutcome {
                pattern: o.pattern,
                then: o.then,
            })
            .collect(),
    })
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

/// The text a stage renders templates into (§5.1): an inline `command:`, an
/// `agent_turn`'s `prompt_file` contents, and the values of a `shell`/`poll`
/// stage's `env:` map.
///
/// A `script_file` is deliberately absent — a script is an executable
/// artifact in its own right rather than a string the engine composes. Its
/// `env:` values are still templated, though.
///
/// The prompt file is read here so its references are validated at load time
/// too. The engine re-reads it when the turn actually runs, so a file edited
/// in between isn't re-validated; that's the same staleness every
/// `prompt_file` already has and not worth a cache.
fn templatable_sources(
    stage_name: &str,
    stage: &StageDef,
) -> Result<Vec<(String, String)>, WorkflowDefError> {
    let mut sources = Vec::new();
    match &stage.kind {
        StageKind::Shell { command, env, .. } => {
            push_command_sources(&mut sources, "command", "env", command, env);
        }
        StageKind::Poll { watch, .. } => {
            push_command_sources(&mut sources, "command", "env", &watch.command, &watch.env);
        }
        StageKind::HumanGate {
            watch: Some(watch), ..
        } => {
            push_command_sources(
                &mut sources,
                "watch.command",
                "watch.env",
                &watch.command,
                &watch.env,
            );
        }
        StageKind::AgentTurn {
            prompt_file: Some(path),
            ..
        } => sources.push((
            "prompt_file".to_string(),
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
        )),
        _ => {}
    }
    Ok(sources)
}

fn push_command_sources(
    sources: &mut Vec<(String, String)>,
    command_label: &str,
    env_label: &str,
    command: &ShellCommand,
    env: &IndexMap<String, String>,
) {
    if let ShellCommand::Inline(command) = command {
        sources.push((command_label.to_string(), command.clone()));
    }
    // Templated whether the command is inline or a `script_file`:
    // the script isn't, but its environment is (#101).
    for (name, value) in env {
        sources.push((format!("{env_label} '{name}'"), value.clone()));
    }
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
    LoopGuardEscapeOnEveryLap {
        stage: String,
        then: String,
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
    ReportSectionsOnOpenEndedTurn {
        stage: String,
    },
    EmptyReportSection {
        stage: String,
    },
    DuplicateReportSection {
        stage: String,
        section: String,
    },
    HumanGateCaptureMustBeText {
        stage: String,
    },
    EmptyReplyMarkers {
        stage: String,
    },
    EmptyReplyMarkerLine {
        stage: String,
    },
    ReplyMarkerLineHasSurroundingWhitespace {
        stage: String,
        line: String,
    },
    ReplyMarkerLineHasNewline {
        stage: String,
        line: String,
    },
    DuplicateReplyMarker {
        stage: String,
        line: String,
    },
    UnknownReplyMarkerOutcome {
        stage: String,
        outcome: String,
    },
    InvalidTemplate {
        stage: String,
        field: String,
        reason: String,
    },
    UnknownTemplateStage {
        stage: String,
        field: String,
        placeholder: String,
        referenced: String,
    },
    TemplateStageCapturesNothing {
        stage: String,
        field: String,
        placeholder: String,
        referenced: String,
    },
    IsolationFieldWithInheritedConfig {
        role: String,
        field: &'static str,
    },
    InvalidEnvName {
        stage: String,
        name: String,
    },
    ReservedEnvName {
        stage: String,
        name: String,
    },
    UnknownRoleTool {
        role: String,
        tool: String,
    },
    ReadOnlyRoleMissingTools {
        role: String,
        missing: String,
    },
    ReadOnlyRoleWithoutWorktree {
        role: String,
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
            WorkflowDefError::LoopGuardEscapeOnEveryLap {
                stage,
                then,
                target,
            } => write!(
                f,
                "stage '{stage}' has a loop_guard whose 'then' stage '{then}' is on every path \
                 from '{target}' back to '{stage}', so its count would restart every lap and it \
                 could never trip"
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
            WorkflowDefError::ReportSectionsOnOpenEndedTurn { stage } => write!(
                f,
                "agent_turn stage '{stage}' declares 'report_sections:' but has an empty 'on:' \
                 map, so its report is optional and never routes anything"
            ),
            WorkflowDefError::EmptyReportSection { stage } => write!(
                f,
                "agent_turn stage '{stage}' has a blank entry in 'report_sections:'"
            ),
            WorkflowDefError::DuplicateReportSection { stage, section } => write!(
                f,
                "agent_turn stage '{stage}' lists the report section '{section}' more than once"
            ),
            WorkflowDefError::HumanGateCaptureMustBeText { stage } => write!(
                f,
                "human_gate stage '{stage}' declares 'capture: json', but a human's reply is free \
                 text, not structured data — only 'capture: text' is supported"
            ),
            WorkflowDefError::EmptyReplyMarkers { stage } => write!(
                f,
                "human_gate stage '{stage}' declares 'markers:' but the list is empty"
            ),
            WorkflowDefError::EmptyReplyMarkerLine { stage } => write!(
                f,
                "human_gate stage '{stage}' has a reply marker with an empty 'line:'"
            ),
            WorkflowDefError::ReplyMarkerLineHasSurroundingWhitespace { stage, line } => write!(
                f,
                "human_gate stage '{stage}' has the reply marker line {line:?}, which has leading \
                 or trailing whitespace"
            ),
            WorkflowDefError::ReplyMarkerLineHasNewline { stage, line } => write!(
                f,
                "human_gate stage '{stage}' has the reply marker line {line:?}, which spans more \
                 than one line"
            ),
            WorkflowDefError::DuplicateReplyMarker { stage, line } => write!(
                f,
                "human_gate stage '{stage}' lists the reply marker line {line:?} more than once"
            ),
            WorkflowDefError::UnknownReplyMarkerOutcome { stage, outcome } => write!(
                f,
                "human_gate stage '{stage}' has a reply marker for the outcome '{outcome}', which \
                 is not in its 'on:' map"
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
            WorkflowDefError::InvalidEnvName { stage, name } => write!(
                f,
                "stage '{stage}' has an invalid env variable name '{name}'; names must match \
                 [A-Za-z_][A-Za-z0-9_]*"
            ),
            WorkflowDefError::ReservedEnvName { stage, name } => write!(
                f,
                "stage '{stage}' sets env variable '{name}', but names starting with CHOCO_ are \
                 reserved for the engine"
            ),
            WorkflowDefError::IsolationFieldWithInheritedConfig { role, field } => write!(
                f,
                "role '{role}' sets '{field}' alongside 'inherit_operator_config: true', which \
                 already gives it the operator's full setup; '{field}' only applies to an \
                 isolated role"
            ),
            WorkflowDefError::UnknownRoleTool { role, tool } => write!(
                f,
                "role '{role}' lists unknown tool '{tool}' in 'disallowed_tools'; the allowed \
                 names are edit, write, notebook_edit"
            ),
            WorkflowDefError::ReadOnlyRoleMissingTools { role, missing } => write!(
                f,
                "role '{role}' is 'read_only: true' but 'disallowed_tools' doesn't list: \
                 {missing}; a read-only role must deny edit, write and notebook_edit"
            ),
            WorkflowDefError::ReadOnlyRoleWithoutWorktree { role } => write!(
                f,
                "role '{role}' is 'read_only: true', which needs 'worktree: true' on the \
                 workflow: the post-turn check only ever inspects a task's own worktree"
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

    fn role_yaml(worktree: bool, role_fields: &str) -> String {
        format!(
            "name: w\nworktree: {worktree}\nroles:\n  reviewer:\n{role_fields}\nstages:\n  review:\n    kind: agent_turn\n    role: reviewer\n    on: {{}}\n"
        )
    }

    #[test]
    fn disallowed_tools_parse_dedupe_and_reject_unknown_names() {
        let dir = TempDir::new();
        let def = WorkflowDefinition::parse(
            &role_yaml(
                false,
                "    disallowed_tools: [edit, write, edit, notebook_edit]",
            ),
            &dir.path,
        )
        .unwrap();
        assert_eq!(
            def.roles["reviewer"].disallowed_tools,
            RoleTool::ALL.to_vec()
        );
        assert!(!def.roles["reviewer"].read_only);
        let def = WorkflowDefinition::parse(&role_yaml(false, "    cli: x"), &dir.path).unwrap();
        assert!(def.roles["reviewer"].disallowed_tools.is_empty());

        for bad in ["Edit", "bash"] {
            let err = WorkflowDefinition::parse(
                &role_yaml(false, &format!("    disallowed_tools: [{bad}]")),
                &dir.path,
            )
            .unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::UnknownRoleTool { role, tool } if role == "reviewer" && tool == bad),
                "{err}"
            );
            let message = err.to_string();
            assert!(
                message.contains("reviewer") && message.contains(bad),
                "{message}"
            );
        }
    }

    #[test]
    fn a_misspelled_role_key_is_rejected_at_load() {
        let dir = TempDir::new();
        for field in ["    readonly: true", "    disallowed_tool: [edit]"] {
            let err = WorkflowDefinition::parse(&role_yaml(true, field), &dir.path).unwrap_err();
            assert!(matches!(err, WorkflowDefError::Yaml(_)), "{err}");
        }
    }

    #[test]
    fn read_only_needs_every_edit_tool_denied_and_a_worktree() {
        let dir = TempDir::new();
        let err = WorkflowDefinition::parse(
            &role_yaml(
                true,
                "    read_only: true\n    disallowed_tools: [edit, write]",
            ),
            &dir.path,
        )
        .unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::ReadOnlyRoleMissingTools { role, missing } if role == "reviewer" && missing == "notebook_edit"),
            "{err}"
        );
        let err = WorkflowDefinition::parse(&role_yaml(true, "    read_only: true"), &dir.path)
            .unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::ReadOnlyRoleMissingTools { .. }
        ));
        let message = err.to_string();
        assert!(
            message.contains("edit, write, notebook_edit") && message.contains("reviewer"),
            "{message}"
        );

        let all = "    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]";
        let err = WorkflowDefinition::parse(&role_yaml(false, all), &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::ReadOnlyRoleWithoutWorktree { role } if role == "reviewer"),
            "{err}"
        );
        assert!(err.to_string().contains("reviewer"));

        let def = WorkflowDefinition::parse(&role_yaml(true, all), &dir.path).unwrap();
        assert!(def.roles["reviewer"].read_only);
    }

    /// #90: a role that says nothing about isolation gets the strict default
    /// — no skills, no memory — rather than the operator's setup.
    #[test]
    fn a_role_is_isolated_with_no_skills_or_memory_by_default() {
        let dir = TempDir::new();
        let yaml = r#"
name: plain
roles:
  coder: {}
stages:
  coding:
    kind: agent_turn
    role: coder
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        assert_eq!(def.roles["coder"].isolation, Isolation::default());
    }

    #[test]
    fn a_role_can_allow_skills_and_memory() {
        let dir = TempDir::new();
        let yaml = r#"
name: plain
roles:
  coder:
    skills: [run-tests, write-migration]
    memory: true
stages:
  coding:
    kind: agent_turn
    role: coder
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        assert_eq!(
            def.roles["coder"].isolation,
            Isolation::Isolated {
                skills: vec!["run-tests".to_string(), "write-migration".to_string()],
                memory: true,
            }
        );
    }

    #[test]
    fn a_role_can_inherit_the_operators_config() {
        let dir = TempDir::new();
        let yaml = r#"
name: chat
roles:
  chat:
    inherit_operator_config: true
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        assert_eq!(
            def.roles["chat"].isolation,
            Isolation::InheritOperatorConfig
        );
    }

    #[test]
    fn skills_or_memory_next_to_inherit_operator_config_is_rejected() {
        for extra in ["skills: []", "memory: false"] {
            let dir = TempDir::new();
            let yaml = format!(
                r#"
name: chat
roles:
  chat:
    inherit_operator_config: true
    {extra}
stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {{}}
"#
            );
            let err = WorkflowDefinition::parse(&yaml, &dir.path).unwrap_err();
            let field = extra.split(':').next().unwrap();
            assert!(
                matches!(&err, WorkflowDefError::IsolationFieldWithInheritedConfig { role, field: f } if role == "chat" && *f == field),
                "for {extra}: got {err}"
            );
        }
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

        let stage = &def.stages["checks_polling"];
        assert!(matches!(stage.kind, StageKind::Poll { .. }));
        let watch = stage.watch().expect("a poll has a watch");
        assert_eq!(watch.interval, Duration::from_secs(30));
        assert_eq!(watch.timeout, Some(Duration::from_secs(300)));
        assert_eq!(watch.outcomes.len(), 2);

        let guard = def.stages["internal_review"].loop_guard.as_ref().unwrap();
        assert_eq!(guard.on, "changes_requested");
        assert_eq!(guard.max, 3);
        assert_eq!(guard.then, "escalate_to_human");

        let StageKind::Shell {
            command,
            capture,
            timeout,
            ..
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

    /// #106, item 12: `then: tidy` sits on every path back from
    /// `review`'s guarded target (`fix`) to `review` itself — `fix`'s only
    /// way back is through `tidy`, and arriving at `tidy` clears the
    /// guard's count before it ever gets back to `review`. The guard could
    /// never trip, so this is rejected at load time rather than left to
    /// silently never fire.
    #[test]
    fn rejects_a_loop_guard_whose_then_stage_is_on_every_path_back_to_the_guard() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  review:
    kind: human_gate
    on: { changes_requested: fix, approved: done }
    loop_guard: { on: changes_requested, max: 3, then: tidy }
  fix:
    kind: human_gate
    on: { resumed: tidy }
  tidy:
    kind: human_gate
    on: { resumed: review }
  done:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::LoopGuardEscapeOnEveryLap { stage, then, target }
                if stage == "review" && then == "tidy" && target == "fix"
        ));
    }

    /// #106, item 12: also rejected when `then:` is literally the same
    /// stage the guarded outcome routes to on every un-tripped lap — the
    /// most direct way to make the guard un-trippable.
    #[test]
    fn rejects_a_loop_guard_whose_then_stage_equals_its_own_guarded_target() {
        let dir = TempDir::new();
        let yaml = r#"
name: broken
stages:
  review:
    kind: human_gate
    on: { changes_requested: fix, approved: done }
    loop_guard: { on: changes_requested, max: 3, then: fix }
  fix:
    kind: human_gate
    on: { resumed: review }
  done:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(matches!(
            err,
            WorkflowDefError::LoopGuardEscapeOnEveryLap { stage, then, target }
                if stage == "review" && then == "fix" && target == "fix"
        ));
    }

    /// #106, item 13: the `coding-task.yaml` shape — the escape stage
    /// (`escalate`) really is reachable from the loop, via `pr`'s
    /// `open_pr`-style error edge, but *avoidable*: the direct return path
    /// from `revising` to `review` never touches it. A guard like this must
    /// stay legal.
    #[test]
    fn accepts_an_escape_stage_reachable_from_the_loop_but_avoidable() {
        let dir = TempDir::new();
        let yaml = r#"
name: fine
stages:
  coding:
    kind: human_gate
    on: { resumed: review }
  revising:
    kind: human_gate
    on: { resumed: review }
  review:
    kind: human_gate
    on:
      approved: pr
      changes_requested: revising
    loop_guard: { on: changes_requested, max: 3, then: escalate }
  pr:
    kind: human_gate
    on: { done: done, error: escalate }
  escalate:
    kind: human_gate
    on: { resumed: revising }
  done:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// #106, item 13: a guard whose guarded outcome never loops back to the
    /// guarded stage at all has no lap to reset the count of, so it's
    /// unconditionally legal regardless of where `then:` sits.
    #[test]
    fn accepts_a_loop_guard_whose_outcome_never_loops_back() {
        let dir = TempDir::new();
        let yaml = r#"
name: fine
stages:
  a:
    kind: human_gate
    on: { resumed: b, done: c }
    loop_guard: { on: resumed, max: 3, then: c }
  b:
    kind: terminal
  c:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// #106, item 2 (re-review): pins that the load-time DFS walks `on:`
    /// edges only, never a `loop_guard.then` edge belonging to some *other*
    /// guard it passes through. `outer`'s guarded outcome targets `mid`,
    /// which only reaches `escalate` and a dead end over `on:` edges —
    /// never back to `outer` — so `outer`'s guard is legal on its own.
    /// `escalate` happens to carry its own guard whose `then:` is `outer`;
    /// that `then:` is not an `on:` edge, so it must not let the DFS treat
    /// `mid -> escalate -> outer` as a real path. A `reaches` that also
    /// pushed `loop_guard.then` targets would find `outer` reachable from
    /// `mid` (via `escalate`'s `then:`) but *not* reachable while avoiding
    /// `escalate` (since that's the only way in), and wrongly reject this
    /// as escaping on every lap.
    #[test]
    fn reaches_follows_on_edges_only_not_another_guards_then() {
        let dir = TempDir::new();
        let yaml = r#"
name: fine
stages:
  outer:
    kind: human_gate
    on: { loop: mid, done: fin }
    loop_guard: { on: loop, max: 3, then: escalate }
  mid:
    kind: human_gate
    on: { resumed: escalate }
  escalate:
    kind: human_gate
    on: { resumed: parked }
    loop_guard: { on: resumed, max: 3, then: outer }
  parked:
    kind: terminal
  fin:
    kind: terminal
"#;
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }

    /// #106, item 15.
    #[test]
    fn loop_guard_escape_on_every_lap_names_the_stage_then_and_target() {
        let stage = WorkflowDefError::LoopGuardEscapeOnEveryLap {
            stage: "review".to_string(),
            then: "tidy".to_string(),
            target: "fix".to_string(),
        }
        .to_string();
        assert!(stage.contains("'review'"), "{stage}");
        assert!(stage.contains("'tidy'"), "{stage}");
        assert!(stage.contains("'fix'"), "{stage}");
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
        let StageKind::Poll { capture, .. } = &def.stages["waiting"].kind else {
            panic!("expected poll stage");
        };
        let command = &def.stages["waiting"].watch().unwrap().command;
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
        let StageKind::Poll { capture, .. } = &def.stages["waiting"].kind else {
            panic!("expected poll stage");
        };
        let command = &def.stages["waiting"].watch().unwrap().command;
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

    /// #95: the sections a stage requires of its report, in declaration
    /// order — the tool matches headings by name, so the strings have to
    /// survive parsing exactly as written, arrow and all.
    #[test]
    fn parses_report_sections_on_an_agent_turn() {
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
    report_sections: ["Branches → tests", "Findings"]
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::AgentTurn {
            report_sections, ..
        } = &def.stages["review"].kind
        else {
            panic!("expected agent_turn stage");
        };
        assert_eq!(report_sections, &["Branches → tests", "Findings"]);
    }

    /// Every stage that predates #95, and every one that doesn't opt in:
    /// no sections required, reports checked only for their outcome.
    #[test]
    fn an_agent_turn_without_report_sections_requires_none() {
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
        let StageKind::AgentTurn {
            report_sections, ..
        } = &def.stages["review"].kind
        else {
            panic!("expected agent_turn stage");
        };
        assert!(report_sections.is_empty());
    }

    /// A blank name would match every line in a report, so a stage that
    /// asked for one would accept anything — the opposite of the point.
    #[test]
    fn rejects_a_blank_report_section() {
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
    report_sections: ["Findings", "  "]
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::EmptyReportSection { stage } if stage == "review"),
            "got {err}"
        );
    }

    /// Compared the way the tool compares them, so a pair that differs only
    /// in case or spacing is caught at load time rather than becoming one
    /// heading that quietly satisfies both entries.
    #[test]
    fn rejects_duplicate_report_sections_differing_only_in_case_or_spacing() {
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
    report_sections: ["Side effects", "side  EFFECTS"]
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::DuplicateReportSection { stage, section }
                    if stage == "review" && section == "side  EFFECTS"
            ),
            "got {err}"
        );
    }

    /// Review of #95: the dedupe key has to be the tool's key. These two
    /// spellings are one section at the tool, so a stage declaring both
    /// could never satisfy the first of them.
    #[test]
    fn rejects_two_report_sections_that_are_one_heading_to_the_tool() {
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
    report_sections: ["Branches → tests", "branches -> tests"]
    on: { approved: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::DuplicateReportSection { stage, section }
                    if stage == "review" && section == "branches -> tests"
            ),
            "got {err}"
        );
    }

    /// An open-ended turn's report is optional and routes nothing, so
    /// requiring sections of it is dead config — rejected for the same
    /// reason `capture:` is on the same shape of stage.
    #[test]
    fn rejects_report_sections_on_an_open_ended_agent_turn() {
        let dir = TempDir::new();
        let yaml = r#"
name: chat
roles:
  chat: { cli: claude }
stages:
  chatting:
    kind: agent_turn
    role: chat
    report_sections: ["Findings"]
    on: {}
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::ReportSectionsOnOpenEndedTurn { stage } if stage == "chatting"
            ),
            "got {err}"
        );
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
        let StageKind::HumanGate { capture, .. } = &def.stages["gate"].kind else {
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

    /// #112: a malformed `arrival` reference fails the workflow load.
    #[test]
    fn rejects_a_malformed_arrival_reference() {
        for bad in ["arrival", "arrival.stage", "arrival.from.x"] {
            let dir = TempDir::new();
            let yaml = format!(
                "name: templated\nstages:\n  report:\n    kind: shell\n    command: \"echo {{{{ {bad} }}}}\"\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
            );
            let err = WorkflowDefinition::parse(&yaml, &dir.path).unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::InvalidTemplate { stage, .. } if stage == "report"),
                "{bad}: got {err}"
            );
        }
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

    // ---- #101: `env:` on shell and poll stages ----

    #[test]
    fn parses_env_on_shell_and_poll_in_declaration_order() {
        let dir = TempDir::new();
        let yaml = r#"
name: env
stages:
  run:
    kind: shell
    command: "true"
    env:
      ZED: "1"
      ALPHA: "{{ task.title }}"
    on: { done: wait }
  wait:
    kind: poll
    command: "true"
    interval: 5s
    env:
      B: "b"
      A: "a"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let def = WorkflowDefinition::parse(yaml, &dir.path).unwrap();
        let StageKind::Shell { env, .. } = &def.stages["run"].kind else {
            panic!("expected shell");
        };
        assert_eq!(env.keys().collect::<Vec<_>>(), ["ZED", "ALPHA"]);
        let env = &def.stages["wait"].watch().expect("expected poll").env;
        assert_eq!(env.keys().collect::<Vec<_>>(), ["B", "A"]);
    }

    #[test]
    fn rejects_a_duplicate_env_key() {
        let dir = TempDir::new();
        let yaml = r#"
name: env
stages:
  run:
    kind: shell
    command: "true"
    env:
      X: "1"
      X: "2"
    on: { done: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(err.to_string().contains("duplicate key"), "{err}");
    }

    fn env_workflow(name: &str, value: &str) -> String {
        format!(
            r#"
name: env
stages:
  run:
    kind: shell
    command: "true"
    env:
      "{name}": "{value}"
    on: {{ done: finished }}
  finished:
    kind: terminal
"#
        )
    }

    #[test]
    fn rejects_invalid_and_reserved_env_names() {
        let dir = TempDir::new();
        for bad in ["1X", "A-B", ""] {
            let err = WorkflowDefinition::parse(&env_workflow(bad, "v"), &dir.path).unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::InvalidEnvName { stage, name }
                    if stage == "run" && name == bad),
                "{bad:?}: {err}"
            );
            assert!(err.to_string().contains("run"), "{err}");
        }
        for reserved in ["CHOCO_X", "choco_x"] {
            let err =
                WorkflowDefinition::parse(&env_workflow(reserved, "v"), &dir.path).unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::ReservedEnvName { stage, name }
                    if stage == "run" && name == reserved),
                "{reserved:?}: {err}"
            );
            assert!(err.to_string().contains(reserved), "{err}");
        }
    }

    #[test]
    fn validates_env_templates_for_inline_and_script_file_stages() {
        let dir = TempDir::new();
        dir.write("run.sh", "#!/bin/sh\n");
        for command in ["command: \"true\"", "script_file: run.sh"] {
            let unknown = format!(
                "name: e\nstages:\n  run:\n    kind: shell\n    {command}\n    env:\n      WHO: \"{{{{ stages.nope.x }}}}\"\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
            );
            let err = WorkflowDefinition::parse(&unknown, &dir.path).unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::UnknownTemplateStage { field, referenced, .. }
                    if field.contains("WHO") && referenced == "nope"),
                "{command}: {err}"
            );
            assert!(err.to_string().contains("WHO"), "{err}");

            let nothing = format!(
                "name: e\nstages:\n  first:\n    kind: shell\n    command: \"true\"\n    on: {{ done: run }}\n  run:\n    kind: shell\n    {command}\n    env:\n      WHO: \"{{{{ stages.first.x }}}}\"\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
            );
            let err = WorkflowDefinition::parse(&nothing, &dir.path).unwrap_err();
            assert!(
                matches!(&err, WorkflowDefError::TemplateStageCapturesNothing { field, .. }
                    if field.contains("WHO")),
                "{command}: {err}"
            );

            let task = format!(
                "name: e\nstages:\n  run:\n    kind: shell\n    {command}\n    env:\n      WHO: \"{{{{ task.title }}}}\"\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
            );
            WorkflowDefinition::parse(&task, &dir.path).unwrap();
        }
    }

    #[test]
    fn the_built_in_coding_task_open_pr_stage_passes_agent_text_only_through_env() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows");
        let def = WorkflowDefinition::load(&root.join("coding-task.yaml")).unwrap();
        let StageKind::Shell { command, env, .. } = &def.stages["open_pr"].kind else {
            panic!("open_pr must be a shell stage");
        };
        assert!(
            matches!(command, ShellCommand::ScriptFile(_)),
            "{command:?}"
        );
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("PR_TASK_TITLE", "{{ task.title }}"),
                ("PR_REVIEW_VERDICT", "{{ stages.internal_review.outcome }}"),
                ("PR_REVIEW_REPORT", "{{ stages.internal_review.summary }}"),
            ]
        );
    }

    // ---- #175: a human_gate's `watch:` and `markers:` ----

    /// A gate YAML with the given `watch:`/`markers:` blocks, each indented
    /// under `gate:`; `on:` has `resumed`, `approved`, `changes_requested`
    /// and `timeout`.
    fn gate_yaml(extra: &str) -> String {
        format!(
            r#"
name: gated
stages:
  gate:
    kind: human_gate
    capture: text
{extra}
    on: {{ resumed: finished, approved: finished, changes_requested: finished, timeout: finished }}
  finished:
    kind: terminal
"#
        )
    }

    const GATE_WATCH: &str = "    watch:\n      command: \"echo hi\"\n      interval: 5s\n";

    fn gate_err(extra: &str) -> WorkflowDefError {
        let dir = TempDir::new();
        WorkflowDefinition::parse(&gate_yaml(extra), &dir.path).unwrap_err()
    }

    #[test]
    fn a_gate_with_watch_and_markers_loads_every_field() {
        let dir = TempDir::new();
        let yaml = gate_yaml(
            r#"    watch:
      command: "echo {{ task.title }}"
      env:
        B: "b"
        A: "{{ task.title }}"
      interval: 5s
      timeout: 2h
      outcomes:
        - match: "GO"
          then: approved
    markers:
      - line: /request-changes
        then: changes_requested
      - line: /approve
        then: approved"#,
        );
        let def = WorkflowDefinition::parse(&yaml, &dir.path).unwrap();
        let StageKind::HumanGate {
            capture,
            markers,
            watch,
        } = &def.stages["gate"].kind
        else {
            panic!("expected human_gate");
        };
        assert_eq!(*capture, Some(Capture::Text));
        let watch = watch.as_ref().expect("watch");
        assert_eq!(
            watch.command,
            ShellCommand::Inline("echo {{ task.title }}".to_string())
        );
        assert_eq!(watch.env.keys().collect::<Vec<_>>(), ["B", "A"]);
        assert_eq!(watch.interval, Duration::from_secs(5));
        assert_eq!(watch.timeout, Some(Duration::from_secs(7200)));
        assert_eq!(
            watch.outcomes,
            [PollOutcome {
                pattern: "GO".into(),
                then: "approved".into()
            }]
        );
        assert_eq!(
            *markers,
            [
                ReplyMarker {
                    line: "/request-changes".into(),
                    then: "changes_requested".into()
                },
                ReplyMarker {
                    line: "/approve".into(),
                    then: "approved".into()
                }
            ]
        );
        assert_eq!(def.stages["gate"].watch(), Some(watch));
        assert_eq!(def.stages["gate"].kind.name(), "human_gate");
    }

    #[test]
    fn a_gate_watch_script_file_resolves_against_the_definition_directory() {
        let dir = TempDir::new();
        dir.write("check.sh", "#!/bin/sh\necho hi\n");
        let yaml = gate_yaml("    watch:\n      script_file: check.sh\n      interval: 1s\n");
        let def = WorkflowDefinition::parse(&yaml, &dir.path).unwrap();
        assert_eq!(
            def.stages["gate"].watch().unwrap().command,
            ShellCommand::ScriptFile(dir.path.join("check.sh"))
        );
    }

    #[test]
    fn a_gate_without_watch_or_markers_has_neither() {
        let dir = TempDir::new();
        let def = WorkflowDefinition::parse(&gate_yaml(""), &dir.path).unwrap();
        let StageKind::HumanGate { markers, watch, .. } = &def.stages["gate"].kind else {
            panic!("expected human_gate");
        };
        assert!(markers.is_empty());
        assert!(watch.is_none());
        assert!(def.stages["gate"].watch().is_none());
    }

    #[test]
    fn a_stage_that_is_not_a_poll_or_watching_gate_has_no_watch() {
        let dir = TempDir::new();
        let def = WorkflowDefinition::parse(&gate_yaml(""), &dir.path).unwrap();
        assert!(def.stages["finished"].watch().is_none());
        assert_eq!(def.stages["finished"].kind.name(), "terminal");
    }

    #[test]
    fn an_unknown_key_inside_a_gates_watch_fails_the_load() {
        let err = gate_err(&format!("{GATE_WATCH}      retries: 3\n"));
        assert!(matches!(err, WorkflowDefError::Yaml(_)), "{err}");
    }

    #[test]
    fn a_gate_watch_without_an_interval_fails_the_load() {
        let err = gate_err("    watch:\n      command: \"echo hi\"\n");
        assert!(matches!(err, WorkflowDefError::Yaml(_)), "{err}");
    }

    #[test]
    fn a_gate_watch_reports_its_durations_with_the_watch_prefix() {
        let err = gate_err("    watch:\n      command: x\n      interval: soon\n");
        assert!(
            matches!(&err, WorkflowDefError::InvalidDuration { stage, field: "watch.interval", .. } if stage == "gate"),
            "{err}"
        );
        let err =
            gate_err("    watch:\n      command: x\n      interval: 1s\n      timeout: later\n");
        assert!(
            matches!(&err, WorkflowDefError::InvalidDuration { stage, field: "watch.timeout", .. } if stage == "gate"),
            "{err}"
        );
    }

    #[test]
    fn a_polls_durations_keep_their_flat_names() {
        let dir = TempDir::new();
        let yaml = "name: p\nstages:\n  w:\n    kind: poll\n    command: x\n    interval: soon\n    on: { a: f }\n  f:\n    kind: terminal\n";
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(
                &err,
                WorkflowDefError::InvalidDuration {
                    field: "interval",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_gate_watch_outcome_must_be_an_on_key() {
        let err = gate_err(&format!(
            "{GATE_WATCH}      outcomes:\n        - match: GO\n          then: nowhere\n"
        ));
        assert!(
            matches!(&err, WorkflowDefError::UnknownPollOutcome { stage, outcome } if stage == "gate" && outcome == "nowhere"),
            "{err}"
        );
    }

    #[test]
    fn a_gate_watch_pattern_must_compile() {
        let err = gate_err(&format!(
            "{GATE_WATCH}      outcomes:\n        - match: \"(\"\n          then: approved\n"
        ));
        assert!(
            matches!(&err, WorkflowDefError::InvalidPollPattern { stage, .. } if stage == "gate"),
            "{err}"
        );
    }

    #[test]
    fn a_gate_watch_timeout_needs_a_timeout_edge() {
        let dir = TempDir::new();
        let yaml = r#"
name: g
stages:
  gate:
    kind: human_gate
    watch:
      command: x
      interval: 1s
      timeout: 1m
    on: { resumed: finished }
  finished:
    kind: terminal
"#;
        let err = WorkflowDefinition::parse(yaml, &dir.path).unwrap_err();
        assert!(
            matches!(&err, WorkflowDefError::MissingTimeoutOutcome { stage } if stage == "gate"),
            "{err}"
        );
    }

    #[test]
    fn a_gate_watch_env_names_are_checked() {
        let err = gate_err(&format!("{GATE_WATCH}      env:\n        CHOCO_X: \"1\"\n"));
        assert!(
            matches!(&err, WorkflowDefError::ReservedEnvName { stage, .. } if stage == "gate"),
            "{err}"
        );
        let err = gate_err(&format!("{GATE_WATCH}      env:\n        \"1X\": \"1\"\n"));
        assert!(
            matches!(&err, WorkflowDefError::InvalidEnvName { stage, .. } if stage == "gate"),
            "{err}"
        );
    }

    #[test]
    fn a_gate_watch_command_is_template_checked_with_a_watch_label() {
        let err = gate_err(
            "    watch:\n      command: \"echo {{ stages.ghost.out }}\"\n      interval: 1s\n",
        );
        assert!(
            matches!(&err, WorkflowDefError::UnknownTemplateStage { stage, field, .. } if stage == "gate" && field == "watch.command"),
            "{err}"
        );
        let err = gate_err(&format!(
            "{GATE_WATCH}      env:\n        K: \"{{{{ stages.finished.out }}}}\"\n"
        ));
        assert!(
            matches!(&err, WorkflowDefError::TemplateStageCapturesNothing { stage, field, .. } if stage == "gate" && field == "watch.env 'K'"),
            "{err}"
        );
        let err = gate_err(&format!(
            "{GATE_WATCH}      env:\n        K: \"{{{{ bad\"\n"
        ));
        assert!(
            matches!(&err, WorkflowDefError::InvalidTemplate { stage, field, .. } if stage == "gate" && field == "watch.env 'K'"),
            "{err}"
        );
    }

    fn markers_block(lines: &[(&str, &str)]) -> String {
        let mut out = String::from("    markers:\n");
        for (line, then) in lines {
            out.push_str(&format!(
                "      - line: {}\n        then: {then}\n",
                serde_json::json!(line)
            ));
        }
        out
    }

    #[test]
    fn markers_reject_an_empty_list() {
        let err = gate_err("    markers: []\n");
        assert!(
            matches!(&err, WorkflowDefError::EmptyReplyMarkers { stage } if stage == "gate"),
            "{err}"
        );
        assert!(err.to_string().contains("'gate'"));
    }

    #[test]
    fn markers_reject_an_empty_line() {
        let err = gate_err(&markers_block(&[("", "approved")]));
        assert!(
            matches!(&err, WorkflowDefError::EmptyReplyMarkerLine { stage } if stage == "gate"),
            "{err}"
        );
    }

    #[test]
    fn markers_reject_surrounding_whitespace() {
        for line in [" /approve", "/approve "] {
            let err = gate_err(&markers_block(&[(line, "approved")]));
            assert!(
                matches!(&err, WorkflowDefError::ReplyMarkerLineHasSurroundingWhitespace { stage, line: l } if stage == "gate" && l == line),
                "{err}"
            );
            assert!(err.to_string().contains("'gate'"));
        }
    }

    #[test]
    fn markers_reject_a_newline_in_a_line() {
        let err = gate_err(&markers_block(&[("/ap\nprove", "approved")]));
        assert!(
            matches!(&err, WorkflowDefError::ReplyMarkerLineHasNewline { stage, line } if stage == "gate" && line == "/ap\nprove"),
            "{err}"
        );
        let err = gate_err(&markers_block(&[("/ap\rprove", "approved")]));
        assert!(
            matches!(&err, WorkflowDefError::ReplyMarkerLineHasNewline { .. }),
            "{err}"
        );
    }

    #[test]
    fn markers_reject_a_duplicate_line() {
        let err = gate_err(&markers_block(&[
            ("/approve", "approved"),
            ("/approve", "changes_requested"),
        ]));
        assert!(
            matches!(&err, WorkflowDefError::DuplicateReplyMarker { stage, line } if stage == "gate" && line == "/approve"),
            "{err}"
        );
    }

    #[test]
    fn markers_reject_an_outcome_that_is_not_an_on_key() {
        let err = gate_err(&markers_block(&[("/approve", "nowhere")]));
        assert!(
            matches!(&err, WorkflowDefError::UnknownReplyMarkerOutcome { stage, outcome } if stage == "gate" && outcome == "nowhere"),
            "{err}"
        );
        assert!(err.to_string().contains("nowhere"));
    }

    #[test]
    fn markers_reject_unknown_keys_and_a_gate_with_markers_needs_no_resumed_edge() {
        let dir = TempDir::new();
        let yaml = "name: g\nstages:\n  gate:\n    kind: human_gate\n    markers:\n      - line: /a\n        then: ok\n        extra: 1\n    on: { ok: f }\n  f:\n    kind: terminal\n";
        assert!(matches!(
            WorkflowDefinition::parse(yaml, &dir.path).unwrap_err(),
            WorkflowDefError::Yaml(_)
        ));
        let yaml = "name: g\nstages:\n  gate:\n    kind: human_gate\n    markers:\n      - line: /a\n        then: ok\n    on: { ok: f }\n  f:\n    kind: terminal\n";
        WorkflowDefinition::parse(yaml, &dir.path).unwrap();
    }
}
