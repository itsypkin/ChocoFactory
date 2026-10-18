//! `choco dashboard` (#164): an interactive terminal view of every task.
//!
//! `app` holds the state and a pure `update`, `view` draws it, and this file
//! does the I/O: the terminal, the key-reading thread, the HTTP polls, the
//! per-task event socket and the actions.

pub mod app;
pub mod search;
pub mod view;

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
use app::{
    ActionKind, ActionOk, App, EditOutcome, Effect, ListResult, Msg, Scope, SendFailure, SocketMsg,
    exit_report, update, worth_keeping,
};

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
    /// Longest a list poll after the first load may take (see
    /// [`poll_timeout_for`]). Polls never overlap and the next starts one
    /// interval after the last ended, so "retrying every <interval>" stays
    /// true even when the daemon accepts a connection and never answers.
    /// The first load uses `timeout` instead.
    pub poll_timeout: Duration,
    /// Tripped by the panic hook when any thread or task panics, so the loop
    /// stops instead of drawing on a terminal the hook already restored.
    pub panicked: Arc<PanicSignal>,
    /// The shell that runs the editor: `<shell> -c '<editor> "$@"' …`.
    pub shell: PathBuf,
    /// Where message files for `s` are made.
    pub message_dir: PathBuf,
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

    #[cfg(test)]
    pub(crate) fn is_tripped(&self) -> bool {
        self.tripped.load(std::sync::atomic::Ordering::SeqCst)
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
        poll_timeout: poll_timeout_for(args.interval.duration),
        panicked: Arc::new(PanicSignal::default()),
        shell: PathBuf::from("/bin/sh"),
        message_dir: std::env::temp_dir(),
    };

    let mut terminal = match enter_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let hook = install_panic_hook(Arc::clone(&config.panicked));
    let (mut keys, reader) = spawn_key_reader(CrosstermSource);
    let mut handover = TerminalHandover { reader };
    let result = run_loop(
        &mut terminal,
        &mut app,
        client,
        &config,
        &mut keys,
        &mut handover,
        &mut |_| {},
    )
    .await;
    restore_terminal();
    restore_panic_hook(hook);
    // Message files that were kept or may be in flight, on the normal screen.
    for line in exit_report(&app) {
        eprintln!("{line}");
    }
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
    install_panic_hook_with(signal, restore_terminal)
}

/// The shortest a list poll is given, so a short `--interval` against a slow
/// daemon still shows data.
const MIN_POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a list poll may take: the poll interval, but at least
/// [`MIN_POLL_TIMEOUT`] and at most the general request timeout. Polls never
/// overlap (`fetching`) and the next one starts one interval after the last
/// ended, which is what the outage line's "retrying every <interval>" means.
pub(crate) fn poll_timeout_for(interval: Duration) -> Duration {
    interval.clamp(MIN_POLL_TIMEOUT, REQUEST_TIMEOUT)
}

fn install_panic_hook_with(signal: Arc<PanicSignal>, restore: fn()) -> PanicHook {
    let prev: PanicHook = Arc::from(std::panic::take_hook());
    let chained = Arc::clone(&prev);
    std::panic::set_hook(Box::new(move |info| {
        restore();
        signal.trip();
        chained(info);
    }));
    prev
}

fn restore_panic_hook(prev: PanicHook) {
    std::panic::set_hook(Box::new(move |info| prev(info)));
}

/// Where terminal events come from; a test can inject a fake.
pub trait KeySource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<TermEvent>;
}

/// The real terminal.
pub struct CrosstermSource;

impl KeySource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }
    fn read(&mut self) -> io::Result<TermEvent> {
        event::read()
    }
}

#[derive(Default)]
struct ReaderState {
    pause_requested: bool,
    parked: bool,
    exited: bool,
}

/// Pauses and resumes the key-reading thread. Even `poll` reads bytes from
/// the tty, so a paused reader makes neither call.
#[derive(Clone, Default)]
pub struct ReaderControl(Arc<(Mutex<ReaderState>, Condvar)>);

fn lock_reader(m: &Mutex<ReaderState>) -> std::sync::MutexGuard<'_, ReaderState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ReaderControl {
    /// Returns once the reader has stopped touching the terminal, or has
    /// exited (a dead reader reads nothing, so this never waits on it).
    pub fn pause(&self) {
        let (m, cv) = &*self.0;
        let mut st = lock_reader(m);
        st.pause_requested = true;
        cv.notify_all();
        while !st.parked && !st.exited {
            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Lets a paused reader go on. Does nothing when no pause is in effect.
    pub fn resume(&self) {
        let (m, cv) = &*self.0;
        let mut st = lock_reader(m);
        st.pause_requested = false;
        st.parked = false;
        cv.notify_all();
    }

    /// Parks while a pause is requested. `false` once the reader has exited.
    fn gate(&self) -> bool {
        let (m, cv) = &*self.0;
        let mut st = lock_reader(m);
        while st.pause_requested && !st.exited {
            st.parked = true;
            cv.notify_all();
            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        !st.exited
    }

    fn mark_exited(&self) {
        let (m, cv) = &*self.0;
        lock_reader(m).exited = true;
        cv.notify_all();
    }
}

pub fn spawn_key_reader(
    mut source: impl KeySource + Send + 'static,
) -> (mpsc::UnboundedReceiver<KeyEvent>, ReaderControl) {
    let (tx, rx) = mpsc::unbounded_channel();
    let control = ReaderControl::default();
    let thread_control = control.clone();
    std::thread::spawn(move || {
        let c = thread_control;
        loop {
            if !c.gate() {
                break;
            }
            match source.poll(Duration::from_millis(50)) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => break,
            }
            if !c.gate() {
                break;
            }
            match source.read() {
                Ok(TermEvent::Key(key)) => {
                    if tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        c.mark_exited();
    });
    (rx, control)
}

/// Gives the terminal to the editor and takes it back.
pub trait Handover {
    fn suspend(&mut self) -> io::Result<()>;
    fn resume(&mut self) -> io::Result<()>;
}

struct TerminalHandover {
    reader: ReaderControl,
}

impl Handover for TerminalHandover {
    fn suspend(&mut self) -> io::Result<()> {
        self.reader.pause();
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            ratatui::crossterm::cursor::Show
        )
    }

    fn resume(&mut self) -> io::Result<()> {
        let screen = execute!(io::stdout(), EnterAlternateScreen);
        let raw = enable_raw_mode();
        self.reader.resume();
        screen.and(raw)
    }
}

// ---- ignoring Ctrl-C while the editor runs -------------------------------

struct SigState {
    count: usize,
    saved: Option<[libc::sigaction; 2]>,
}

static SIGNALS: Mutex<SigState> = Mutex::new(SigState {
    count: 0,
    saved: None,
});

const GUARDED: [libc::c_int; 2] = [libc::SIGINT, libc::SIGQUIT];

/// While held, this process ignores SIGINT and SIGQUIT (the editor, in the
/// same foreground group, gets them). Process-wide and counted: the last
/// release restores what the first acquire saw.
struct SigintIgnore {
    released: bool,
}

impl SigintIgnore {
    fn acquire() -> io::Result<Self> {
        let mut st = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        if st.count == 0 {
            // SAFETY: plain sigaction calls on zeroed, fully set structs.
            unsafe {
                let mut saved: [libc::sigaction; 2] = std::mem::zeroed();
                let mut ignore: libc::sigaction = std::mem::zeroed();
                ignore.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut ignore.sa_mask);
                for (i, sig) in GUARDED.iter().enumerate() {
                    if libc::sigaction(*sig, &ignore, &mut saved[i]) != 0 {
                        let err = io::Error::last_os_error();
                        for j in 0..i {
                            libc::sigaction(GUARDED[j], &saved[j], std::ptr::null_mut());
                        }
                        return Err(err);
                    }
                }
                st.saved = Some(saved);
            }
        }
        st.count += 1;
        Ok(SigintIgnore { released: false })
    }

    fn release(mut self) -> io::Result<()> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        let mut st = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        st.count -= 1;
        if st.count > 0 {
            return Ok(());
        }
        let Some(saved) = st.saved.take() else {
            return Ok(());
        };
        let mut first = None;
        for (i, sig) in GUARDED.iter().enumerate() {
            // SAFETY: restores the structs `acquire` read back.
            if unsafe { libc::sigaction(*sig, &saved[i], std::ptr::null_mut()) } != 0
                && first.is_none()
            {
                first = Some(io::Error::last_os_error());
            }
        }
        first.map_or(Ok(()), Err)
    }
}

impl Drop for SigintIgnore {
    fn drop(&mut self) {
        // Only reached unreleased when unwinding; nothing can report it then.
        let _ = self.release_inner();
    }
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
    handover: &mut dyn Handover,
    on_frame: &mut dyn FnMut(&Buffer),
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut rt = Runtime {
        client,
        config,
        tx,
        socket: None,
        search_load: None,
    };
    let client = Arc::clone(&rt.client);

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
            // The first load gets the full request timeout: a short
            // `--interval` must not stop a slow daemon's first answer.
            fetch_bounded(&client, config.timeout, config, app.all_projects()).await
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
    let effects = update(app, Msg::List(Box::new(first)));
    let quit = apply_effects(effects, terminal, app, handover, &mut rt).await;
    if quit.is_err() || matches!(quit, Ok(true)) {
        abort(&mut rt.socket);
        return quit.map(|_| ());
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
                abort(&mut rt.socket);
                return Err(PANICKED.to_string());
            }
        };
        let effects = update(app, msg);
        let quit = apply_effects(effects, terminal, app, handover, &mut rt).await;
        if quit.is_err() || matches!(quit, Ok(true)) {
            abort(&mut rt.socket);
            return quit.map(|_| ());
        }
        draw(terminal, app, on_frame)?;
    }
    abort(&mut rt.socket);
    Ok(())
}

/// What carrying out effects needs.
struct Runtime<'a> {
    client: Arc<Client>,
    config: &'a LoopConfig,
    tx: mpsc::UnboundedSender<Msg>,
    socket: Option<JoinHandle<()>>,
    search_load: Option<JoinHandle<()>>,
}

/// Carries out effects in order. `Compose` is awaited here, in place: no
/// frame is drawn and no key is read while the editor runs, and its result
/// goes straight to `update`, whose effects join the queue. Returns `true`
/// for `Quit`.
async fn apply_effects<B: Backend>(
    effects: Vec<Effect>,
    terminal: &mut Terminal<B>,
    app: &mut App,
    handover: &mut dyn Handover,
    rt: &mut Runtime<'_>,
) -> Result<bool, String> {
    let mut queue: VecDeque<Effect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        match effect {
            Effect::Compose { id } => {
                let msg = match app.editor.clone() {
                    Some(editor) => compose(terminal, handover, rt.config, id, editor).await?,
                    None => Msg::Edited {
                        id,
                        path: PathBuf::new(),
                        outcome: EditOutcome::CreateFailed("no editor is set".into()),
                    },
                };
                queue.extend(update(app, msg));
            }
            other => {
                if run_effect(
                    other,
                    &rt.client,
                    rt.config,
                    &rt.tx,
                    &mut rt.socket,
                    &mut rt.search_load,
                ) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

static MESSAGE_FILES: AtomicU64 = AtomicU64::new(0);

/// How the editor child ended.
enum Ran {
    Handover(String),
    Spawn(String),
    WaitFailed(String),
    Exited(std::process::ExitStatus),
}

/// The editor round trip for `s`: make the file, hand the terminal over, run
/// the editor, take the terminal back (always), read the file. `Err` only
/// when the terminal could not be taken back.
async fn compose<B: Backend>(
    terminal: &mut Terminal<B>,
    handover: &mut dyn Handover,
    config: &LoopConfig,
    id: String,
    editor: String,
) -> Result<Msg, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let short: String = id.chars().take(8).collect();
    // A name already taken (a stale kept file, a planted file) is skipped,
    // never reused: `create_new` fails on it and the next number is tried.
    let mut path;
    let mut attempts = 0;
    loop {
        let n = MESSAGE_FILES.fetch_add(1, Ordering::Relaxed);
        path = config.message_dir.join(format!(
            "choco-message-{short}-{}-{n}.md",
            std::process::id()
        ));
        attempts += 1;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 100 => {}
            Err(e) => {
                return Ok(Msg::Edited {
                    id,
                    path,
                    outcome: EditOutcome::CreateFailed(e.to_string()),
                });
            }
        }
    }
    let mut back_err: Option<String> = None;

    let ran = match handover.suspend() {
        Err(e) => Ran::Handover(e.to_string()),
        Ok(()) => match SigintIgnore::acquire() {
            Err(e) => Ran::Handover(e.to_string()),
            Ok(guard) => {
                let ran = run_editor(config, &editor, &path).await;
                if let Err(e) = guard.release() {
                    back_err.get_or_insert(e.to_string());
                }
                ran
            }
        },
    };

    // Every way back passes here, with no early return above.
    if let Err(e) = handover.resume() {
        back_err.get_or_insert(e.to_string());
    }
    // Clear and redraw everything without `Terminal::clear`, which asks the
    // terminal for the cursor position and fails when it does not answer.
    let cleared = terminal
        .size()
        .and_then(|size| terminal.resize(ratatui::layout::Rect::from(size)));
    if let Err(e) = cleared {
        back_err.get_or_insert(e.to_string());
    }
    if let Some(e) = back_err {
        return Err(take_back_failed(&e, &path));
    }

    let outcome = match ran {
        Ran::Handover(e) => EditOutcome::HandoverFailed(e),
        Ran::Spawn(error) => EditOutcome::SpawnFailed { editor, error },
        Ran::WaitFailed(error) => {
            let content = std::fs::read_to_string(&path).map_err(|e| e.to_string());
            let exists = content.is_ok() || path.exists();
            EditOutcome::WaitFailed {
                error,
                content,
                exists,
            }
        }
        Ran::Exited(status) => {
            let content = std::fs::read_to_string(&path).map_err(|e| e.to_string());
            let exists = content.is_ok() || path.exists();
            EditOutcome::Exited {
                status,
                content,
                exists,
            }
        }
    };
    Ok(Msg::Edited { id, path, outcome })
}

async fn run_editor(config: &LoopConfig, editor: &str, path: &Path) -> Ran {
    let mut cmd = tokio::process::Command::new(&config.shell);
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(path);
    // SAFETY: only async-signal-safe `signal` calls run between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    match cmd.spawn() {
        Err(e) => Ran::Spawn(e.to_string()),
        Ok(mut child) => match child.wait().await {
            Ok(status) => Ran::Exited(status),
            Err(e) => Ran::WaitFailed(e.to_string()),
        },
    }
}

/// The error for a terminal that could not be taken back; it names the
/// message file when it holds text (the same rule `update` uses).
fn take_back_failed(error: &str, path: &Path) -> String {
    let head = format!("could not take the terminal back: {error}");
    match std::fs::read_to_string(path) {
        Ok(text) if worth_keeping(&text) => {
            format!("{head}; your unsent message is in {}", path.display())
        }
        Ok(_) => match std::fs::remove_file(path) {
            Ok(()) => head,
            Err(e) => format!("{head}; could not remove {}: {e}", path.display()),
        },
        Err(_) if path.exists() => {
            format!("{head}; your unsent message is in {}", path.display())
        }
        Err(_) => head,
    }
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

async fn fetch_bounded(
    client: &Client,
    limit: Duration,
    config: &LoopConfig,
    projects: bool,
) -> ListResult {
    tokio::time::timeout(limit, fetch(client, config, projects))
        .await
        .unwrap_or_else(|_| ListResult {
            at: Utc::now(),
            active: Err(timed_out()),
            closed: Err(timed_out()),
            projects: None,
            server: None,
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
    search_load: &mut Option<JoinHandle<()>>,
) -> bool {
    match effect {
        Effect::Quit => return true,
        Effect::Fetch { projects } => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let (closed, project_id, timeout) =
                (config.closed, config.project_id.clone(), config.timeout);
            let poll_timeout = config.poll_timeout;
            let (shell, message_dir) = (config.shell.clone(), config.message_dir.clone());
            tokio::spawn(async move {
                let cfg = LoopConfig {
                    tick: Duration::ZERO,
                    closed,
                    project_id,
                    timeout,
                    poll_timeout,
                    panicked: Arc::default(),
                    shell,
                    message_dir,
                };
                let result =
                    fetch_bounded(&client, poll_timeout.min(timeout), &cfg, projects).await;
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
        Effect::Pause(id) => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                // Never `--now`: stopping running work is a CLI-only choice.
                let result = tokio::time::timeout(timeout, client.pause_task(&id, false))
                    .await
                    .map_err(|_| timed_out())
                    .and_then(|r| r.map(ActionOk::Paused).map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Action {
                    kind: ActionKind::Pause,
                    id,
                    result,
                });
            });
        }
        Effect::Resume(id) => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                let result = tokio::time::timeout(timeout, client.resume_task(&id))
                    .await
                    .map_err(|_| timed_out())
                    .and_then(|r| r.map(|_| ActionOk::Resumed).map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Action {
                    kind: ActionKind::Resume,
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
        Effect::Send { id, path, text } => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let timeout = config.timeout;
            tokio::spawn(async move {
                let result =
                    match tokio::time::timeout(timeout, client.send_message(&id, &text)).await {
                        Ok(Ok(_)) => Ok(()),
                        Ok(Err(e)) => Err(SendFailure::Failed(e.to_string())),
                        Err(_) => Err(SendFailure::TimedOut),
                    };
                let _ = tx.send(Msg::Sent { id, path, result });
            });
        }
        Effect::RemoveFile { path } => {
            if let Err(e) = std::fs::remove_file(&path) {
                let _ = tx.send(Msg::RemoveFailed {
                    path,
                    error: e.to_string(),
                });
            }
        }
        // Handled by `apply_effects`, which awaits the editor in place.
        Effect::Compose { .. } => unreachable!("Compose is carried out by apply_effects"),
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
        Effect::LoadSearch { generation } => {
            let (client, tx) = (Arc::clone(client), tx.clone());
            let (project_id, timeout) = (config.project_id.clone(), config.timeout);
            // An abandoned load is cut off so reopening cannot stack requests.
            abort(search_load);
            *search_load = Some(tokio::spawn(async move {
                // No status, order or limit: every task in scope (see #292).
                let result = tokio::time::timeout(
                    timeout,
                    client.list_task_summaries(project_id.as_deref(), None, None, None),
                )
                .await
                .map_err(|_| timed_out())
                .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Msg::Search { generation, result });
            }));
        }
    }
    false
}

/// Both lists, and the project names when asked for.
async fn fetch(client: &Client, config: &LoopConfig, projects: bool) -> ListResult {
    let pid = config.project_id.as_deref();
    let active = client.list_task_summaries(pid, Some("open,pausing,paused,stuck"), None, None);
    // `--closed 0` shows no closed section rows: skip the request.
    let closed = async {
        if config.closed == 0 {
            Ok(Vec::new())
        } else {
            client
                .list_task_summaries(
                    pid,
                    Some("closed,cancelled"),
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
    // The header's agent-slot counts: re-read with every refresh. A failed
    // read is `None` and keeps the last values; the list fetches already
    // report an unreachable daemon.
    let server = client.server_status(config.timeout.min(Duration::from_secs(5)));
    let (active, closed, projects, server) = tokio::join!(active, closed, names, server);
    ListResult {
        at: Utc::now(),
        active: active.map_err(|e| e.to_string()),
        closed: closed.map_err(|e| e.to_string()),
        projects,
        server: server.ok(),
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
                    && let Ok(event) = serde_json::from_str(&text)
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
