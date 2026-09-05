//! `choco` (P1-10, design §6.2): a thin HTTP client against `chocofactoryd`'s
//! API, covering task create/status/send/list/events and project
//! create/list, with `--parent-task` support for delegation.

mod cli;
mod client;
mod mcp;
mod render;

use std::process::ExitCode;

use chocofactory_core::models::{Project, Task};
use clap::Parser;
use cli::{Cli, Command, ProjectCmd, RoleOverrideArgs, TaskCmd};
use client::{Client, ClientError, CreateTaskParams, EventsPage, RoleOverrides, build_task_config};
use serde_json::Value;

/// Borrows the parsed `--role-*`/`--config` flags in the shape
/// `build_task_config` wants, keeping `cli` free of any dependency on
/// `client`'s types.
fn role_overrides(args: &RoleOverrideArgs) -> RoleOverrides<'_> {
    RoleOverrides {
        role_cli: &args.role_cli,
        role_model: &args.role_model,
        role_system_prompt: &args.role_system_prompt,
        role_system_prompt_file: &args.role_system_prompt_file,
        config: args.config.as_deref(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Handled before `Client::new` and outside the `Output`/`run` path
    // entirely: serving the MCP tool must never depend on `chocofactoryd`
    // being reachable, and its stdio framing is its own protocol, not
    // something the `--json`/human-readable split applies to.
    if let Command::McpServe(args) = &cli.command {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        return match mcp::serve(&args.outcomes, stdin.lock(), stdout.lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

    let client = Client::new(cli.base_url);

    match run(&client, cli.command).await {
        Ok(output) => {
            let rendered = if cli.json {
                output.to_json()
            } else {
                output.to_human()
            };
            if let Some(rendered) = rendered {
                println!("{rendered}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// A command's result, held in typed form so it can be rendered either as
/// the daemon's raw JSON (`--json`, for agents and scripts) or as a
/// human-readable summary (the default).
enum Output {
    Project(Project),
    Projects(Vec<Project>),
    Task(Task),
    Tasks(Vec<Task>),
    /// The daemon's `TaskDetail` — kept as raw JSON because it has no
    /// exported Rust type, and passing it through verbatim is exactly what
    /// a delegating agent polling `workflow_state.current_stage` wants.
    TaskDetail(Value),
    Events(EventsPage),
    /// A 202-with-no-body call. Human mode still confirms it landed;
    /// `--json` stays silent so nothing has to parse a courtesy message.
    Accepted(String),
}

impl Output {
    fn to_json(&self) -> Option<String> {
        let value = match self {
            Output::Project(p) => serde_json::to_string(p),
            Output::Projects(p) => serde_json::to_string(p),
            Output::Task(t) => serde_json::to_string(t),
            Output::Tasks(t) => serde_json::to_string(t),
            Output::TaskDetail(d) => serde_json::to_string(d),
            Output::Events(e) => serde_json::to_string(e),
            Output::Accepted(_) => return None,
        };
        Some(value.expect("API models are always serializable"))
    }

    fn to_human(&self) -> Option<String> {
        Some(match self {
            Output::Project(p) => render::project(p),
            Output::Projects(p) => render::projects(p),
            Output::Task(t) => render::task(t),
            Output::Tasks(t) => render::tasks(t),
            Output::TaskDetail(d) => render::task_detail(d),
            Output::Events(e) => render::events(e),
            Output::Accepted(msg) => msg.clone(),
        })
    }
}

async fn run(client: &Client, command: Command) -> Result<Output, ClientError> {
    match command {
        Command::Project(ProjectCmd::Create { name }) => {
            Ok(Output::Project(client.create_project(&name).await?))
        }
        Command::Project(ProjectCmd::List) => Ok(Output::Projects(client.list_projects().await?)),
        Command::Task(TaskCmd::Create(args)) => {
            // Built before resolving the project so a malformed flag fails
            // immediately, without a lookup request first.
            let config = build_task_config(&role_overrides(&args.roles), args.repo.as_deref())?;
            let project_id = client.resolve_project(&args.project).await?;
            let task = client
                .create_task(&CreateTaskParams {
                    project_id: &project_id,
                    workflow_def: &args.workflow,
                    title: &args.title,
                    prompt: &args.prompt,
                    config,
                    parent_task_id: args.parent_task.as_deref(),
                })
                .await?;
            Ok(Output::Task(task))
        }
        Command::Task(TaskCmd::Reconfigure { id, roles }) => {
            if roles.is_empty() {
                return Err(ClientError::InvalidConfig(
                    "nothing to change — pass at least one --role-cli/--role-model/\
                     --role-system-prompt/--role-system-prompt-file/--config"
                        .to_string(),
                ));
            }
            // No `--repo` flag here: `reconfigure` is the per-role surface, and
            // moving a running task's working directory is a different
            // operation with different consequences. This is only the absence
            // of a convenience flag, not a guarantee — `--config '{"cwd":...}'`
            // still sets it, as does `PATCH /tasks/{id}` directly.
            //
            // `build_task_config` can still return `None` even though flags
            // were supplied — `--config '{}'` is the case — so that's reported
            // rather than unwrapped into a panic or sent as an empty patch.
            let config = build_task_config(&role_overrides(&roles), None)?.ok_or_else(|| {
                ClientError::InvalidConfig(
                    "the supplied config is empty — nothing would change".to_string(),
                )
            })?;
            Ok(Output::Task(client.update_task_config(&id, &config).await?))
        }
        Command::Task(TaskCmd::Status { id }) => {
            Ok(Output::TaskDetail(client.get_task(&id).await?))
        }
        Command::Task(TaskCmd::Send { id, text }) => {
            client.send_message(&id, &text).await?;
            Ok(Output::Accepted(format!(
                "Message accepted for task {id}. The reply is recorded as an \
                 event — see `choco task events {id}`."
            )))
        }
        Command::Task(TaskCmd::Cancel { id }) => {
            client.cancel_task(&id).await?;
            // Deliberately doesn't claim an agent was killed or a worktree
            // removed: a task parked on a `human_gate`, or one cancelled
            // before it ever started, has neither, and the 202 carries no
            // body saying which happened. Naming what is certainly true and
            // pointing at `status` beats a confident sentence that is
            // sometimes wrong.
            Ok(Output::Accepted(format!(
                "Task {id} cancelled. Any running agent process and worktree \
                 have been cleaned up — see `choco task status {id}`."
            )))
        }
        Command::Task(TaskCmd::List { project, status }) => {
            // Resolved the same way as `task create`, so a name works in
            // both places rather than only where a task is born.
            let project_id = match project {
                Some(project) => Some(client.resolve_project(&project).await?),
                None => None,
            };
            let tasks = client
                .list_tasks(project_id.as_deref(), status.as_deref())
                .await?;
            Ok(Output::Tasks(tasks))
        }
        Command::Task(TaskCmd::Events { id, limit, after }) => Ok(Output::Events(
            client.list_events(&id, limit, after.as_deref()).await?,
        )),
        // `main` returns before ever constructing a `Client`/calling `run`
        // when `cli.command` is `McpServe` — reachable only if that early
        // return is ever removed without updating this arm too.
        Command::McpServe(_) => unreachable!("McpServe is handled in main() before run()"),
    }
}
