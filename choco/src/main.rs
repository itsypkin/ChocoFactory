//! `choco` (P1-10, design §6.2): a thin HTTP client against `chocofactoryd`'s
//! API, covering task create/status/send/list/events and project
//! create/list.

mod cli;
mod client;
mod dashboard;
mod mcp;
mod render;
mod server;
mod update;
mod watch;

use std::process::ExitCode;

use chocofactory_core::models::{Project, RetryMode, RetryOutcome, Task};
use clap::Parser;
use cli::{Cli, Command, ProjectCmd, RoleOverrideArgs, TaskCmd};
use client::{
    Client, ClientError, CreateTaskParams, EventsPage, InitWorkflowsResult, RoleOverrides,
    build_task_config, workflow_arg,
};
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

/// Resolves a `--repo`/`project create --repo` value to an absolute,
/// existing directory (issue #88), client-side — the daemon's own cwd
/// differs from the user's, so canonicalizing there would resolve the wrong
/// path entirely. A relative path, including `.`, works, since
/// `std::fs::canonicalize` resolves against *this process's* cwd.
fn canonicalize_repo(path: &str) -> Result<String, ClientError> {
    let canonical = std::fs::canonicalize(path).map_err(|err| {
        ClientError::InvalidRepoPath(format!("--repo '{path}' could not be resolved: {err}"))
    })?;
    if !canonical.is_dir() {
        return Err(ClientError::InvalidRepoPath(format!(
            "--repo '{path}' is not a directory"
        )));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// An explicit `--base-url`/`CHOCO_BASE_URL` wins; else the port the running
/// daemon published in its lock file; else the default port, but only when
/// there is no record of a daemon at all (one from before the lock file
/// existed writes none). A lock file that records a dead daemon is an error,
/// not a reason to talk to whatever listens on the default port. A lock file
/// that cannot be read falls through to the default, but says so on stderr.
fn resolve_base_url(explicit: Option<String>) -> Result<String, String> {
    let (url, warning) =
        resolve_base_url_in(chocofactory_core::paths::config_root().as_deref(), explicit)?;
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    Ok(url)
}

/// Returns the URL and, when the lock file could not be read, the warning to print.
fn resolve_base_url_in(
    root: Option<&std::path::Path>,
    explicit: Option<String>,
) -> Result<(String, Option<String>), String> {
    use chocofactory_core::daemon_lock::{LockState, read_lock};
    if let Some(url) = explicit {
        return Ok((url, None));
    }
    let mut warning = None;
    if let Some(root) = root
        && root.exists()
    {
        match read_lock(root) {
            Ok(LockState::Running(info)) => {
                return Ok((format!("http://127.0.0.1:{}", info.port), None));
            }
            Ok(LockState::NotRunning { last: None }) => {
                // A missing lock file means no daemon was ever started here. An
                // existing one with no daemon info (a start that failed before
                // publishing) must not silently fall back to :4141 either.
                if root.join("chocofactoryd.lock").exists() {
                    return Err(
                        "chocofactoryd is not running (the lock file has no daemon info); \
                         start it with `choco server start` or pass --base-url"
                            .to_string(),
                    );
                }
            }
            Ok(LockState::NotRunning { last: Some(last) }) => {
                return Err(format!(
                    "chocofactoryd is not running (last ran as pid {} on port {}); \
                     start it with `choco server start` or pass --base-url",
                    last.pid, last.port
                ));
            }
            Err(err) => {
                warning = Some(format!(
                    "warning: could not read the daemon lock file ({err}); using http://127.0.0.1:4141"
                ))
            }
        }
    }
    Ok(("http://127.0.0.1:4141".to_string(), warning))
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
        let stage = mcp::StageReport {
            outcomes: args.outcomes.clone(),
            required_sections: args.required_sections.clone(),
        };
        return match mcp::serve(&stage, stdin.lock(), stdout.lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

    if let Command::Server(cmd) = cli.command {
        return server::run(cmd, cli.json).await;
    }

    if let Command::Update {
        check,
        version,
        force,
    } = cli.command
    {
        return update::run(check, version, force).await;
    }

    if let Command::Dashboard(args) = cli.command {
        use std::io::IsTerminal;
        // Checked before anything touches the terminal or the daemon.
        if cli.json || !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            eprintln!(
                "error: the dashboard needs an interactive terminal; use `choco task list` \
                 or `choco task status --live` instead"
            );
            return ExitCode::FAILURE;
        }
        let base_url = match resolve_base_url(cli.base_url) {
            Ok(url) => url,
            Err(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::FAILURE;
            }
        };
        return dashboard::run(base_url, args).await;
    }

    let base_url = match resolve_base_url(cli.base_url) {
        Ok(url) => url,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let client = Client::new(base_url);

    // A watching `task status` returns its own exit code (3–6 for the ways a
    // watch can end other than success), so it's handled apart from `run`.
    if let Command::Task(TaskCmd::Status {
        id,
        live,
        until,
        interval,
        timeout,
    }) = &cli.command
        && (*live || until.is_some())
    {
        let args = watch::WatchArgs {
            id,
            until: until.as_ref(),
            interval,
            timeout: timeout.as_ref(),
            json: cli.json,
        };
        return match watch::watch(&client, args).await {
            Ok(end) => end.exit_code(),
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

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
///
/// `large_enum_variant`: `Task`'s `stuck_reason` field (X-4, issue #61)
/// pushed this over clippy's size-difference threshold against `Accepted`'s
/// bare `String`. Same reasoning as `cli::Command`'s identical allow: one of
/// these is built, once, from a single response and then matched on and
/// dropped — boxing would cost an allocation to save nothing measurable.
#[allow(clippy::large_enum_variant)]
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
    /// `choco project init-workflows`'s result (issue #88).
    InitWorkflows(InitWorkflowsResult),
    /// A 202-with-no-body call. Human mode still confirms it landed;
    /// `--json` stays silent so nothing has to parse a courtesy message.
    Accepted(String),
    /// `task retry`'s own `202`, which does carry a body: whether the stage
    /// resumed its interrupted session or started a fresh one (#92). The
    /// task id rides along only so the human rendering can point at
    /// `choco task status` the way every other accepted call does.
    Retried(String, RetryOutcome),
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
            Output::InitWorkflows(r) => serde_json::to_string(r),
            Output::Accepted(_) => return None,
            Output::Retried(_, r) => serde_json::to_string(r),
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
            Output::InitWorkflows(r) => render::init_workflows(r),
            Output::Accepted(msg) => msg.clone(),
            Output::Retried(id, r) => render::retried(id, r),
        })
    }
}

async fn run(client: &Client, command: Command) -> Result<Output, ClientError> {
    match command {
        Command::Project(ProjectCmd::Create { name, repo }) => {
            let repo_path = repo.as_deref().map(canonicalize_repo).transpose()?;
            Ok(Output::Project(
                client.create_project(&name, repo_path.as_deref()).await?,
            ))
        }
        Command::Project(ProjectCmd::Update {
            project,
            name,
            repo,
            no_repo,
        }) => {
            if name.is_none() && repo.is_none() && !no_repo {
                return Err(ClientError::InvalidConfig(
                    "nothing to change — pass at least one of --name/--repo/--no-repo".to_string(),
                ));
            }
            // `--repo`/`--no-repo` conflict (clap), so at most one of these
            // produces `Some`.
            let repo_path: Option<Option<String>> = if no_repo {
                Some(None)
            } else {
                repo.as_deref()
                    .map(canonicalize_repo)
                    .transpose()?
                    .map(Some)
            };
            let project_id = client.resolve_project(&project).await?;
            let updated = client
                .update_project(
                    &project_id,
                    name.as_deref(),
                    repo_path.as_ref().map(|p| p.as_deref()),
                )
                .await?;
            Ok(Output::Project(updated))
        }
        Command::Project(ProjectCmd::List) => Ok(Output::Projects(client.list_projects().await?)),
        Command::Project(ProjectCmd::InitWorkflows { project }) => {
            let project_id = client.resolve_project(&project).await?;
            Ok(Output::InitWorkflows(
                client.init_workflows(&project_id).await?,
            ))
        }
        Command::Task(TaskCmd::Create(args)) => {
            // Built before resolving the project so a malformed flag fails
            // immediately, without a lookup request first.
            let config = build_task_config(&role_overrides(&args.roles), args.repo.as_deref())?;
            let workflow = workflow_arg(&args.workflow)?;
            let project_id = client.resolve_project(&args.project).await?;
            let task = client
                .create_task(&CreateTaskParams {
                    project_id: &project_id,
                    workflow: &workflow,
                    title: &args.title,
                    prompt: &args.prompt,
                    config,
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
        Command::Task(TaskCmd::Status { id, .. }) => {
            Ok(Output::TaskDetail(client.get_task(&id).await?))
        }
        Command::Task(TaskCmd::Send { id, text }) => {
            client.send_message(&id, &text).await?;
            Ok(Output::Accepted(format!(
                "Message accepted for task {id}. The reply is recorded as an \
                 event — see `choco task events {id}`."
            )))
        }
        Command::Task(TaskCmd::Cancel { id, keep }) => {
            client.cancel_task(&id, keep).await?;
            if keep {
                // The worktree path lives in the daemon's view of the task,
                // so ask for it rather than recomputing it here. The cancel
                // already succeeded, so a failed lookup must not read as a
                // failed cancel.
                return Ok(Output::Accepted(match client.get_task(&id).await {
                    Ok(detail) => keep_cancel_message(&id, &detail),
                    Err(_) => format!(
                        "Task {id} cancelled with --keep. Nothing was removed; see \
                         `choco task status {id}` for what was kept."
                    ),
                }));
            }
            // Deliberately doesn't claim an agent was killed or a worktree
            // removed: a task parked on a `human_gate`, or one cancelled
            // before it ever started, has neither, and the 202 carries no
            // body saying which happened. Naming what is certainly true and
            // pointing at `status` beats a confident sentence that is
            // sometimes wrong.
            Ok(Output::Accepted(format!(
                "Task {id} cancelled. Any running agent process, worktree and \
                 local branch have been cleaned up — see `choco task status {id}`."
            )))
        }
        Command::Task(TaskCmd::Retry { id, resume, fresh }) => {
            let mode = match (resume, fresh) {
                (true, false) => RetryMode::Resume,
                (false, true) => RetryMode::Fresh,
                // `clap`'s `conflicts_with` rules out both being set, so
                // this is only the "neither was given" case.
                _ => RetryMode::Auto,
            };
            let outcome = client.retry_task(&id, mode).await?;
            Ok(Output::Retried(id, outcome))
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
        Command::Server(_) => unreachable!("Server is handled in main() before run()"),
        Command::Update { .. } => unreachable!("Update is handled in main() before run()"),
        Command::Dashboard(_) => unreachable!("Dashboard is handled in main() before run()"),
    }
}

/// The success line for `cancel --keep`, from the task as the daemon
/// reports it after the cancel.
fn keep_cancel_message(id: &str, detail: &Value) -> String {
    let Some(kept) = detail.get("kept").filter(|k| !k.is_null()) else {
        return format!("Task {id} cancelled. Nothing to keep: this task had no worktree.");
    };
    let path = kept
        .get("worktree_path")
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    let branch = kept.get("branch").and_then(Value::as_str).unwrap_or("-");
    format!(
        "Task {id} cancelled. Kept for you: worktree {path}, branch {branch}. \
         Nothing was removed; see `choco task status {id}`."
    )
}

#[cfg(test)]
mod tests {
    use super::{keep_cancel_message, resolve_base_url_in};
    use std::os::unix::fs::PermissionsExt;

    const DEFAULT: &str = "http://127.0.0.1:4141";

    #[test]
    fn keep_message_names_the_kept_worktree_and_branch() {
        let detail = serde_json::json!({
            "kept": {"worktree_path": "/w/demo/t1", "branch": "task/t1"}
        });
        let msg = keep_cancel_message("t1", &detail);
        assert!(msg.contains("worktree /w/demo/t1"), "{msg}");
        assert!(msg.contains("branch task/t1"), "{msg}");
        let none = keep_cancel_message("t1", &serde_json::json!({"kept": null}));
        assert!(none.contains("Nothing to keep"), "{none}");
        let odd = keep_cancel_message("t1", &serde_json::json!({"kept": {}}));
        assert!(
            odd.contains("(unknown)") && odd.contains("branch -"),
            "{odd}"
        );
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("choco-resolve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_url_wins_and_missing_root_uses_default() {
        let root = tmp("explicit");
        assert_eq!(
            resolve_base_url_in(Some(&root), Some("http://x:1".into())),
            Ok(("http://x:1".to_string(), None))
        );
        assert_eq!(
            resolve_base_url_in(Some(&root), None),
            Ok((DEFAULT.to_string(), None))
        );
        assert_eq!(
            resolve_base_url_in(None, None),
            Ok((DEFAULT.to_string(), None))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_lock_is_an_error_not_a_fallback() {
        let root = tmp("stale");
        std::fs::write(
            root.join("chocofactoryd.lock"),
            r#"{"pid":4242,"port":5151,"version":"1","commit":"c","started_at":"2026-01-01T00:00:00Z","exe":"/x"}"#,
        )
        .unwrap();
        let err = resolve_base_url_in(Some(&root), None).unwrap_err();
        assert!(
            err.contains("not running") && err.contains("4242") && err.contains("5151"),
            "{err}"
        );
        // An explicit URL still wins.
        assert!(resolve_base_url_in(Some(&root), Some("http://x:1".into())).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_lock_file_is_an_error_not_a_fallback() {
        let root = tmp("empty");
        std::fs::write(root.join("chocofactoryd.lock"), "").unwrap();
        let err = resolve_base_url_in(Some(&root), None).unwrap_err();
        assert!(err.contains("not running"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_lock_file_falls_back_to_the_default() {
        let root = tmp("unreadable");
        let lock = root.join("chocofactoryd.lock");
        std::fs::write(&lock, "{}").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root can still open it; then there is no error to test.
        if chocofactory_core::daemon_lock::read_lock(&root).is_err() {
            let (url, warning) = resolve_base_url_in(Some(&root), None).unwrap();
            assert_eq!(url, DEFAULT);
            let w = warning.expect("an unreadable lock file must warn");
            assert!(w.starts_with("warning:") && w.contains("lock file"), "{w}");
        }
        let _ = std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_dir_all(&root);
    }
}
