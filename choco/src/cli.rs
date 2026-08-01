//! `choco` argument parsing (P1-10, design §6.2). Pure `clap` derive
//! definitions — no I/O, no HTTP. `--workflow`/`--status` are plain
//! `String`s rather than fixed enums: workflow definitions (§2.2) and task
//! status (`chokofactory_core::models::Task::status`) are both free-form,
//! driven by data on disk/in the DB, not fixed by this crate.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "choco", about = "HTTP client for chokofactoryd")]
pub struct Cli {
    /// Base URL of a running `chokofactoryd`. Falls back to `CHOCO_BASE_URL`,
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

#[derive(Subcommand)]
pub enum Command {
    /// Task create/status/send/list/events.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Project create/list.
    #[command(subcommand)]
    Project(ProjectCmd),
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
    /// List tasks, optionally filtered by project and/or status.
    List {
        /// Project name or id to filter by.
        #[arg(long)]
        project: Option<String>,
        /// Status to filter by (free-form — driven by workflow definitions).
        #[arg(long)]
        status: Option<String>,
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
    /// `~/.config/chokofactory/workflows/`, not a fixed set).
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
