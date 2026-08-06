//! `choco` (P1-10, design §6.2): a thin HTTP client against `chokofactoryd`'s
//! API, covering task create/status/send/list/events and project
//! create/list, with `--parent-task` support for delegation.

mod cli;
mod client;
mod render;

use std::process::ExitCode;

use chokofactory_core::models::{Project, Task};
use clap::Parser;
use cli::{Cli, Command, ProjectCmd, TaskCmd};
use client::{Client, ClientError, EventsPage};
use serde_json::Value;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
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
            let project_id = client.resolve_project(&args.project).await?;
            let task = client
                .create_task(
                    &project_id,
                    &args.workflow,
                    &args.title,
                    &args.prompt,
                    args.repo.as_deref(),
                    args.parent_task.as_deref(),
                )
                .await?;
            Ok(Output::Task(task))
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
    }
}
