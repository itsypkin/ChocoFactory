//! `choco dashboard` (#164): an interactive terminal view of every task.
//!
//! `app` holds the state and a pure `update`, `view` draws it, and this file
//! does the I/O: the terminal, the key-reading thread, the HTTP polls, the
//! per-task event socket and the actions.

pub mod app;
pub mod view;

use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chocofactory_core::models::RetryMode;
use chrono::Utc;
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::cli::DashboardArgs;
use crate::client::Client;
use app::{ActionKind, ActionOk, App, Effect, ListResult, Msg, Scope, SocketMsg, update};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What the loop needs besides the app and the keys.
pub struct LoopConfig {
    /// How often to redraw (the timers count between polls).
    pub tick: Duration,
    pub closed: usize,
    /// `Some` in one-project mode.
    pub project_id: Option<String>,
    /// Longest any one request may take before it is reported as failed
    /// (a hung daemon must never freeze the board or swallow an action).
    pub timeout: Duration,
    /// Longest a list poll may take. At most the poll interval, so the outage banner's "retrying every <interval>" stays true
    /// even when the daemon accepts a connection and never answers.
    pub poll_timeout: Duration,
    /// Tripped by the panic hook when any thread or task panics, so the loop
    /// stops instead of drawing on a terminal the hook already restored.
    pub panicked: Arc<PanicSignal>,
}

/// Set once from the panic hook; the loop watches it.
#[derive(Default)]
pub struct PanicSignal {
    tripped: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

impl PanicSignal {
    pub fn trip(&self) {
        self.tripped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Stores a permit when nobody waits yet, so the wake-up is not lost.
        self.wake.notify_one();
    }

    async fn tripped(&self) {
        loop {
            if self.tripped.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            self.wake.notified().await;
        }
    }
}

pub async fn run(base_url: String, args: DashboardArgs) -> ExitCode {
    // Its stderr version warning would corrupt the screen.
    let client = Arc::new(Client::new(base_url.clone()).without_version_check());

    let (scope, project_id) = match &args.project {
        None => (Scope::AllProjects, None),
        Some(p) => match resolve_project(&client, p).await {
            Ok((id, name)) => (Scope::Project { name }, Some(id)),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let mut app = App::new(
        scope,
        base_url,
        args.interval.duration,
        args.interval.raw.clone(),
        Utc::now(),
    );
    let config = LoopConfig {
        tick: Duration::from_secs(1),
        closed: args.closed,
        project_id,
        timeout: REQUEST_TIMEOUT,
        poll_timeout: args.interval.duration.min(REQUEST_TIMEOUT),
        panicked: Arc::new(PanicSignal::default()),
    };

    let mut terminal = match enter_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let hook = install_panic_hook(Arc::clone(&config.panicked));
    let mut keys = spawn_key_reader();
    let result = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut keys,
        &mut |_| {},
    )
    .await;
    restore_terminal();
    restore_panic_hook(hook);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves `--project` to `(id, name)`, naming the project even when an id
/// was given.
async fn resolve_project(
    client: &Client,
    name_or_id: &str,
) -> Result<(String, String), crate::client::ClientError> {
    let id = client.resolve_project(name_or_id).await?;
    let projects = client.list_projects().await?;
    let name = projects
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.name)
        .unwrap_or_else(|| name_or_id.to_string());
    Ok((id, name))
}

// ---- terminal -----------------------------------------------------------

type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    // Mouse capture stays off: the terminal keeps its own selection.
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
}

/// Restores the terminal before the panic message prints, so it lands on
/// the normal screen instead of vanishing with the alternate one. A panic on
/// any thread also trips `signal`, which stops the loop.
fn install_panic_hook(signal: Arc<PanicSignal>) -> PanicHook {
    let prev: PanicHook = Arc::from(std::panic::take_hook());
    let chained = Arc::clone(&prev);
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        signal.trip();
        chained(info);
    }));
    prev
}

fn restore_panic_hook(prev: PanicHook) {
    std::panic::set_hook(Box::new(move |info| prev(info)));
}

fn spawn_key_reader() -> mpsc::UnboundedReceiver<KeyEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if let TermEvent::Key(key) = ev
                && tx.send(key).is_err()
            {
                break;
            }
        }
    });
    rx
}

// ---- the loop -----------------------------------------------------------

/// Runs until `q`/Ctrl-C or the key channel closes. `on_frame` sees each
/// drawn frame (tests use it to read the screen).
pub async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    client: Arc<Client>,
    config: &LoopConfig,
    keys: &mut mpsc::UnboundedReceiver<KeyEvent>,
    on_frame: &mut dyn FnMut(&Buffer),
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut socket: Option<JoinHandle<()>> = None;

    // First data before the first draw: daemon version, project names (all
    // projects) and both lists.
    // A failure leaves the header on `daemon ?`; the list fetch below
    // reports an unreachable daemon on the status line.
    // Keys stay live meanwhile: with a hung daemon the user can still quit.
    let first = {
        let startup = async {
            if let Ok(status) = client
                .server_status(config.timeout.min(Duration::from_secs(5)))
                .await
            {
                app.daemon_version = Some(status.version);
            }
            fetch_bounded(&client, config, app.all_projects()).await
        };
        tokio::pin!(startup);
        loop {
            tokio::select! {
                first = &mut startup => break first,
                () = config.panicked.tripped() => return Err(PANICKED.to_string()),
                key = keys.recv() => match key {
                    Some(k) if is_quit(k) => return Ok(()),
                    Some(_) => {}
                    None => return Ok(()),
                },
            }
        }
    };
    for effect in update(app, Msg::List(Box::new(first))) {
        if run_effect(effect, &client, config, &tx, &mut socket) {
            abort(&mut socket);
            return Ok(());
        }
    }

    let mut tick = tokio::time::interval(config.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    draw(terminal, app, on_frame)?;
    loop {
        let msg = tokio::select! {
            key = keys.recv() => match key {
                Some(k) => Msg::Key(k),
                None => break,
            },
            Some(msg) = rx.recv() => msg,
            _ = tick.tick() => Msg::Tick(Utc::now()),
            () = config.panicked.tripped() => {
                abort(&mut socket);
                return Err(PANICKED.to_string());
            }
        };
        for effect in update(app, msg) {
            if run_effect(effect, &client, config, &tx, &mut socket) {
                abort(&mut socket);
                return Ok(());
            }
        }
        draw(terminal, app, on_frame)?;
    }
    abort(&mut socket);
    Ok(())
}

const PANICKED: &str = "stopped because a background task panicked (see the panic message above)";

fn is_quit(key: KeyEvent) -> bool {
    use ratatui::crossterm::event::KeyModifiers;
    key.kind != ratatui::crossterm::event::KeyEventKind::Release
        && (key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn timed_out() -> String {
    "request timed out".to_string()
}

async fn fetch_bounded(client: &Client, config: &LoopConfig, projects: bool) -> ListResult {
    tokio::time::timeout(
        config.poll_timeout.min(config.timeout),
        fetch(client, config, projects),
    )
    .await
    .unwrap_or_else(|_| ListResult {
        at: Utc::now(),
        active: Err(timed_out()),
        closed: Err(timed_out()),
        projects: None,
    })
}

fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &App,
    on_frame: &mut dyn FnMut(&Buffer),
) -> Result<(), String> {
    let done = terminal
        .draw(|f| view::draw(f, app))
        .map_err(|e| e.to_string())?;
    on_frame(done.buffer);
    Ok(())
}

fn abort(socket: &mut Option<JoinHandle<()>>) {
    if let Some(h) = socket.take() {
        h.abort();
    }
}

/// Carries out one effect. Returns `true` for `Quit`.
fn run_effect(
    effect: Effect,
    client: &Arc<Client>,
    config: &LoopConfig,
    tx: &mpsc::UnboundedSender<Msg>,
    socket: &mut Option<JoinHandle<()>>,
) -> bool {
    match effect {
        Effect::Quit => return true,
        Effect::Fetch { projects } => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let (closed, project_id, timeout) =
                (config.closed, config.project_id.clone(), config.timeout);
            let poll_timeout = config.poll_timeout;
            tokio::spawn(async move {
                let cfg = LoopConfig {
                    tick: Duration::ZERO,
                    closed,
                    project_id,
                    timeout,
                    poll_timeout,
                    panicked: Arc::default(),
                };
                let result = fetch_bounded(&client, &cfg, projects).await;
                let _ = tx.send(Msg::List(Box::new(result)));
            });
        }
        Effect::FetchDetail(id) => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                let result = tokio::time::timeout(timeout, client.get_task(&id))
                    .await
                    .map_err(|_| timed_out())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Detail { id, result });
            });
        }
        Effect::Cancel(id) => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                // Never `--keep`: keeping work is a CLI-only handover.
                let result = tokio::time::timeout(timeout, client.cancel_task(&id, false))
                    .await
                    .map_err(|_| timed_out())
                    .and_then(|r| r.map(|()| ActionOk::Cancelled).map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Action {
                    kind: ActionKind::Cancel,
                    id,
                    result,
                });
            });
        }
        Effect::Retry(id) => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                let result = tokio::time::timeout(timeout, client.retry_task(&id, RetryMode::Auto))
                    .await
                    .map_err(|_| timed_out())
                    .and_then(|r| r.map(ActionOk::Retried).map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Action {
                    kind: ActionKind::Retry,
                    id,
                    result,
                });
            });
        }
        Effect::OpenUrl(url) => open_url(url, tx),
        Effect::OpenSocket(id) => {
            abort(socket);
            *socket = Some(tokio::spawn(follow_events(
                client.base_url().to_string(),
                id,
                tx.clone(),
            )));
        }
        Effect::CloseSocket => abort(socket),
    }
    false
}

/// Both lists, and the project names when asked for.
async fn fetch(client: &Client, config: &LoopConfig, projects: bool) -> ListResult {
    let pid = config.project_id.as_deref();
    let active = client.list_task_summaries(pid, "open,stuck", None, None);
    // `--closed 0` shows no closed section rows: skip the request.
    let closed = async {
        if config.closed == 0 {
            Ok(Vec::new())
        } else {
            client
                .list_task_summaries(
                    pid,
                    "closed,cancelled",
                    Some("updated_desc"),
                    Some(config.closed),
                )
                .await
        }
    };
    let names = async {
        if projects {
            Some(client.list_projects().await.map_err(|e| e.to_string()))
        } else {
            None
        }
    };
    let (active, closed, projects) = tokio::join!(active, closed, names);
    ListResult {
        at: Utc::now(),
        active: active.map_err(|e| e.to_string()),
        closed: closed.map_err(|e| e.to_string()),
        projects,
    }
}

fn open_url(url: String, tx: &mpsc::UnboundedSender<Msg>) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let spawned = std::process::Command::new(program)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        // Reaped off-thread so the child never lingers as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            let _ = tx.send(Msg::OpenFailed {
                url,
                error: e.to_string(),
            });
        }
    }
}

/// Streams a task's last 200 events, reconnecting with a 1 s to 5 s backoff.
async fn follow_events(base_url: String, id: String, tx: mpsc::UnboundedSender<Msg>) {
    let ws_base = match base_url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some((_, rest)) => format!("ws://{rest}"),
        None => format!("ws://{base_url}"),
    };
    let url = format!("{ws_base}/tasks/{id}/events/live?tail=200");
    let send = |msg| {
        tx.send(Msg::Socket {
            id: id.clone(),
            msg,
        })
        .is_ok()
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(url.as_str()).await {
            backoff = Duration::from_secs(1);
            if !send(SocketMsg::Connected) {
                return;
            }
            while let Some(Ok(msg)) = ws.next().await {
                if let WsMessage::Text(text) = msg
                    && let Ok(event) = serde_json::from_str(&text.to_string())
                    && !send(SocketMsg::Event(Box::new(event)))
                {
                    return;
                }
            }
        }
        if !send(SocketMsg::Down) {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests;
