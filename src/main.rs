use bew::app::{Action, App};
use bew::live::LiveUpdates;
use bew::model::{Bd, IssueGraph};
use bew::{tmux, ui};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest the loop waits for input before it checks on jobs and timers.
const TICK: Duration = Duration::from_millis(50);
/// How often bew reloads everything when the events journal is not followed.
const RELOAD_INTERVAL: Duration = Duration::from_secs(10);
/// How often bew checks which agent panes are alive.
const PANES_INTERVAL: Duration = Duration::from_secs(2);

const HELP: &str = "bew · beads workspace

Usage: bew [OPTIONS]

Options:
      --db <PATH>   Use a specific beads database
      --bd <PATH>   Path to the bd executable [env: BEW_BD]
  -h, --help        Print help
  -V, --version     Print version

Tree:      j/k move · h/l fold · Tab toggle · Enter open (creates on the + Create New entry)
           / go to · q/Esc quit
Task view: j/k dependency · Enter open · Backspace back · Esc tree · q quit
Both:      e edit description · et edit title · s set status · p set priority
           + add child · x close issue · a start agent · f focus agent

Steps:     status, priority, close, add issue and agent replace the screen.
           Enter applies, Esc cancels; both return to where you were.

Add issue: Enter advances · j/k selects type/priority · e opens $EDITOR for text
           Backspace returns to the previous selection · Esc asks before discarding

Agents:    a asks for a command, runs it in a new tmux pane to the right and
           binds the pane to the issue. bew does not change the issue itself.
           f shows the bound pane. ● marks issues with a live pane.

Refresh:   bew reloads after each action and when the terminal gains focus
           (in tmux: set -g focus-events on). With the events journal on,
           changes appear as they land; otherwise bew reloads every 10s.
           To turn the journal on: bd config set events-journal true
";

type Backend = CrosstermBackend<Box<dyn Write>>;

/// The terminal in raw mode on the alternate screen. ratatui keeps the last
/// frame and writes only the cells that differ from it.
struct Tui {
    terminal: Terminal<Backend>,
    active: bool,
}

impl Tui {
    fn new() -> io::Result<Self> {
        let output: Box<dyn Write> =
            match OpenOptions::new().read(true).write(true).open("/dev/tty") {
                Ok(tty) => Box::new(BufWriter::new(tty)),
                Err(_) => Box::new(BufWriter::new(io::stdout())),
            };
        let mut tui = Self {
            terminal: Terminal::new(CrosstermBackend::new(output))?,
            active: false,
        };
        tui.enter()?;
        Ok(tui)
    }

    fn enter(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        terminal::enable_raw_mode()?;
        self.active = true;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            Hide,
            EnableFocusChange,
            EnableBracketedPaste
        )?;
        // The screen may hold anything now, so the next frame draws in full.
        self.terminal.clear()
    }

    fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableFocusChange,
            LeaveAlternateScreen,
            Show
        )?;
        terminal::disable_raw_mode()?;
        self.active = false;
        Ok(())
    }

    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        self.terminal.draw(|frame| ui::draw(app, frame))?;
        execute!(self.terminal.backend_mut(), EndSynchronizedUpdate)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// The result of work done on a worker thread.
enum Done {
    /// A `bd` write, then a reload. The write's message, or why it failed.
    Write {
        seq: u64,
        created: bool,
        result: io::Result<(String, io::Result<IssueGraph>)>,
    },
    Reload {
        seq: u64,
        result: io::Result<IssueGraph>,
    },
    Statuses(io::Result<Vec<String>>),
    Panes(io::Result<HashMap<String, String>>),
    Agent {
        issue_id: String,
        result: io::Result<String>,
    },
    Focus(io::Result<()>),
}

/// Everything the UI thread owns. Only this thread touches `app` and the
/// terminal; `bd` and `tmux` run on worker threads and report back as `Done`.
struct Session {
    app: App,
    bd: Bd,
    tui: Tui,
    live: LiveUpdates,
    done_sender: mpsc::Sender<Done>,
    done: mpsc::Receiver<Done>,
    /// The number of the last graph load started, and of the last applied.
    /// A load that started before the applied one is stale.
    seq: u64,
    applied_seq: u64,
    /// While a write runs, key presses wait in `held` and run afterwards,
    /// against the refreshed data.
    writing: bool,
    held: VecDeque<Event>,
    reloads: usize,
    listing_panes: bool,
    next_reload: Instant,
    next_panes: Instant,
    dirty: bool,
}

impl Session {
    fn spawn(&self, job: impl FnOnce() -> Done + Send + 'static) {
        let sender = self.done_sender.clone();
        thread::spawn(move || {
            let _ = sender.send(job());
        });
    }

    /// Starts a full reload. Without `force`, it does not start while another
    /// one runs, so slow reloads do not pile up. No reload starts during a
    /// write: the write reloads when it finishes.
    fn reload(&mut self, force: bool) {
        if self.writing || (!force && self.reloads > 0) {
            return;
        }
        self.reloads += 1;
        self.seq += 1;
        let seq = self.seq;
        let bd = self.bd.clone();
        self.spawn(move || Done::Reload {
            seq,
            result: bd.load(),
        });
    }

    fn list_panes(&mut self) {
        if !self.app.tmux() || self.listing_panes {
            return;
        }
        self.listing_panes = true;
        self.spawn(|| Done::Panes(tmux::panes()));
    }

    /// Runs a `bd` write and a reload on a worker. `busy` shows until it is
    /// done; `write` returns the message to show on success.
    fn write(
        &mut self,
        busy: String,
        created: bool,
        write: impl FnOnce(&Bd) -> io::Result<String> + Send + 'static,
    ) {
        self.writing = true;
        self.app.set_status(busy);
        self.seq += 1;
        let seq = self.seq;
        let bd = self.bd.clone();
        self.spawn(move || Done::Write {
            seq,
            created,
            result: write(&bd).map(|message| (message, bd.load())),
        });
    }

    /// Applies a loaded graph unless a newer load was applied already.
    fn apply_graph(&mut self, seq: u64, graph: IssueGraph) {
        if seq <= self.applied_seq {
            return;
        }
        self.applied_seq = seq;
        if graph != self.app.graph {
            self.app.refresh_graph(graph);
            self.dirty = true;
        }
    }

    fn finish(&mut self, done: Done) {
        match done {
            Done::Write {
                seq,
                created,
                result,
            } => {
                self.writing = false;
                self.dirty = true;
                match result {
                    Err(error) => self.app.set_status(error.to_string()),
                    Ok((message, graph)) => {
                        if created {
                            self.app.finish_add_issue();
                        }
                        match graph {
                            Ok(graph) => {
                                self.apply_graph(seq, graph);
                                self.app.set_status(message);
                            }
                            Err(error) => self.app.set_status(format!(
                                "{message}, but reload failed: {error} · view may be stale"
                            )),
                        }
                    }
                }
            }
            Done::Reload { seq, result } => {
                self.reloads -= 1;
                match result {
                    Ok(graph) => self.apply_graph(seq, graph),
                    Err(error) => {
                        self.app.set_status(format!("reload failed: {error}"));
                        self.dirty = true;
                    }
                }
            }
            Done::Statuses(result) => {
                // Without an answer, the built-in statuses stay.
                if let Ok(statuses) = result {
                    self.app.set_statuses(statuses);
                }
            }
            Done::Panes(result) => {
                self.listing_panes = false;
                // No answer means no tmux server, so no live panes.
                self.dirty |= self.app.set_panes(result.unwrap_or_default());
            }
            Done::Agent { issue_id, result } => {
                match result {
                    Ok(pane_id) => self
                        .app
                        .set_status(format!("Started {pane_id} for {issue_id} · f focus")),
                    Err(error) => self.app.set_status(error.to_string()),
                }
                self.dirty = true;
                self.list_panes();
            }
            Done::Focus(result) => {
                if let Err(error) = result {
                    self.app.set_status(error.to_string());
                    self.dirty = true;
                }
            }
        }
    }

    /// Handles one terminal event; returns true to quit.
    fn handle_event(&mut self, event: Event) -> io::Result<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.dirty = true;
                let action = self.app.handle_key(key);
                self.act(action)
            }
            Event::Paste(text) => {
                self.dirty = true;
                self.app.handle_paste(&text);
                Ok(false)
            }
            Event::FocusGained => {
                self.reload(true);
                self.list_panes();
                Ok(false)
            }
            Event::Resize(..) => {
                self.dirty = true;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Carries out an action from the app; returns true to quit.
    fn act(&mut self, action: Action) -> io::Result<bool> {
        match action {
            Action::Quit => return Ok(true),
            Action::None => {}
            Action::CloseIssue(issue_id) => {
                self.write(format!("Closing {issue_id}…"), false, move |bd| {
                    bd.close_issue(&issue_id)?;
                    Ok(format!("Closed {issue_id}"))
                });
            }
            Action::SetStatus(issue_id, status) => {
                self.write(
                    format!("Setting {issue_id} to {status}…"),
                    false,
                    move |bd| {
                        bd.set_status(&issue_id, &status)?;
                        Ok(format!("Set {issue_id} to {status}"))
                    },
                );
            }
            Action::SetPriority(issue_id, priority) => {
                self.write(
                    format!("Setting {issue_id} to P{priority}…"),
                    false,
                    move |bd| {
                        bd.set_priority(&issue_id, priority)?;
                        Ok(format!("Set {issue_id} to P{priority}"))
                    },
                );
            }
            Action::CreateIssue(draft) => {
                self.write("Creating issue…".to_string(), true, move |bd| {
                    let issue_id = bd.create_issue(&draft)?;
                    Ok(match &draft.parent_id {
                        Some(parent_id) => format!("Created {issue_id} under {parent_id}"),
                        None => format!("Created {issue_id}"),
                    })
                });
            }
            Action::Edit(field) => {
                let Some(issue_id) = self.app.current_issue().map(|issue| issue.id.clone()) else {
                    return Ok(false);
                };
                self.tui.leave()?;
                let result = self.bd.edit(field, &issue_id);
                self.tui.enter()?;
                if let Err(error) = result {
                    self.app.set_status(error.to_string());
                }
                // A reload that started before the edit would miss it.
                self.reload(true);
            }
            Action::EditAddIssue(field) => {
                let initial = self.app.add_issue_field(field).unwrap_or("").to_string();
                self.tui.leave()?;
                let result = edit_add_issue_field(&initial);
                self.tui.enter()?;
                match result {
                    Ok(value) => self.app.set_add_issue_field(field, value),
                    Err(error) => self.app.set_status(error.to_string()),
                }
            }
            Action::StartAgent { issue_id, command } => {
                self.app
                    .set_status(format!("Starting a pane for {issue_id}…"));
                let cwd = env::current_dir()?;
                self.spawn(move || Done::Agent {
                    result: tmux::start(&command, &cwd, &issue_id),
                    issue_id,
                });
            }
            Action::FocusAgent(pane_id) => {
                self.spawn(move || Done::Focus(tmux::focus(&pane_id)));
            }
        }
        Ok(false)
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            // Wait for input, then take every event that is already queued,
            // so a burst of keys or a resize costs one draw.
            let timeout = self
                .app
                .pending_key_timeout()
                .map_or(TICK, |remaining| remaining.min(TICK));
            if event::poll(timeout)? {
                loop {
                    let event = event::read()?;
                    if is_ctrl_c(&event) {
                        return Ok(());
                    }
                    self.held.push_back(event);
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }

            while let Ok(done) = self.done.try_recv() {
                self.finish(done);
            }

            while !self.writing {
                let Some(event) = self.held.pop_front() else {
                    break;
                };
                if self.handle_event(event)? {
                    return Ok(());
                }
            }
            if !self.writing
                && self
                    .app
                    .pending_key_timeout()
                    .is_some_and(|remaining| remaining.is_zero())
            {
                self.dirty = true;
                let action = self.app.flush_pending_key();
                if self.act(action)? {
                    return Ok(());
                }
            }

            self.dirty |= self.live.poll(&mut self.app);

            let now = Instant::now();
            if now >= self.next_reload {
                self.next_reload = now + RELOAD_INTERVAL;
                // The journal brings changes on its own; a step holds a
                // choice the user is making, so it is not reloaded under them.
                if !self.live.is_following() && self.app.step().is_none() {
                    self.reload(false);
                }
            }
            if now >= self.next_panes {
                self.next_panes = now + PANES_INTERVAL;
                self.list_panes();
            }

            if self.dirty {
                self.tui.draw(&mut self.app)?;
                self.dirty = false;
            }
        }
    }
}

fn is_ctrl_c(event: &Event) -> bool {
    matches!(event, Event::Key(key)
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn parse_args() -> Result<Option<Bd>, String> {
    let mut args = env::args_os().skip(1);
    let mut bd = env::var_os("BEW_BD").unwrap_or_else(|| OsString::from("bd"));
    let mut db = None;
    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("bew {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--bd" => {
                bd = args
                    .next()
                    .ok_or_else(|| "--bd requires a path".to_string())?;
            }
            "--db" => {
                db = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--db requires a path".to_string())?,
                ));
            }
            unknown => return Err(format!("unknown option: {unknown}\n\n{HELP}")),
        }
    }
    Ok(Some(Bd::new(bd, db)))
}

fn edit_add_issue_field(initial: &str) -> io::Result<String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = env::temp_dir().join(format!("bew-add-{}-{nonce}.md", std::process::id()));
    fs::write(&path, initial)?;

    let result = (|| {
        let status = Command::new("sh")
            .args(["-c", "exec ${VISUAL:-${EDITOR:-vi}} \"$1\"", "bew-editor"])
            .arg(&path)
            .status()
            .map_err(|error| {
                io::Error::new(error.kind(), format!("could not open editor: {error}"))
            })?;
        if !status.success() {
            return Err(io::Error::other(format!("editor failed: {status}")));
        }
        fs::read_to_string(&path)
    })();
    let _ = fs::remove_file(&path);
    result
}

fn run(bd: Bd) -> io::Result<()> {
    // The first load happens before the screen changes, so a failure prints
    // as a plain error.
    let mut app = App::new(bd.load()?);
    app.set_tmux(tmux::available());
    let tui = Tui::new()?;
    let (done_sender, done) = mpsc::channel();
    let now = Instant::now();
    let mut session = Session {
        live: LiveUpdates::spawn(&bd),
        app,
        bd,
        tui,
        done_sender,
        done,
        seq: 0,
        applied_seq: 0,
        writing: false,
        held: VecDeque::new(),
        reloads: 0,
        listing_panes: false,
        next_reload: now + RELOAD_INTERVAL,
        next_panes: now,
        dirty: true,
    };
    let bd = session.bd.clone();
    session.spawn(move || Done::Statuses(bd.statuses()));
    session.run()
}

fn main() -> ExitCode {
    let bd = match parse_args() {
        Ok(Some(bd)) => bd,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bew: {message}");
            return ExitCode::from(2);
        }
    };

    match run(bd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bew: {error}");
            ExitCode::FAILURE
        }
    }
}
