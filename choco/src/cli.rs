//! `choco` argument parsing (P1-10, design §6.2). Pure `clap` derive
//! definitions — no I/O, no HTTP. `--workflow`/`--status` are plain
//! `String`s rather than fixed enums: workflow definitions (§2.2) and task
//! status (`chocofactory_core::models::Task::status`) are both free-form,
//! driven by data on disk/in the DB, not fixed by this crate.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand};

/// A `--interval`/`--timeout` value: parsed with the same
/// `chocofactory_core::duration::parse_duration` the workflow YAML uses,
/// with the spelling as typed kept for messages.
#[derive(Clone, Debug)]
pub struct DurationArg {
    pub raw: String,
    pub duration: Duration,
}

fn parse_duration_arg(s: &str) -> Result<DurationArg, String> {
    chocofactory_core::duration::parse_duration(s)
        .map(|duration| DurationArg {
            raw: s.to_string(),
            duration,
        })
        .map_err(|_| {
            format!("'{s}' is not a duration — use <integer><s|m|h>, non-zero (e.g. 5s, 5m, 1h)")
        })
}

/// What `task status --until` waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Until {
    /// The task's `status` equals this (`closed`, `cancelled`, `stuck`).
    Status(String),
    /// The task has entered this stage (`stage:<name>`).
    Stage(String),
}

impl FromStr for Until {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "closed" | "cancelled" | "stuck" => Ok(Until::Status(s.to_string())),
            _ => match s.strip_prefix("stage:") {
                Some(name) if !name.is_empty() => Ok(Until::Stage(name.to_string())),
                _ => Err(format!(
                    "'{s}' is not a valid target — use closed, cancelled, stuck or stage:<name>"
                )),
            },
        }
    }
}

impl fmt::Display for Until {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Until::Status(s) => write!(f, "{s}"),
            Until::Stage(s) => write!(f, "stage:{s}"),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "choco",
    about = "HTTP client for chocofactoryd",
    version = chocofactory_core::version::long_version()
)]
pub struct Cli {
    /// Base URL of a running `chocofactoryd`. Also read from `CHOCO_BASE_URL`.
    /// Defaults to the running daemon's port from its lock file, else
    /// `http://127.0.0.1:4141`. Ignored by `server` commands.
    #[arg(long, env = "CHOCO_BASE_URL")]
    pub base_url: Option<String>,

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
    /// Start, stop, restart or inspect the chocofactoryd daemon.
    #[command(subcommand)]
    Server(ServerCmd),
    /// Update choco and chocofactoryd to the latest release (only for copies
    /// installed by install.sh). Refuses, exit 3, while an agent turn or shell
    /// step is running unless `--force`. Exit codes: 0 updated or up to date;
    /// 1 error or not updatable; 3 refused because work is in flight.
    Update {
        /// Only report whether an update is available.
        #[arg(long)]
        check: bool,
        /// Install this version instead of the latest.
        #[arg(long)]
        version: Option<String>,
        /// Update even when already on that version or work is in flight
        /// (that work is marked stuck; `choco task retry` continues it).
        #[arg(long)]
        force: bool,
    },
    /// Serves the `report_outcome` MCP tool over stdio (issue #73): a
    /// stage's agent turn calls it to state its outcome explicitly instead
    /// of leaving the engine to infer one from prose. `chocofactoryd` spawns
    /// this itself via `--mcp-config` as part of every agent turn; it isn't
    /// meant to be run by hand, hence hidden from `--help`.
    #[command(hide = true)]
    McpServe(McpServeArgs),
}

/// `choco server ...`: manages the local daemon through its lock file.
#[derive(Subcommand)]
pub enum ServerCmd {
    /// Start chocofactoryd in the background (its own session) and wait
    /// until it answers. Logs go to ~/.config/chocofactory/logs/chocofactoryd.log.
    /// Does nothing if it is already running. Running tasks are unaffected.
    /// Exit codes: 0 started or already running; 1 error.
    Start {
        /// Port to listen on (0 picks a free one). Default: the daemon's own.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop chocofactoryd gracefully (SIGTERM). Refuses, exit 3, while an agent
    /// turn or shell step is running, since stopping marks those tasks stuck
    /// (`choco task retry` continues them); `--force` stops anyway. Tasks
    /// waiting on a poll or a human are not affected.
    /// Exit codes: 0 stopped or not running; 1 error; 3 refused.
    Stop {
        /// Stop even if work is in flight; that work is marked stuck.
        #[arg(long)]
        force: bool,
    },
    /// Stop (as `stop`) then start chocofactoryd, keeping its port unless
    /// `--port` is given. Starts it if it was not running.
    /// Exit codes: 0 ok; 1 error, or the old daemon had to be killed (a new
    /// one is started only if the old one's lock was released); 3 refused because work is in flight.
    Restart {
        /// Stop even if work is in flight; that work is marked stuck.
        #[arg(long)]
        force: bool,
        /// Port for the new daemon. Default: the old daemon's port.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show whether chocofactoryd is running, its version, open tasks and
    /// in-flight work. Exit codes: 0 running; 1 holds the lock but does not
    /// answer; 3 not running.
    Status,
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

    /// A section the stage requires this turn's report to carry; repeat for
    /// each one (issue #95). Comes from the stage's `report_sections:`.
    /// Omitted entirely — every stage that hasn't opted in — means a report
    /// is only checked for its `outcome`, exactly as before.
    ///
    /// Repeatable and `allow_hyphen_values` for the same two reasons
    /// `--outcome` is: a section name is free text from a YAML list, so no
    /// delimiter can round-trip it, and one starting with `-` would
    /// otherwise fail the whole subcommand and take the tool down with it.
    #[arg(long = "require-section", allow_hyphen_values = true)]
    pub required_sections: Vec<String>,
}

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Create a task under a project, starting the named workflow.
    Create(TaskCreateArgs),
    /// Show a task, its stage and history; optionally watch or wait for it.
    ///
    /// With --live or --until the command keeps polling and its exit code
    /// says how the watch ended:
    ///
    ///   0  the awaited target was reached (with --live alone: the task closed)
    ///   1  an error (unknown task, API error, daemon unreachable)
    ///   2  a usage error
    ///   3  the task became stuck, and stuck was not the target
    ///   4  the task was cancelled, and cancelled was not the target
    ///   5  --timeout elapsed first
    ///   6  the task closed without reaching the target
    ///
    /// --live alone does not stop at stuck (a human may retry it).
    #[command(verbatim_doc_comment)]
    #[command(group(ArgGroup::new("watching").args(["live", "until"]).multiple(true)))]
    Status {
        /// Task id.
        id: String,
        /// Keep the view current until the task closes or is cancelled.
        /// On a terminal the view is redrawn every poll; piped, one line is
        /// printed per change; with --json, one JSON object per change.
        #[arg(long)]
        live: bool,
        /// Block until the task reaches TARGET, then exit 0: `closed`,
        /// `cancelled`, `stuck`, or `stage:<name>`. Implies watching.
        #[arg(long, value_name = "TARGET")]
        until: Option<Until>,
        /// Poll cadence, `<integer><s|m|h>`. Needs --live or --until.
        #[arg(long, value_parser = parse_duration_arg, default_value = "2s", requires = "watching")]
        interval: DurationArg,
        /// Give up after this long and exit 5. Needs --live or --until.
        #[arg(long, value_parser = parse_duration_arg, requires = "watching")]
        timeout: Option<DurationArg>,
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
    /// Re-run a stuck task's current stage (X-4, issue #61).
    ///
    /// Reopens the task and re-enters whatever stage it stopped in — not a
    /// replay of the outcome that got it stuck, since the daemon never
    /// persisted one. Only works on a task whose status is `stuck`; see
    /// `choco task status` for the reason.
    ///
    /// When the stage's last agent turn was cut off from outside — a usage
    /// limit, or the daemon closing an idle session — the retry continues
    /// that session instead of starting a new one over a working tree full
    /// of work it knows nothing about (#92). Anything the agent itself did
    /// wrong starts fresh.
    Retry {
        /// Task id.
        id: String,
        /// Resume the interrupted session, and fail if it cannot be resumed
        /// rather than quietly starting a fresh one.
        #[arg(long, conflicts_with = "fresh")]
        resume: bool,
        /// Start a fresh session even if the interrupted one could be
        /// resumed — the way out of a turn that keeps failing on resume.
        #[arg(long)]
        fresh: bool,
    },
    /// List tasks, optionally filtered by project and/or status.
    List {
        /// Project name or id to filter by.
        #[arg(long)]
        project: Option<String>,
        /// Status to filter by — free-form (driven by workflow
        /// definitions), but the daemon itself writes one of `open`,
        /// `closed`, `cancelled`, or `stuck`.
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
    /// A workflow name, or a path to a workflow `.yaml` file. A name resolves
    /// to `<repo>/.chocofactory/workflows/<name>.yaml` when the project has
    /// a repo, else to the built-in of that name. A path (anything
    /// containing `/` or ending in `.yaml` or `.yml`) is used as is; its
    /// prompts and scripts resolve next to it.
    #[arg(long)]
    pub workflow: String,
    #[arg(long)]
    pub title: String,
    /// The task's initial message.
    #[arg(long)]
    pub prompt: String,
    /// Working directory for the task's agent subprocess. Maps to
    /// `config.cwd`. Defaults to the project's own repo path (issue #88)
    /// when the project has one and this is omitted.
    #[arg(long)]
    pub repo: Option<String>,
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
        /// A repo this project's tasks default their own `--repo` to
        /// (issue #88). Resolved to an absolute path client-side (a
        /// relative path, including `.`, works) — canonicalization fails
        /// before any request reaches the daemon if the path doesn't exist
        /// or isn't a directory.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Change a project's name and/or repo path.
    Update {
        /// Project name or id.
        project: String,
        /// New name.
        #[arg(long)]
        name: Option<String>,
        /// New repo path, resolved to an absolute path client-side like
        /// `project create --repo`. Conflicts with `--no-repo`.
        #[arg(long, conflicts_with = "no_repo")]
        repo: Option<String>,
        /// Clears the project's repo path. Conflicts with `--repo`.
        #[arg(long, conflicts_with = "repo")]
        no_repo: bool,
    },
    /// List all projects.
    List,
    /// Seed this project's repo with the built-in workflows and their
    /// prompt files, under `<repo>/.chocofactory/workflows/` (issue #88).
    ///
    /// Never overwrites an existing file. Copies the built-ins in this
    /// version of chocofactoryd as a starting point (an eject); later
    /// upgrades don't change the copies.
    /// The seeded directory is meant to be committed and shared with the
    /// team: `git add .chocofactory/ && git commit`.
    InitWorkflows {
        /// Project name or id.
        project: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flags_display_version() {
        for flag in ["--version", "-V"] {
            let err = Cli::try_parse_from(["choco", flag])
                .err()
                .expect("should not parse");
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion, "{flag}");
        }
    }

    #[test]
    fn update_version_arg_is_not_the_top_level_flag() {
        let cli = Cli::try_parse_from(["choco", "update", "--version", "0.2.0"]).unwrap();
        match cli.command {
            Command::Update { version, .. } => assert_eq!(version, Some("0.2.0".to_string())),
            _ => panic!("expected Command::Update"),
        }
    }

    #[test]
    fn server_flags_parse() {
        let cli = Cli::parse_from(["choco", "server", "stop", "--force"]);
        assert!(matches!(
            cli.command,
            Command::Server(ServerCmd::Stop { force: true })
        ));
        let cli = Cli::parse_from(["choco", "server", "restart", "--force", "--port", "0"]);
        assert!(matches!(
            cli.command,
            Command::Server(ServerCmd::Restart {
                force: true,
                port: Some(0)
            })
        ));
        let cli = Cli::parse_from(["choco", "server", "start", "--port", "4242"]);
        assert!(matches!(
            cli.command,
            Command::Server(ServerCmd::Start { port: Some(4242) })
        ));
        assert!(Cli::try_parse_from(["choco", "server", "start", "--port", "x"]).is_err());
    }

    #[test]
    fn base_url_is_none_without_flag_or_env() {
        // SAFETY-free check: only meaningful when the env var is unset.
        if std::env::var_os("CHOCO_BASE_URL").is_none() {
            let cli = Cli::parse_from(["choco", "project", "list"]);
            assert_eq!(cli.base_url, None);
        }
        let cli = Cli::parse_from(["choco", "--base-url", "http://x:1", "project", "list"]);
        assert_eq!(cli.base_url.as_deref(), Some("http://x:1"));
    }

    /// #92: `--resume` and `--fresh` are opposites, so asking for both is
    /// a parse error rather than one of them silently winning.
    #[test]
    fn retry_rejects_asking_to_resume_and_start_fresh_at_once() {
        let both = Cli::try_parse_from(["choco", "task", "retry", "t1", "--resume", "--fresh"]);
        assert!(both.is_err(), "both flags at once must not parse");

        let neither = Cli::parse_from(["choco", "task", "retry", "t1"]);
        let Command::Task(TaskCmd::Retry { resume, fresh, .. }) = neither.command else {
            panic!("expected a retry command");
        };
        assert!(!resume && !fresh, "neither flag means the daemon decides");

        let resumed = Cli::parse_from(["choco", "task", "retry", "t1", "--resume"]);
        let Command::Task(TaskCmd::Retry { resume, fresh, .. }) = resumed.command else {
            panic!("expected a retry command");
        };
        assert!(resume && !fresh);

        let forced_fresh = Cli::parse_from(["choco", "task", "retry", "t1", "--fresh"]);
        let Command::Task(TaskCmd::Retry { resume, fresh, .. }) = forced_fresh.command else {
            panic!("expected a retry command");
        };
        assert!(!resume && fresh);
    }

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
        assert!(args.required_sections.is_empty());
    }

    /// #95: repeatable and order-preserving, and a section name is prose —
    /// spaces, an arrow and a comma all have to survive the round trip,
    /// because whatever arrives here is matched against the report's own
    /// headings.
    #[test]
    fn mcp_serve_collects_repeated_require_section_flags() {
        let cli = Cli::parse_from([
            "choco",
            "mcp-serve",
            "--outcome",
            "approved",
            "--require-section",
            "Branches → tests",
            "--require-section",
            "Findings, blocking and minor",
        ]);
        let Command::McpServe(args) = cli.command else {
            panic!("expected McpServe");
        };
        assert_eq!(args.outcomes, vec!["approved"]);
        assert_eq!(
            args.required_sections,
            vec!["Branches → tests", "Findings, blocking and minor"]
        );
    }

    /// `#[command(hide = true)]` hides it from `--help` text; it must not
    /// also make the subcommand itself unparseable.
    #[test]
    fn mcp_serve_is_hidden_but_still_parses() {
        assert!(Cli::try_parse_from(["choco", "mcp-serve"]).is_ok());
    }
}
