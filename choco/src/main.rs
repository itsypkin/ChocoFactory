//! `choco` (P1-10, design §6.2): a thin HTTP client against `chokofactoryd`'s
//! API, covering task create/status/send/list and project create/list, with
//! `--parent-task` support for delegation.

mod cli;
mod client;

use std::process::ExitCode;

use clap::Parser;
use cli::{Cli, Command, ProjectCmd, TaskCmd};
use client::{Client, ClientError};
use serde_json::to_string;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let client = Client::new(cli.base_url);

    match run(&client, cli.command).await {
        Ok(Some(output)) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one subcommand, returning the JSON to print on success (`None` for
/// commands with no body, e.g. `task send`).
async fn run(client: &Client, command: Command) -> Result<Option<String>, ClientError> {
    match command {
        Command::Project(ProjectCmd::Create { name }) => {
            let project = client.create_project(&name).await?;
            Ok(Some(to_json(&project)))
        }
        Command::Project(ProjectCmd::List) => {
            let projects = client.list_projects().await?;
            Ok(Some(to_json(&projects)))
        }
        Command::Task(TaskCmd::Create(args)) => {
            let task = client
                .create_task(
                    &args.project,
                    &args.workflow,
                    &args.title,
                    &args.prompt,
                    args.repo.as_deref(),
                    args.parent_task.as_deref(),
                )
                .await?;
            Ok(Some(to_json(&task)))
        }
        Command::Task(TaskCmd::Status { id }) => {
            let detail = client.get_task(&id).await?;
            Ok(Some(to_json(&detail)))
        }
        Command::Task(TaskCmd::Send { id, text }) => {
            client.send_message(&id, &text).await?;
            Ok(None)
        }
        Command::Task(TaskCmd::List { project, status }) => {
            let tasks = client
                .list_tasks(project.as_deref(), status.as_deref())
                .await?;
            Ok(Some(to_json(&tasks)))
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    to_string(value).expect("serde_json::Value/Project/Task are always serializable")
}
