//! `choco` argument parsing (P1-10, design §6.2). Pure `clap` derive
//! definitions — no I/O, no HTTP. `--workflow`/`--status` are plain
//! `String`s rather than fixed enums: workflow definitions (§2.2) and task
//! status (`chocofactory_core::models::Task::status`) are both free-form,
//! driven by data on disk/in the DB, not fixed by this crate.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "choco", about = "HTTP client for chocofactoryd")]
pub struct Cli {
    /// Base URL of a running `chocofactoryd`. Falls back to `CHOCO_BASE_URL`,
    /// then `http://127.0.0.1:4141` (the daemon's default port).
    #[arg(long, env = "CHOCO_BASE_URL", default_value = "http://127.0.0.1:4141")]
    pub base_url: String,

    /// Print the daemon's raw JSON instead of a human-readable summary.
    /// `choco` is meant to be both human-scriptable and agent-callable
    /// (design Q12) — this is the machine-facing half.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// `large_enum_variant`: the task variants carry every `--role-*` flag list,
/// making them much wider than `Project`'s. Boxing to even them out would buy
/// nothing — exactly one of these is built, once, from `argv` at startup, and
/// then matched on and dropped. The indirection would cost a heap allocation
/// and fight `clap`'s derive for no measurable gain.
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Task create/status/send/cancel/list/events.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Project create/list.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Serves the `report_outcome` MCP tool over stdio (issue #73): a
    /// stage's agent turn calls it to state its outcome explicitly instead
    /// of leaving the engine to infer one from prose. `chocofactoryd` spawns
    /// this itself via `--mcp-config` as part of every agent turn; it isn't
    /// meant to be run by hand, hence hidden from `--help`.
    #[command(hide = true)]
    McpServe(McpServeArgs),
}

#[derive(Args)]
pub struct McpServeArgs {
    /// One of the current stage's `on:` edge names; repeat for each one.
    /// Determines both the tool's `outcome` schema (an `enum` of exactly
    /// these values) and whether a report routes the workflow at all —
    /// omitted entirely means the stage has no edges to route on, so
    /// `outcome` is left free-form and purely informational.
    ///
    /// A repeatable flag rather than one comma-joined value (review, #75):
    /// an `on:` edge name is an arbitrary YAML string key and could itself
    /// contain a comma, which a `value_delimiter` would misparse on both
    /// ends of the round trip.
    ///
    /// `allow_hyphen_values` (review, #75 round 2): an edge name starting
    /// with `-` (e.g. `-needs-work`) would otherwise make clap treat it as
    /// an unrecognised flag rather than this one's value, failing the whole
    /// subcommand — which would silently degrade every turn on that stage to
    /// the text-fallback path, recreating #73's original bug through a new
    /// door.
    #[arg(long = "outcome", allow_hyphen_values = true)]
    pub outcomes: Vec<String>,
}

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Create a task under a project, starting the named workflow.
    Create(TaskCreateArgs),
    /// Show a task, its current stage, and how it got there.
    Status {
        /// Task id.
        id: String,
    },
    /// Send a message into a task's active session (or resume a human_gate).
    Send {
        /// Task id.
        id: String,
        #[arg(long)]
        text: String,
    },
    /// Stop a task: kills its agent subprocess, marks it cancelled, and
    /// removes its worktree.
    ///
    /// Ends the task's work, not its record — its events and the stage it
    /// stopped in stay readable via `choco task status`/`events`. Cannot be
    /// undone: a cancelled task accepts no further messages.
    Cancel {
        /// Task id.
        id: String,
    },
    /// List tasks, optionally filtered by project and/or status.
    List {
        /// Project name or id to filter by.
        #[arg(long)]
        project: Option<String>,
        /// Status to filter by (free-form — driven by workflow definitions).
        #[arg(long)]
        status: Option<String>,
    },
    /// Change a task's per-role config after creation (design §5.5).
    ///
    /// Merges into the task's existing config, so overriding one role leaves
    /// the task-wide `--repo` and every other role untouched. Takes effect on
    /// the task's next turn — a session already running keeps the config it
    /// started with.
    Reconfigure {
        /// Task id.
        id: String,
        #[command(flatten)]
        roles: RoleOverrideArgs,
    },
    /// Show a task's recorded events (the agent conversation and tool calls).
    Events {
        /// Task id.
        id: String,
        /// Maximum events to return. The daemon caps this at 500.
        #[arg(long)]
        limit: Option<usize>,
        /// Opaque `next_token` from a previous page, to continue from there.
        #[arg(long)]
        after: Option<String>,
    },
}

#[derive(Args)]
pub struct TaskCreateArgs {
    /// Project name or id this task belongs to. A name is resolved against
    /// `project list`, and is rejected if it matches more than one project.
    #[arg(long)]
    pub project: String,
    /// Workflow definition name (any name under
    /// `~/.config/chocofactory/workflows/`, not a fixed set).
    #[arg(long)]
    pub workflow: String,
    #[arg(long)]
    pub title: String,
    /// The task's initial message.
    #[arg(long)]
    pub prompt: String,
    /// Working directory for the task's agent subprocess. Maps to
    /// `config.cwd`.
    #[arg(long)]
    pub repo: Option<String>,
    /// Tags this task as spawned via delegation from another task.
    #[arg(long = "parent-task")]
    pub parent_task: Option<String>,
    #[command(flatten)]
    pub roles: RoleOverrideArgs,
}

/// Per-role task-level config overrides (design §5.5, Q8) — shared by
/// `task create` and `task reconfigure` via `#[command(flatten)]` so the two
/// commands can't drift apart.
///
/// Every flag is `ROLE=VALUE` and repeatable, because a workflow can declare
/// several roles (`coder`/`reviewer`) and each is configured independently.
/// That's also why there's no bare `--model`: with more than one role it
/// would be ambiguous which one it meant.
#[derive(Args, Default)]
pub struct RoleOverrideArgs {
    /// Override a role's CLI, as `ROLE=CLI` (repeatable, e.g.
    /// `--role-cli coder=claude`). Sets `config.roles.<ROLE>.cli`.
    #[arg(long = "role-cli", value_name = "ROLE=CLI")]
    pub role_cli: Vec<String>,
    /// Override a role's model, as `ROLE=MODEL` (repeatable, e.g.
    /// `--role-model coder=opus --role-model reviewer=sonnet`). Sets
    /// `config.roles.<ROLE>.model`.
    #[arg(long = "role-model", value_name = "ROLE=MODEL")]
    pub role_model: Vec<String>,
    /// Override a role's system prompt with literal text, as `ROLE=TEXT`
    /// (repeatable). Sets `config.roles.<ROLE>.system_prompt`.
    #[arg(long = "role-system-prompt", value_name = "ROLE=TEXT")]
    pub role_system_prompt: Vec<String>,
    /// Override a role's system prompt with a file's contents, as
    /// `ROLE=PATH` (repeatable). `choco` reads `PATH` itself and sends the
    /// text, so the daemon is never asked to read a path from task config.
    #[arg(long = "role-system-prompt-file", value_name = "ROLE=PATH")]
    pub role_system_prompt_file: Vec<String>,
    /// Raw task-level config JSON object, applied *before* the typed
    /// `--role-*` flags above (which win per field). The escape hatch for
    /// agent callers and for any field the typed flags don't cover.
    #[arg(long, value_name = "JSON")]
    pub config: Option<String>,
}

impl RoleOverrideArgs {
    /// True when the user supplied nothing at all — lets `task reconfigure`
    /// reject an empty patch instead of issuing a no-op request.
    pub fn is_empty(&self) -> bool {
        self.role_cli.is_empty()
            && self.role_model.is_empty()
            && self.role_system_prompt.is_empty()
            && self.role_system_prompt_file.is_empty()
            && self.config.is_none()
    }
}

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Create a project.
    Create {
        /// Project name.
        name: String,
    },
    /// List all projects.
    List,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--outcome` is repeatable, not a single comma-joined flag (review,
    /// #75) — this is what `ClaudeAdapter::spawn` (issue #73) actually emits
    /// into `--mcp-config`'s `args`, so a mismatch here would silently make
    /// every routing stage's tool free-form. Repeatable also means an `on:`
    /// edge name containing a comma round-trips intact.
    #[test]
    fn mcp_serve_collects_repeated_outcome_flags() {
        let cli = Cli::parse_from([
            "choco",
            "mcp-serve",
            "--outcome",
            "approved",
            "--outcome",
            "changes_requested",
        ]);
        let Command::McpServe(args) = cli.command else {
            panic!("expected McpServe");
        };
        assert_eq!(args.outcomes, vec!["approved", "changes_requested"]);
    }

    /// A comma inside an outcome name is just a character — the whole reason
    /// this is a repeatable flag rather than one comma-joined value.
    #[test]
    fn mcp_serve_outcome_with_an_embedded_comma_round_trips_intact() {
        let cli = Cli::parse_from(["choco", "mcp-serve", "--outcome", "needs, more, work"]);
        let Command::McpServe(args) = cli.command else {
            panic!("expected McpServe");
        };
        assert_eq!(args.outcomes, vec!["needs, more, work"]);
    }

    /// Review, #75 round 2: without `allow_hyphen_values`, clap treats a
    /// leading `-` as the start of a new (unrecognised) flag rather than
    /// this one's value, and `try_parse_from` — the shape `main()` actually
    /// calls — fails the whole subcommand rather than panicking, which
    /// would silently degrade every turn on a stage with an edge like this
    /// to the text-fallback path.
    #[test]
    fn mcp_serve_outcome_starting_with_a_hyphen_parses() {
        let cli = Cli::try_parse_from(["choco", "mcp-serve", "--outcome", "-needs-work"])
            .expect("a leading hyphen in an outcome name must still parse");
        let Command::McpServe(args) = cli.command else {
            panic!("expected McpServe");
        };
        assert_eq!(args.outcomes, vec!["-needs-work"]);
    }

    #[test]
    fn mcp_serve_with_no_outcomes_flag_is_empty() {
        let cli = Cli::parse_from(["choco", "mcp-serve"]);
        let Command::McpServe(args) = cli.command else {
            panic!("expected McpServe");
        };
        assert!(args.outcomes.is_empty());
    }

    /// `#[command(hide = true)]` hides it from `--help` text; it must not
    /// also make the subcommand itself unparseable.
    #[test]
    fn mcp_serve_is_hidden_but_still_parses() {
        assert!(Cli::try_parse_from(["choco", "mcp-serve"]).is_ok());
    }
}
