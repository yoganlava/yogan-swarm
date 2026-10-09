//! The TUI: tasks grouped by status on the left, the selected task on the right. It only reads
//! the state directory; workers run detached, so closing it changes nothing.

use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use regex::Regex;
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::critic::{Findings, Severity};
use crate::lead::{self, Mode, Phase, Request};
use crate::stream::{self, Content};
use crate::task::{self, Status, Task};
use crate::{config, git, pr, sched, slot, worker};

/// List order, what needs you first; `Discarded` isn't shown.
const GROUPS: [(Status, &str); 7] = [
    (Status::Review, "Review"),
    (Status::Failed, "Failed"),
    (Status::Proposed, "Proposed"),
    (Status::Running, "Running"),
    (Status::Checking, "Checking"),
    (Status::Approved, "Queued"),
    (Status::PrOpen, "PR open"),
];

const TABS: [&str; 6] = ["Summary", "Activity", "Gate", "Findings", "Diff", "Run"];
const FINDINGS: usize = 3;
const DIFF: usize = 4;
const RUN: usize = 5;
/// Lines a scroll key moves the detail pane.
const SCROLL: u16 = 10;

const KEYS: [(&str, &str); 22] = [
    ("n", "new task"),
    ("j/k", "move"),
    ("tab", "pane"),
    ("1-6", "tabs"),
    ("d", "diff"),
    ("m", "open PR"),
    ("a", "approve"),
    ("A", "approve all"),
    ("e", "edit"),
    ("r", "reply"),
    ("t", "retry"),
    ("p", "plan it"),
    ("y", "copy"),
    ("x", "discard"),
    ("?", "help"),
    ("q", "quit"),
    ("c", "continue"),
    ("w", "rewind"),
    ("R", "run"),
    ("o", "open"),
    (",", "settings"),
    ("pgup/pgdn", "scroll"),
];

/// How a Settings field changes: cycling through choices, or stepping a number within bounds.
#[derive(Clone, Copy)]
enum Field {
    Pick(&'static [&'static str]),
    Step(f64, f64, f64),
}

const MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

/// What Settings edits, as (table, key, label, field); an empty label continues the row above.
const SETTINGS: [(&str, &str, &str, Field); 16] = [
    ("lead", "model", "lead", Field::Pick(MODELS)),
    ("lead", "effort", "", Field::Pick(worker::EFFORTS)),
    ("ask", "model", "questions", Field::Pick(MODELS)),
    ("ask", "effort", "", Field::Pick(worker::EFFORTS)),
    ("critic", "model", "critic", Field::Pick(MODELS)),
    ("critic", "effort", "", Field::Pick(worker::EFFORTS)),
    ("worker", "model", "workers", Field::Pick(MODELS)),
    ("worker", "effort", "", Field::Pick(worker::EFFORTS)),
    ("pr", "model", "PR drafts", Field::Pick(MODELS)),
    ("pr", "effort", "", Field::Pick(worker::EFFORTS)),
    (
        "watch",
        "stall_after",
        "stall after",
        Field::Pick(&["5m", "10m", "15m", "20m", "30m", "45m", "1h"]),
    ),
    ("watch", "nudges", "nudges", Field::Step(1.0, 0.0, 9.0)),
    (
        "watch",
        "loop_repeats",
        "loop repeats",
        Field::Step(1.0, 2.0, 20.0),
    ),
    (
        "watch",
        "autocompact",
        "autocompact",
        Field::Step(10_000.0, 50_000.0, 1_000_000.0),
    ),
    (
        "watch",
        "handoff_at",
        "handoff at",
        Field::Step(0.05, 0.5, 0.95),
    ),
    (
        "watch",
        "max_handoffs",
        "max handoffs",
        Field::Step(1.0, 0.0, 9.0),
    ),
];
/// The first watch row.
const WATCH: usize = 10;

/// The `,` screen: the `SETTINGS` values in effect for the file it saves to, as loaded and as
/// edited.
struct Settings {
    global: bool,
    /// The file it saves to, under `~` when it's in the home directory.
    path: String,
    loaded: Vec<toml::Value>,
    values: Vec<toml::Value>,
    row: usize,
}

impl Settings {
    fn load(repo: &Path, home: &Path, global: bool) -> Result<Settings> {
        let (path, table) = config::settings(repo, home, global)?;
        let path = match path.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        };
        let values: Vec<_> = SETTINGS
            .iter()
            .map(|(t, k, ..)| table.get(*t).and_then(|t| t.get(*k)).cloned())
            .map(|v| v.unwrap_or_else(|| "".into()))
            .collect();
        Ok(Settings {
            global,
            path,
            loaded: values.clone(),
            values,
            row: 0,
        })
    }

    /// Moves the selected value to the next choice or step, or back.
    fn cycle(&mut self, forward: bool) {
        let v = &mut self.values[self.row];
        let sign = if forward { 1.0 } else { -1.0 };
        *v = match (SETTINGS[self.row].3, &*v) {
            (Field::Pick(choices), v) => {
                let n = choices.len();
                let next = match choices.iter().position(|c| Some(*c) == v.as_str()) {
                    Some(i) if forward => (i + 1) % n,
                    Some(i) => (i + n - 1) % n,
                    None => 0,
                };
                choices[next].into()
            }
            (Field::Step(step, min, max), toml::Value::Integer(i)) => {
                ((*i as f64 + sign * step).clamp(min, max) as i64).into()
            }
            (Field::Step(step, min, max), toml::Value::Float(x)) => {
                (((x + sign * step).clamp(min, max) * 100.0).round() / 100.0).into()
            }
            (_, v) => v.clone(),
        };
    }

    /// Writes the values changed since loading; returns the file.
    fn save(&mut self, repo: &Path, home: &Path) -> Result<PathBuf> {
        let changed: Vec<_> = (SETTINGS.iter().zip(&self.values).zip(&self.loaded))
            .filter(|((_, v), was)| v != was)
            .map(|(((t, k, ..), v), _)| (*t, *k, v.clone()))
            .collect();
        ensure!(!changed.is_empty(), "nothing changed");
        let path = config::save(repo, home, self.global, &changed)?;
        self.loaded = self.values.clone();
        Ok(path)
    }
}

/// Every color and glyph, so a light-terminal or ASCII variant is one swap. Only key chips paint
/// a background: elsewhere the terminal's own theme shows through.
pub struct Theme {
    accent: Color,
    /// Behind a footer key.
    key: Color,
    /// Shell commands in Activity.
    shell: Color,
    green: Color,
    amber: Color,
    red: Color,
    pass: &'static str,
    fail: &'static str,
    checking: &'static str,
    queued: &'static str,
    bar: &'static str,
    ellipsis: &'static str,
    /// Before a child task, under its parent.
    tree: &'static str,
    spinner: &'static [&'static str],
}

impl Theme {
    /// Truecolor when `COLORTERM` says so, except in VS Code, whose 16 ANSI colours follow its
    /// theme; ASCII glyphs under `YOGAN_ASCII=1`.
    pub fn detect() -> Theme {
        let env = |k: &str, vals: &[&str]| std::env::var(k).is_ok_and(|v| vals.contains(&&*v));
        Theme::new(
            env("COLORTERM", &["truecolor", "24bit"]) && !vscode(),
            env("YOGAN_ASCII", &["1"]),
        )
    }

    fn new(truecolor: bool, ascii: bool) -> Theme {
        let rgb = |r, g, b, ansi| if truecolor { Color::Rgb(r, g, b) } else { ansi };
        let glyphs = |fancy, plain| if ascii { plain } else { fancy };
        Theme {
            accent: rgb(122, 162, 247, Color::Blue),
            key: rgb(42, 47, 69, Color::Black),
            shell: rgb(125, 207, 255, Color::Cyan),
            green: rgb(158, 206, 106, Color::Green),
            amber: rgb(224, 175, 104, Color::Yellow),
            red: rgb(247, 118, 142, Color::Red),
            pass: glyphs("✓", "+"),
            fail: glyphs("✗", "x"),
            checking: glyphs("◆", "*"),
            queued: glyphs("○", "o"),
            bar: glyphs("▌", ">"),
            ellipsis: glyphs("…", "~"),
            tree: glyphs("└ ", "- "),
            spinner: if ascii {
                &["|", "/", "-", "\\"]
            } else {
                &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            },
        }
    }

    /// Green, amber and red mean pass, unsure and fail; the accent is running work.
    fn glyph(&self, status: Status, gate_failed: bool, tick: usize) -> Span<'static> {
        match status {
            Status::Running => Span::styled(self.spinner[tick % self.spinner.len()], self.accent),
            Status::Checking => Span::styled(self.checking, self.accent),
            Status::Review if gate_failed => Span::styled(self.fail, self.red),
            Status::Review | Status::PrOpen => Span::styled(self.pass, self.green),
            Status::Failed => Span::styled(self.fail, self.red),
            Status::Proposed => Span::styled(self.queued, self.amber),
            Status::Approved | Status::Discarded => Span::raw(self.queued).dim(),
        }
    }
}

fn vscode() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "vscode")
}

struct App {
    repo: PathBuf,
    state: PathBuf,
    name: String,
    /// Requests whose lead is planning or failed, listed before the tasks.
    requests: Vec<Request>,
    /// Shown tasks in list order, each with when its file last changed.
    tasks: Vec<(Task, Option<SystemTime>)>,
    /// A row of `requests` then `tasks`.
    selected: usize,
    /// Narrow layout shows the detail pane instead of the list.
    detail: bool,
    help: bool,
    compose: Option<Compose>,
    /// An error to show in the footer until the next key.
    notice: Option<String>,
    /// Progress or an outcome to show in the footer until the next key.
    info: Option<String>,
    /// Push and open the previewed PR once the "pushing" footer has been drawn.
    opening: bool,
    tab: usize,
    /// The selected task's tool calls as (tool, target), newest last.
    activity: Vec<(String, String)>,
    gate_log: String,
    /// The selected task's changed files as (path, added, deleted).
    diff: Vec<(String, u64, u64)>,
    /// Which task and file version `gate_log` and `diff` were read for.
    loaded: Option<(String, Option<SystemTime>)>,
    /// Asking whether to discard the selected task.
    confirm: bool,
    /// A slot, its base and optionally one file, whose diff to page once the TUI is suspended.
    pager: Option<(PathBuf, String, Option<String>)>,
    /// Showing the selected task's PR draft.
    preview: bool,
    /// A task being drafted, with the draft it had and the drafting worker's pid, to preview
    /// once a new one lands.
    awaiting: Option<(String, Option<pr::Draft>, u32)>,
    /// The instruction for `g` in the preview, while it's being typed.
    instruction: Option<TextArea<'static>>,
    /// Edit the PR draft, or else the selected proposal, in `$EDITOR` once the TUI is
    /// suspended.
    edit: bool,
    /// The reply to the selected proposal's lead, while it's being typed.
    reply: Option<TextArea<'static>>,
    /// Sessions a task gets: its first plus `[watch] max_handoffs`.
    sessions: u32,
    /// What the reply modal is for when it isn't the lead: `r` upholds the selected finding,
    /// `x` waives it, `t` retries a failed task and `w` rewinds one.
    reply_for: Option<char>,
    /// `w`'s picker: the worker's commits as `sha subject`, newest first, and the picked one.
    commits: Vec<String>,
    commit: usize,
    /// A slot and session to continue in `claude` once the TUI is suspended.
    interactive: Option<(PathBuf, String)>,
    /// How far the detail pane is scrolled: down from the top, or up from the tail in Activity
    /// and the Gate output. Drawing clamps it.
    scroll: Cell<u16>,
    /// The selected task's findings, and the cursor over their actionable ones.
    findings: Findings,
    finding: usize,
    /// Tasks in Review with a disputed finding.
    disputed: Vec<String>,
    /// The selected question's id, answer and the repo files the answer cites.
    answer: Option<(String, String, Vec<String>)>,
    run: RunTab,
    /// Tasks whose run script is running.
    serving: Vec<String>,
    settings: Option<Settings>,
    /// Running in VS Code's terminal.
    vscode: bool,
    /// A `code --wait` on a file from `edit_file`, applied once it exits.
    editing: Option<(Child, Edit)>,
    /// A file and line to open in `$EDITOR` once the TUI is suspended.
    open_at: Option<(PathBuf, Option<u32>)>,
    /// The cursor over the Diff tab's files.
    diff_file: usize,
    /// Where the last frame drew things, for the mouse.
    hits: RefCell<Hits>,
}

/// A file `e` edits, as (task id, whether it's the PR draft, path).
type Edit = (String, bool, PathBuf);

/// The list and detail panes, for the wheel, and each clickable rect in drawing order.
#[derive(Default)]
struct Hits {
    list: Rect,
    detail: Rect,
    targets: Vec<(Rect, Target)>,
}

/// What a click does: press a key, or select a list row, which has no key.
#[derive(Clone, Copy)]
enum Target {
    Key(KeyEvent),
    Row(usize),
}

/// The key a footer label stands for; pairs like `j/k` stand for none.
fn key_of(label: &str) -> Option<KeyEvent> {
    let code = match label {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "ctrl-s" => return Some(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        _ => match label.chars().collect::<Vec<_>>()[..] {
            [c] => KeyCode::Char(c),
            _ => return None,
        },
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

/// The Run tab: the slot's ports and `[scripts]`, read when the task changes, and the run
/// script's pid and output, read every frame.
#[derive(Default)]
struct RunTab {
    /// The task and slot the ports and scripts were read for.
    loaded: Option<(String, Option<u32>)>,
    /// The first port and how many.
    ports: Option<(u32, u32)>,
    /// The configured `[scripts]` by name.
    scripts: Vec<&'static str>,
    pid: Option<u32>,
    log: String,
}

/// The `n` screen: a request for the lead, and an optional ticket.
struct Compose {
    request: TextArea<'static>,
    ticket: TextArea<'static>,
    mode: Mode,
    /// The focused field: request, ticket, mode, then the Submit button.
    focus: usize,
}

const MODES: [(Mode, &str); 3] = [
    (Mode::Auto, "Auto"),
    (Mode::Plan, "Plan"),
    (Mode::Ask, "Ask"),
];

/// An empty input with a placeholder and no cursor-line underline.
fn field(placeholder: &str) -> TextArea<'static> {
    let mut t = TextArea::default();
    t.set_cursor_line_style(Style::new());
    t.set_placeholder_text(placeholder);
    t
}

impl Compose {
    fn new(mode: Mode, text: &str) -> Compose {
        let mut request = field("Ask a question, or describe a change for the lead to plan");
        request.insert_str(text);
        Compose {
            request,
            ticket: field("e.g. CC-687"),
            mode,
            focus: 0,
        }
    }
}

pub fn run(repo: &Path) -> Result<()> {
    let state = task::state_dir(repo)?;
    reap(&state)?;
    let name = state.file_name().unwrap_or_default().to_string_lossy();
    let mut app = App {
        repo: repo.to_path_buf(),
        state: state.clone(),
        name: name.into_owned(),
        requests: Vec::new(),
        tasks: Vec::new(),
        selected: 0,
        detail: false,
        help: false,
        compose: None,
        notice: None,
        info: None,
        opening: false,
        tab: 0,
        activity: Vec::new(),
        gate_log: String::new(),
        diff: Vec::new(),
        loaded: None,
        confirm: false,
        pager: None,
        preview: false,
        awaiting: None,
        instruction: None,
        edit: false,
        reply: None,
        answer: None,
        reply_for: None,
        commits: Vec::new(),
        commit: 0,
        interactive: None,
        scroll: Cell::new(0),
        findings: Findings::default(),
        finding: 0,
        disputed: Vec::new(),
        sessions: config::load(repo).map_or(0, |c| c.watch.max_handoffs + 1),
        run: RunTab::default(),
        serving: Vec::new(),
        settings: None,
        vscode: vscode(),
        editing: None,
        open_at: None,
        diff_file: 0,
        hits: RefCell::default(),
    };
    let theme = Theme::detect();
    let mouse = config::load(repo).is_ok_and(|c| c.tui.mouse);
    let mut terminal = init(mouse);
    let mut in_review: Option<Vec<String>> = None;
    let start = Instant::now();
    let res = (|| -> Result<()> {
        loop {
            // checked before the reload, so an exited worker's last save is already loaded
            let exited = app.awaiting.as_ref().is_some_and(|a| !worker::alive(a.2));
            app.reload(&state)?;
            app.load_tab();
            app.check_awaiting(exited);
            let review = app.tasks.iter().filter(|(t, _)| t.status == Status::Review);
            let review: Vec<_> = review.map(|(t, _)| t.id.clone()).collect();
            // a bell when a task newly reaches Review; VS Code marks its terminal tab
            if in_review.is_some_and(|was| review.iter().any(|id| !was.contains(id))) {
                print!("\x07");
                std::io::stdout().flush()?;
            }
            in_review = Some(review);
            let tick = (start.elapsed().as_millis() / 125) as usize; // spinner at 8 Hz
            // ratatui only writes cells that changed, so an idle screen draws nothing
            terminal.draw(|f| draw(f, &app, &theme, tick, SystemTime::now()))?;
            // ponytail: pushes from the TUI, which freezes for the push; a worker can do it if slow
            if std::mem::take(&mut app.opening) {
                match app.open_pr() {
                    Ok(url) => app.info = Some(format!("PR opened: {url}")),
                    Err(e) => (app.info, app.notice) = (None, Some(format!("{e:#}"))),
                }
                continue;
            }
            let running = app.tasks.iter().any(|(t, _)| running(t));
            let wait = Duration::from_millis(if running { 125 } else { 250 });
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && !app.key(key) => {
                        return Ok(());
                    }
                    Event::Mouse(m) if !app.mouse(m) => return Ok(()),
                    _ => {}
                }
            }
            if let Some((dir, base, path)) = app.pager.take() {
                restore(mouse);
                let diff = format!("{base}...HEAD");
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["diff", &diff, "--"])
                    .args(path)
                    .status()?;
                terminal = init(mouse);
            }
            if let Some((dir, session)) = app.interactive.take() {
                restore(mouse);
                let ran = Command::new("claude")
                    .args(["--resume", &session])
                    .current_dir(&dir)
                    .status();
                terminal = init(mouse);
                if let Err(e) = ran {
                    app.notice = Some(format!("claude: {e}"));
                }
            }
            if let Some((file, line)) = app.open_at.take() {
                restore(mouse);
                let opened = editor(&file, line);
                terminal = init(mouse);
                if let Err(e) = opened {
                    app.notice = Some(format!("{e:#}"));
                }
            }
            if std::mem::take(&mut app.edit) {
                match app.edit_file() {
                    Err(e) => app.notice = Some(format!("{e:#}")),
                    // VS Code edits in a tab while the TUI stays up
                    Ok(edit) if app.vscode => {
                        let code = Command::new("code")
                            .arg("--wait")
                            .arg(&edit.2)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                        match code {
                            Ok(child) => app.editing = Some((child, edit)),
                            Err(e) => app.notice = Some(format!("code: {e}")),
                        }
                    }
                    Ok(edit) => {
                        restore(mouse);
                        let edited = editor(&edit.2, None);
                        terminal = init(mouse);
                        if let Err(e) = edited.and_then(|()| app.apply_edit(edit)) {
                            app.notice = Some(format!("{e:#}"));
                        }
                    }
                }
            }
            let closed = app.editing.as_mut().map(|(c, _)| c.try_wait());
            if closed.is_some_and(|w| !matches!(w, Ok(None)))
                && let Some((_, edit)) = app.editing.take()
                && let Err(e) = app.apply_edit(edit)
            {
                app.notice = Some(format!("{e:#}"));
            }
        }
    })();
    restore(mouse);
    res
}

/// `ratatui::init`, capturing the mouse when `mouse`.
fn init(mouse: bool) -> DefaultTerminal {
    let terminal = ratatui::init();
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    terminal
}

fn restore(mouse: bool) {
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
}

/// Runs VS Code's `code` CLI, which hands the file to the window and returns.
fn code(args: &[&std::ffi::OsStr]) -> Result<()> {
    let status = Command::new("code")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running code")?;
    ensure!(status.success(), "code exited with {status}");
    Ok(())
}

/// A finding's `path:line` (or `path:line-end`) as the path and line.
fn path_line(location: &str) -> (String, Option<u32>) {
    let (path, rest) = location.split_once(':').unwrap_or((location, ""));
    let digits = rest.split(|c: char| !c.is_ascii_digit()).next();
    (path.to_string(), digits.and_then(|d| d.parse().ok()))
}

/// Approves `ids` in `tasks`. Errs when one's parent would stay unapproved; returns a warning
/// when an approved task names a crate that another task in flight also names.
fn approve(tasks: &mut [Task], ids: &[String]) -> Result<Option<String>> {
    for t in tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
        t.status = Status::Approved;
    }
    let in_flight = |s| {
        matches!(
            s,
            Status::Approved | Status::Running | Status::Checking | Status::Review
        )
    };
    let mut warning = None;
    for t in tasks.iter().filter(|t| ids.contains(&t.id)) {
        // a child starts once its parent's PR is open, so it would wait forever
        if let Some(p) = &t.parent {
            let parent = tasks.iter().find(|o| &o.id == p);
            ensure!(
                parent.is_some_and(|p| !matches!(p.status, Status::Proposed | Status::Discarded)),
                "approve the parent of “{}” first",
                t.title
            );
        }
        let shared = tasks.iter().find(|o| {
            o.id != t.id && in_flight(o.status) && o.crates.iter().any(|c| t.crates.contains(c))
        });
        if let Some(o) = shared {
            warning.get_or_insert(format!(
                "“{}” shares a crate with “{}”, so expect a rebase",
                t.title, o.title
            ));
        }
    }
    Ok(warning)
}

/// The fields `e` shows for a proposal; the rest of the task file is yogan's.
const EDITABLE: [&str; 8] = [
    "title",
    "body",
    "ticket",
    "acceptance",
    "parent",
    "crates",
    "model",
    "effort",
];

/// `t` with its editable fields replaced by those in `text`; others in `text` are ignored.
fn edited(t: &Task, text: &str) -> Result<Task> {
    let mut table: toml::Table = toml::from_str(&toml::to_string(t)?)?;
    let edited: toml::Table = toml::from_str(text)?;
    table.retain(|k, _| !EDITABLE.contains(&k));
    table.extend(
        edited
            .into_iter()
            .filter(|(k, _)| EDITABLE.contains(&k.as_str())),
    );
    Ok(toml::Value::Table(table).try_into()?)
}

/// Opens `file`, at `line` if given, in `$EDITOR` (default `vi`) and waits for it to exit.
fn editor(file: &Path, line: Option<u32>) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let at = line.map_or(String::new(), |l| format!(" +{l}"));
    let edited = Command::new("sh")
        .args(["-c", &format!("{editor}{at} \"$1\""), "sh"])
        .arg(file)
        .status()?;
    ensure!(edited.success(), "{editor} exited with {edited}");
    Ok(())
}

/// Ids from `t`'s furthest ancestor with the same status down to `t`.
fn lineage(t: &Task, tasks: &[(Task, Option<SystemTime>)]) -> Vec<String> {
    let mut path = vec![t.id.clone()];
    let mut cur = t;
    while let Some(p) = cur.parent.as_ref().and_then(|p| {
        tasks
            .iter()
            .map(|(o, _)| o)
            .find(|o| &o.id == p && o.status == t.status)
    }) {
        if path.contains(&p.id) {
            break; // an edit made a cycle
        }
        path.insert(0, p.id.clone());
        cur = p;
    }
    path
}

fn running(t: &Task) -> bool {
    matches!(t.status, Status::Running | Status::Checking)
}

/// A worker that died without recording an outcome (a crash, a reboot) fails its task.
fn reap(state: &Path) -> Result<()> {
    for mut t in task::load_all(state)? {
        if running(&t) && !t.pid.is_some_and(worker::alive) {
            t.status = Status::Failed;
            t.summary = Some("the worker exited without finishing".into());
            t.save(state)?;
        }
    }
    // a lead records its pid when it starts; none a minute on means it never did
    let never_started = |r: &Request| {
        let file = state.join(format!("requests/{}.toml", r.id));
        let since = fs::metadata(file).and_then(|m| m.modified()).ok();
        let age = since.and_then(|s| s.elapsed().ok()).unwrap_or_default();
        r.pid.is_none() && age > Duration::from_secs(60)
    };
    for mut r in lead::load_all(state)? {
        if r.status == Phase::Planning
            && (r.pid.is_some_and(|p| !worker::alive(p)) || never_started(&r))
        {
            r.status = Phase::Failed;
            r.summary = Some("the lead exited without finishing".into());
            r.save(state)?;
        }
    }
    Ok(())
}

impl App {
    /// Re-reads the request and task files, keeping the selection on the same row.
    fn reload(&mut self, state: &Path) -> Result<()> {
        let id = match self.request() {
            Some(r) => Some(r.id.clone()),
            None => self.task().map(|(t, _)| t.id.clone()),
        };
        let mut requests = lead::load_all(state)?;
        let answered = |r: &Request| lead::answer_path(state, &r.id).exists();
        requests.retain(|r| match r.status {
            Phase::Planning | Phase::Failed => true,
            Phase::Done => answered(r),
            Phase::Dismissed => false,
        });
        // planning and failed first, then the questions
        requests.sort_by_key(|r| (r.status == Phase::Done, r.id.clone()));
        self.requests = requests;
        let rank = |s: Status| GROUPS.iter().position(|(g, _)| *g == s);
        let tasks: Vec<_> = task::load_all(state)?
            .into_iter()
            .filter(|t| rank(t.status).is_some())
            .map(|t| {
                let file = state.join(format!("tasks/{}.toml", t.id));
                let since = fs::metadata(file).and_then(|m| m.modified()).ok();
                (t, since)
            })
            .collect();
        let paths: Vec<_> = tasks.iter().map(|(t, _)| lineage(t, &tasks)).collect();
        let mut tasks: Vec<_> = tasks.into_iter().zip(paths).collect();
        // children right after their parent, so the list reads as a tree
        tasks.sort_by(|(a, pa), (b, pb)| (rank(a.0.status), pa).cmp(&(rank(b.0.status), pb)));
        self.tasks = tasks.into_iter().map(|(t, _)| t).collect();
        let mut ids =
            (self.requests.iter().map(|r| &r.id)).chain(self.tasks.iter().map(|(t, _)| &t.id));
        let same = id.and_then(|id| ids.position(|i| *i == id));
        let rows = self.requests.len() + self.tasks.len();
        self.selected = same.unwrap_or(self.selected.min(rows.saturating_sub(1)));
        let review = self
            .tasks
            .iter()
            .filter(|(t, _)| t.status == Status::Review);
        let disputes =
            |t: &Task| Findings::load(state, &t.id).is_ok_and(|f| !f.disputed.is_empty());
        self.disputed = review
            .filter(|(t, _)| disputes(t))
            .map(|(t, _)| t.id.clone())
            .collect();
        self.serving = (self.tasks.iter())
            .filter(|(t, _)| t.slot.is_some() && slot::running(state, &t.id).is_some())
            .map(|(t, _)| t.id.clone())
            .collect();
        Ok(())
    }

    /// Whether `j/k`, `r` and `x` act on the Findings tab's findings.
    fn on_findings(&self) -> bool {
        self.detail && self.tab == FINDINGS && self.task().is_some()
    }

    /// Whether `j/k` and `enter` act on the Diff tab's files.
    fn on_diff(&self) -> bool {
        self.detail && self.tab == DIFF && self.task().is_some()
    }

    /// The selected row's request, if it's one.
    fn request(&self) -> Option<&Request> {
        self.requests.get(self.selected)
    }

    /// The selected task's base; see `task::base`. Read from every task, since a discarded
    /// parent isn't listed but its child still targets its branch.
    fn base(&self) -> Result<String> {
        let (t, _) = self.task().context("no task selected")?;
        Ok(task::base(t, task::load_all(&self.state)?.iter()))
    }

    /// The selected row's task, if it's one.
    fn task(&self) -> Option<&(Task, Option<SystemTime>)> {
        let i = self.selected.checked_sub(self.requests.len())?;
        self.tasks.get(i)
    }

    /// Returns false to quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl =
            |c| key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c);
        self.notice = None;
        self.info = None;
        if ctrl('c') {
            return false;
        }
        if let Some(c) = &mut self.compose {
            let ctrl_enter = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => self.compose = None,
                KeyCode::Tab => c.focus = (c.focus + 1) % 4,
                // enter alone and alt+enter (VS Code's shift+enter) are newlines in the request
                KeyCode::Enter if c.focus == 3 || ctrl_enter => {
                    if let Err(e) = self.submit() {
                        self.notice = Some(format!("{e:#}"));
                    }
                }
                _ if ctrl('s') => {
                    if let Err(e) = self.submit() {
                        self.notice = Some(format!("{e:#}"));
                    }
                }
                KeyCode::Left | KeyCode::Right if c.focus == 2 => {
                    let i = MODES.iter().position(|(m, _)| *m == c.mode).unwrap_or(0);
                    let step = if key.code == KeyCode::Right { 1 } else { 2 };
                    c.mode = MODES[(i + step) % 3].0;
                }
                _ if c.focus >= 2 => {}
                KeyCode::Enter if c.focus == 1 => {}
                _ if c.focus == 1 => _ = c.ticket.input(key),
                _ => _ = c.request.input(key),
            }
            return true;
        }
        if let Some(s) = &mut self.settings {
            match key.code {
                KeyCode::Esc => self.settings = None,
                KeyCode::Up | KeyCode::Char('k') => s.row = s.row.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => s.row = (s.row + 1).min(SETTINGS.len() - 1),
                KeyCode::Left | KeyCode::Char('h') => s.cycle(false),
                KeyCode::Right | KeyCode::Char('l') => s.cycle(true),
                KeyCode::Char('g') => {
                    let global = !s.global;
                    self.open_settings(global);
                }
                _ if ctrl('s') => match self.save_settings() {
                    Ok(path) => self.info = Some(format!("saved {}", path.display())),
                    Err(e) => self.notice = Some(format!("{e:#}")),
                },
                _ => {}
            }
            return true;
        }
        if let Some(input) = &mut self.reply {
            match key.code {
                KeyCode::Esc => (self.reply, self.reply_for) = (None, None),
                KeyCode::Up if self.reply_for == Some('w') => {
                    self.commit = self.commit.saturating_sub(1);
                }
                KeyCode::Down if self.reply_for == Some('w') => {
                    self.commit = (self.commit + 1).min(self.commits.len().saturating_sub(1));
                }
                _ if ctrl('s') => {
                    let text = input.lines().join("\n");
                    self.reply = None;
                    if let Err(e) = self.send_reply(&text) {
                        self.notice = Some(format!("{e:#}"));
                    }
                }
                _ => _ = input.input(key),
            }
            return true;
        }
        if let Some(input) = &mut self.instruction {
            match key.code {
                KeyCode::Esc => self.instruction = None,
                KeyCode::Enter => {
                    let text = input.lines().join(" ");
                    self.instruction = None;
                    if let Err(e) = self.draft(Some(text.trim())) {
                        self.notice = Some(format!("{e:#}"));
                    }
                }
                _ => _ = input.input(key),
            }
            return true;
        }
        if self.preview {
            match key.code {
                KeyCode::Esc => self.preview = false,
                KeyCode::Enter => {
                    self.opening = true;
                    self.info = Some("pushing the branch and opening the PR…".into());
                }
                KeyCode::Char('g') => {
                    self.instruction = Some(field("e.g. shorter, drop the critic line"));
                }
                KeyCode::Char('e') => self.edit = true,
                KeyCode::Char('q') => return false,
                _ => {}
            }
            return true;
        }
        if self.confirm {
            self.confirm = false;
            if key.code == KeyCode::Char('y')
                && let Err(e) = self.discard()
            {
                self.notice = Some(format!("{e:#}"));
            }
            return true;
        }
        if key.code == KeyCode::Char('q') {
            return false;
        }
        if self.help {
            self.help = false;
            return true;
        }
        let proposed = self
            .task()
            .is_some_and(|(t, _)| t.status == Status::Proposed);
        let failed = self.request().is_some_and(|r| r.status == Phase::Failed);
        let status = self.task().map(|(t, _)| t.status);
        let answered = self.request().is_some_and(|r| r.status == Phase::Done);
        if self.on_findings() {
            let n = self.findings.actionable().count();
            let disputed = self.findings.is_disputed(self.finding);
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.finding = (self.finding + 1).min(n.saturating_sub(1));
                    return true;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.finding = self.finding.saturating_sub(1);
                    return true;
                }
                KeyCode::Char('x') if n > 0 => {
                    self.reply = Some(field("Why waive it? The reason goes in the PR body"));
                    self.reply_for = Some('x');
                    return true;
                }
                KeyCode::Char('r') if disputed => {
                    self.reply = Some(field("Why the critic is right, for the worker"));
                    self.reply_for = Some('r');
                    return true;
                }
                _ => {}
            }
        }
        if self.on_diff() {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    let n = self.diff.len();
                    self.diff_file = (self.diff_file + 1).min(n.saturating_sub(1));
                    return true;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.diff_file = self.diff_file.saturating_sub(1);
                    return true;
                }
                KeyCode::Enter => {
                    if let Err(e) = self.open_diff() {
                        self.notice = Some(format!("{e:#}"));
                    }
                    return true;
                }
                _ => {}
            }
        }
        let before = (self.selected, self.tab);
        match key.code {
            // before `d`, which would take ctrl-d too
            _ if ctrl('d') || ctrl('u') => self.scroll_by(ctrl('d')),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(',') => self.open_settings(false),
            KeyCode::Char('n') => self.compose = Some(Compose::new(Mode::Auto, "")),
            KeyCode::Char('p') if answered => {
                let (r, answer) = (self.request(), self.answer.as_ref());
                let (question, answer) = (r.map_or("", |r| &r.text), answer.map_or("", |a| &a.1));
                let text = format!("{question}\n\nThe answer to build on:\n\n{answer}");
                self.compose = Some(Compose::new(Mode::Plan, &text));
            }
            KeyCode::Char('y') if answered => {
                let answer = self.answer.as_ref().map_or("", |a| &a.1);
                match copy(answer) {
                    Ok(()) => self.info = Some("copied the answer".into()),
                    Err(e) => self.notice = Some(format!("{e:#}")),
                }
            }
            KeyCode::Char(c @ '1'..='6') => self.tab = c as usize - '1' as usize,
            KeyCode::Char('R') => {
                if let Err(e) = self.toggle_run() {
                    self.notice = Some(format!("{e:#}"));
                }
            }
            KeyCode::Char('o') if self.task().is_some() => {
                if let Err(e) = self.open() {
                    self.notice = Some(format!("{e:#}"));
                }
            }
            KeyCode::Char('d') => match self.slot_dir() {
                Some(dir) => self.pager = Some((dir, self.base().unwrap_or_default(), None)),
                None => self.notice = Some("this task has no worktree".into()),
            },
            KeyCode::Char('x') => self.confirm = self.task().is_some() || failed || answered,
            KeyCode::Char('t') if failed => {
                if let Err(e) = self.retry() {
                    self.notice = Some(format!("{e:#}"));
                }
            }
            KeyCode::Char('t') if status == Some(Status::Failed) => {
                let model = self.task().and_then(|(t, _)| t.model.clone());
                let keep = model.map_or(String::new(), |m| format!("; empty keeps {m}"));
                self.reply = Some(field(&format!("Model to retry on{keep}")));
                self.reply_for = Some('t');
            }
            KeyCode::Char('c') if matches!(status, Some(Status::Review | Status::Failed)) => {
                let session = self.task().and_then(|(t, _)| t.sessions.last().cloned());
                match (self.slot_dir(), session) {
                    (Some(dir), Some(s)) => self.interactive = Some((dir, s)),
                    _ => self.notice = Some("this task has no session to continue".into()),
                }
            }
            KeyCode::Char('w') if status == Some(Status::Review) => match self.worker_commits() {
                Ok(commits) if !commits.is_empty() => {
                    (self.commits, self.commit) = (commits, 0);
                    self.reply = Some(field("Why rewind? The worker resumes with this"));
                    self.reply_for = Some('w');
                }
                Ok(_) => self.notice = Some("the worker made no commits".into()),
                Err(e) => self.notice = Some(format!("{e:#}")),
            },
            KeyCode::Char('m') => {
                if let Err(e) = self.draft(None) {
                    self.notice = Some(format!("{e:#}"));
                }
            }
            KeyCode::Char(c @ ('a' | 'A')) => {
                if let Err(e) = self.approve(c == 'A') {
                    self.notice = Some(format!("{e:#}"));
                }
            }
            KeyCode::Char('e') => self.edit = proposed,
            KeyCode::Char('r') if proposed => {
                self.reply = Some(field("What should the lead change?"));
            }
            KeyCode::Char('r') if answered => self.reply = Some(field("Ask a follow-up")),
            KeyCode::Char('r') if status == Some(Status::Review) => {
                self.reply = Some(field("What should the worker change?"));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let rows = self.requests.len() + self.tasks.len();
                self.selected = (self.selected + 1).min(rows.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Tab => self.detail = !self.detail,
            KeyCode::PageDown | KeyCode::PageUp => self.scroll_by(key.code == KeyCode::PageDown),
            _ => {}
        }
        if (self.selected, self.tab) != before {
            self.scroll.set(0);
            self.diff_file = 0;
        }
        true
    }
}

impl App {
    /// Scrolls the detail pane a step toward the end of its text (`down`) or back; Activity and
    /// the Gate output count from their tail, so down there means newer.
    fn scroll_by(&mut self, down: bool) {
        let tail = self.task().is_some() && matches!(self.tab, 1 | 2 | RUN);
        let s = self.scroll.get();
        self.scroll.set(match down != tail {
            true => s.saturating_add(SCROLL),
            false => s.saturating_sub(SCROLL),
        });
    }

    fn slot_dir(&self) -> Option<PathBuf> {
        let (t, _) = self.task()?;
        Some(self.state.join("slots").join(t.slot?.to_string()))
    }

    /// Reads what the current tab shows for the selected task. The gate log and diff are
    /// re-read only when the task's file changes; the activity tail every time.
    fn load_tab(&mut self) {
        let question = self.request().filter(|r| r.status == Phase::Done);
        let id = question.map(|r| r.id.clone());
        if id.is_none() {
            self.answer = None;
        } else if self.answer.as_ref().map(|a| &a.0) != id.as_ref() {
            let id = id.unwrap_or_default();
            let text = fs::read_to_string(lead::answer_path(&self.state, &id)).unwrap_or_default();
            let cited = cites(&self.repo, &text);
            self.answer = Some((id, text, cited));
        }
        let Some((id, since)) = self.task().map(|(t, since)| (t.id.clone(), *since)) else {
            return;
        };
        if self.tab == 1 {
            let log = self.state.join(format!("logs/{id}.jsonl"));
            self.activity = activity(&log, &self.slot_dir().unwrap_or_default());
        }
        if self.tab == RUN {
            let slot = self.task().and_then(|(t, _)| t.slot);
            let key = Some((id.clone(), slot));
            if self.run.loaded != key {
                let cfg = config::load(&self.repo).ok();
                let ports = cfg.as_ref().and_then(|c| c.ports.as_ref());
                // a task without a slot has no ports yet
                self.run.ports = slot
                    .zip(ports)
                    .map(|(n, p)| (p.base + n * p.per_slot, p.per_slot));
                let s = cfg.as_ref().and_then(|c| c.scripts.as_ref());
                self.run.scripts = s.map_or(Vec::new(), |s| {
                    let named = [
                        ("setup", &s.setup),
                        ("run", &s.run),
                        ("teardown", &s.teardown),
                    ];
                    named
                        .into_iter()
                        .filter(|(_, c)| c.is_some())
                        .map(|(n, _)| n)
                        .collect()
                });
                self.run.loaded = key;
            }
            self.run.pid = slot::running(&self.state, &id);
            let log = self.state.join(format!("logs/{id}.run.log"));
            self.run.log = String::from_utf8_lossy(&tail(&log, 64 << 10)).into_owned();
        }
        let key = Some((id.clone(), since));
        if self.tab >= 2 && self.loaded != key {
            let log = self.state.join(format!("logs/{id}.gate.log"));
            self.gate_log = fs::read_to_string(log).unwrap_or_default();
            let base = self.base().unwrap_or_default();
            self.diff = self
                .slot_dir()
                .map(|d| diffstat(&d, &base))
                .unwrap_or_default();
            self.diff_file = self.diff_file.min(self.diff.len().saturating_sub(1));
            self.findings = Findings::load(&self.state, &id).unwrap_or_default();
            let n = self.findings.actionable().count();
            self.finding = self.finding.min(n.saturating_sub(1));
            self.loaded = key;
        }
    }

    /// Stops the task's worker, runs `[scripts] teardown` in its slot and frees the slot.
    fn discard(&mut self) -> Result<()> {
        if let Some(r) = self.request() {
            let mut r = r.clone();
            r.status = Phase::Dismissed;
            return r.save(&self.state);
        }
        let Some((t, _)) = self.task() else {
            return Ok(());
        };
        let mut t = t.clone();
        if running(&t)
            && let Some(pid) = t.pid
        {
            let _ = worker::stop(pid); // it may already have exited
        }
        t.status = Status::Discarded;
        self.free_slot(t, "discarded")
    }

    /// `t` on a failed request: reruns its lead, resuming the session when it has one.
    fn retry(&mut self) -> Result<()> {
        let id = self.request().context("no request selected")?.id.clone();
        let prompt = lead::retry(&self.state, &id)?;
        lead::spawn(&self.repo, &self.state, &id, prompt.as_deref())?;
        self.info = Some("the lead is planning again".into());
        Ok(())
    }

    /// Opens Settings on the project file, or the global one.
    fn open_settings(&mut self, global: bool) {
        match config::home().and_then(|home| Settings::load(&self.repo, &home, global)) {
            Ok(s) => self.settings = Some(s),
            Err(e) => self.notice = Some(format!("{e:#}")),
        }
    }

    fn save_settings(&mut self) -> Result<PathBuf> {
        let s = self.settings.as_mut().context("Settings isn't open")?;
        let path = s.save(&self.repo, &config::home()?)?;
        self.sessions = config::load(&self.repo)?.watch.max_handoffs + 1;
        Ok(path)
    }

    /// `R`: stops the selected task's run script, or starts `[scripts] run` in its slot.
    fn toggle_run(&mut self) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        if slot::running(&self.state, &t.id).is_some() {
            slot::stop_run(&self.state, &t.id)?;
            self.info = Some("stopped the run script".into());
            return Ok(());
        }
        ensure!(
            t.status == Status::Review,
            "run a task's script once it's in Review"
        );
        let n = t.slot.context("this task has no worktree")?;
        let dir = self.slot_dir().context("this task has no worktree")?;
        let cfg = config::load(&self.repo)?;
        let run = cfg.scripts.as_ref().and_then(|s| s.run.as_deref());
        let cmd = run.context("no [scripts] run in the project file")?;
        let env = slot::env(&self.repo, &dir, n, t, cfg.ports.as_ref());
        slot::start_run(&self.state, &t.id, cmd, &dir, &env)?;
        (self.tab, self.info) = (RUN, Some("started the run script".into()));
        Ok(())
    }

    /// Stops the task's run script and runs `[scripts] teardown` in its slot, then saves it with
    /// the slot freed.
    fn free_slot(&mut self, mut t: Task, done: &str) -> Result<()> {
        let _ = slot::stop_run(&self.state, &t.id); // it may already have exited
        let mut teardown = Ok(());
        if let (Some(n), Some(dir)) = (t.slot, self.slot_dir()) {
            let cfg = config::load(&self.repo)?;
            if let Some(cmd) = cfg.scripts.as_ref().and_then(|s| s.teardown.as_deref()) {
                let env = slot::env(&self.repo, &dir, n, &t, cfg.ports.as_ref());
                let log = self.state.join(format!("logs/{}.teardown.log", t.id));
                teardown = slot::run_script(cmd, &dir, &env, &log);
            }
        }
        t.slot = None;
        t.save(&self.state)?;
        sched::run(&self.repo)?;
        if let Err(e) = teardown {
            bail!("{done}, but teardown failed: {e:#}");
        }
        Ok(())
    }

    /// `m`: previews the PR draft if it matches the branch, else has a worker rebase, re-gate
    /// and draft it (following `instruction` when regenerating); the preview opens when it lands.
    fn draft(&mut self, instruction: Option<&str>) -> Result<()> {
        ensure!(self.awaiting.is_none(), "a PR draft is on its way");
        let (t, _) = self.task().context("no task selected")?;
        ensure!(
            t.status == Status::Review && gate_passed(t),
            "a PR needs a task in Review with a passing gate"
        );
        let dir = self.slot_dir().context("this task has no worktree")?;
        let head = git(&dir, &["rev-parse", "HEAD"])?;
        if instruction.is_none() && t.pr_draft.as_ref().is_some_and(|d| d.head == head) {
            self.preview = true;
            return Ok(());
        }
        let mut args = vec!["--pr"];
        if let Some(text) = instruction {
            args.extend(["--instruction", text]);
        }
        let _ = fs::remove_file(self.state.join(format!("logs/{}.pr.log", t.id))); // may not exist
        let pid = worker::spawn(&self.repo, &t.id, &args)?;
        self.awaiting = Some((t.id.clone(), t.pr_draft.clone(), pid));
        self.preview = false;
        Ok(())
    }

    /// Opens the preview once the awaited task has a new draft; gives up if it can't get one
    /// or its worker `exited` without one.
    fn check_awaiting(&mut self, exited: bool) {
        let Some((id, old, _)) = &self.awaiting else {
            return;
        };
        // never move the selection under an open modal, whose keys would then act on it
        let modal = self.confirm || self.help || self.reply.is_some() || self.compose.is_some();
        if modal || self.instruction.is_some() {
            return;
        }
        let Some(i) = self.tasks.iter().position(|(t, _)| &t.id == id) else {
            self.awaiting = None; // discarded meanwhile
            return;
        };
        let t = &self.tasks[i].0;
        if t.status == Status::Review && t.pr_draft.is_some() && t.pr_draft != *old {
            let i = self.requests.len() + i;
            (self.selected, self.preview, self.awaiting) = (i, true, None);
        } else if t.status == Status::Failed || (t.status == Status::Review && !gate_passed(t)) {
            self.notice = Some("no PR draft: see the task's Summary and Gate tabs".into());
            self.awaiting = None;
        } else if exited {
            let log = self.state.join(format!("logs/{id}.pr.log"));
            let why = fs::read_to_string(log).unwrap_or("the worker exited".into());
            self.notice = Some(format!("no PR draft: {}", why.trim()));
            self.awaiting = None;
        }
    }

    /// `e`: writes what it edits for the selected task to a file: in the preview, the PR draft
    /// as `title`, blank line, body; else the proposal's editable fields as TOML.
    fn edit_file(&self) -> Result<Edit> {
        ensure!(self.editing.is_none(), "already editing in VS Code");
        let (t, _) = self.task().context("no task selected")?;
        fs::create_dir_all(self.state.join("logs"))?;
        if self.preview {
            let draft = t.pr_draft.as_ref().context("no PR draft")?;
            let file = self.state.join(format!("logs/{}.pr.md", t.id));
            fs::write(&file, format!("{}\n\n{}\n", draft.title, draft.body))?;
            return Ok((t.id.clone(), true, file));
        }
        ensure!(
            t.status == Status::Proposed,
            "only a proposal can be edited"
        );
        let table: toml::Table = toml::from_str(&toml::to_string(t)?)?;
        let shown: toml::Table = table
            .into_iter()
            .filter(|(k, _)| EDITABLE.contains(&k.as_str()))
            .collect();
        let file = self.state.join(format!("logs/{}.task.toml", t.id));
        let header = format!("# Editable: {}\n", EDITABLE.join(", "));
        fs::write(&file, format!("{header}{}", toml::to_string(&shown)?))?;
        Ok((t.id.clone(), false, file))
    }

    /// Applies a file from `edit_file`, once edited, to its task; a proposal is checked like a
    /// new one.
    fn apply_edit(&mut self, (id, draft, file): Edit) -> Result<()> {
        let all = task::load_all(&self.state)?;
        let t = all
            .iter()
            .find(|t| t.id == id)
            .context("the task is gone")?;
        let text = fs::read_to_string(&file)?;
        if !draft {
            ensure!(t.status == Status::Proposed, "it's no longer a proposal");
            let t = edited(t, &text)?;
            task::check_proposal(&t, &all)?;
            return t.save(&self.state);
        }
        let mut t = t.clone();
        let n = t.slot.context("this task has no worktree")?;
        let dir = self.state.join("slots").join(n.to_string());
        let base = task::base(&t, all.iter());
        let draft = t.pr_draft.as_mut().context("no PR draft")?;
        let (title, body) = text.trim().split_once('\n').unwrap_or((text.trim(), ""));
        (draft.title, draft.body) = (pr::clean(title.trim()), pr::clean(body.trim()));
        let cfg = config::load(&self.repo)?;
        let migration = pr::migration(&dir, &base)?;
        draft.problem = pr::check_title(&draft.title, &cfg.pr.types, migration).err();
        t.save(&self.state)
    }

    /// `o`: opens the selected finding's `path:line` in the editor, or else the task's worktree.
    fn open(&mut self) -> Result<()> {
        let dir = self.slot_dir().context("this task has no worktree")?;
        let finding = self
            .on_findings()
            .then(|| self.findings.actionable().nth(self.finding));
        let at = finding.flatten().map(|f| path_line(&f.location));
        match (at, self.vscode) {
            (Some((path, line)), true) => {
                let line = line.map_or(String::new(), |l| format!(":{l}"));
                let at = format!("{}{line}", dir.join(path).display());
                code(&["-g".as_ref(), at.as_ref()])?;
            }
            (Some((path, line)), false) => self.open_at = Some((dir.join(path), line)),
            (None, true) => code(&["-n".as_ref(), dir.as_os_str()])?,
            (None, false) => self.info = Some(dir.display().to_string()),
        }
        Ok(())
    }

    /// `enter` on the Diff tab: the selected file's diff in VS Code's diff editor against its
    /// base version, else paged.
    fn open_diff(&mut self) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        let id = t.id.clone();
        let (path, ..) = self.diff.get(self.diff_file).context("no file selected")?;
        let path = path.clone();
        let dir = self.slot_dir().context("this task has no worktree")?;
        let base = self.base()?;
        if !self.vscode {
            self.pager = Some((dir, base, Some(path)));
            return Ok(());
        }
        let fork = git(&dir, &["merge-base", &base, "HEAD"])?;
        // a file the change adds has no base version, so it diffs against an empty one
        let old = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["show", &format!("{fork}:{path}")])
            .output()?;
        let name = Path::new(&path).file_name().unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("yogan-{id}-base-{}", name.display()));
        fs::write(
            &tmp,
            if old.status.success() {
                old.stdout
            } else {
                Vec::new()
            },
        )?;
        code(&[
            "--diff".as_ref(),
            tmp.as_os_str(),
            dir.join(&path).as_os_str(),
        ])
    }

    /// A click presses the key of the target under it or selects its row; the wheel scrolls the
    /// detail pane or moves through the list. Returns false to quit.
    fn mouse(&mut self, m: MouseEvent) -> bool {
        let modal = self.compose.is_some() || self.settings.is_some() || self.reply.is_some();
        let modal =
            modal || self.preview || self.confirm || self.help || self.instruction.is_some();
        let at = Position::new(m.column, m.row);
        let (in_list, in_detail, hit) = {
            let hits = self.hits.borrow();
            let hit = hits.targets.iter().rev().find(|(r, _)| r.contains(at));
            (
                hits.list.contains(at),
                hits.detail.contains(at),
                hit.map(|(_, t)| *t),
            )
        };
        let before = self.selected;
        match (m.kind, hit) {
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Key(key))) => {
                return self.key(key);
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Target::Row(i))) if !modal => {
                self.selected = i;
            }
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, _) if !modal => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if in_detail {
                    self.scroll_by(down);
                } else if in_list {
                    let rows = self.requests.len() + self.tasks.len();
                    self.selected = match down {
                        true => (self.selected + 1).min(rows.saturating_sub(1)),
                        false => self.selected.saturating_sub(1),
                    };
                }
            }
            _ => {}
        }
        if self.selected != before {
            self.scroll.set(0);
            self.diff_file = 0;
        }
        true
    }

    /// `enter` in the preview: pushes, opens the PR, then tears down and frees the slot.
    /// Returns the PR's URL.
    fn open_pr(&mut self) -> Result<String> {
        let (t, _) = self.task().context("no task selected")?;
        let mut t = t.clone();
        let dir = self.slot_dir().context("this task has no worktree")?;
        let draft = t.pr_draft.as_ref().context("no PR draft")?;
        if let Some(problem) = &draft.problem {
            bail!("fix the title first (e): {problem}");
        }
        ensure!(
            git(&dir, &["rev-parse", "HEAD"])? == draft.head,
            "the branch changed since this draft; press m to redraft"
        );
        let cfg = config::load(&self.repo)?;
        let url = pr::open(&t, &dir, &self.base()?, cfg.pr.draft)?;
        t.pr_url = Some(url.clone());
        t.status = Status::PrOpen;
        self.preview = false;
        self.free_slot(t, &format!("PR opened ({url})"))?;
        Ok(url)
    }

    /// `r`/`x` on the selected finding: upholds it with the human's note and sends it back to
    /// the worker, or waives it with their reason.
    fn act_on_finding(&mut self, act: char, text: &str) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        // the worker saves findings as it goes, so act only while it's stopped
        ensure!(
            t.status == Status::Review,
            "act on findings once the task is in Review"
        );
        let id = t.id.clone();
        let mut findings = Findings::load(&self.state, &id)?;
        // the tab reloads on task file changes, which this isn't, so force it
        self.loaded = None;
        if act == 'x' {
            findings.waive(self.finding, text)?;
            findings.save(&self.state, &id)?;
            self.info = Some("waived; the reason goes in the PR body".into());
            return Ok(());
        }
        let f = findings.uphold(self.finding)?;
        findings.save(&self.state, &id)?;
        let prompt = format!(
            "The human upholds this finding you disputed, so fix it and commit.\n\n\
             [{:?}] {} - {}\nEvidence: {}\nYour reason: {}\nTheir note: {text}",
            f.severity,
            f.location,
            f.claim,
            f.evidence,
            f.reply.as_deref().unwrap_or_default()
        );
        worker::spawn(&self.repo, &id, &["--reply", &prompt])?;
        self.info = Some("sent back to the worker".into());
        Ok(())
    }

    /// `a`/`A`: approves the selected proposal, or all of them, and starts whatever is ready.
    fn approve(&mut self, all: bool) -> Result<()> {
        let selected = self.task().map(|(t, _)| t.id.clone());
        let mut tasks = task::load_all(&self.state)?;
        let ids: Vec<String> = tasks
            .iter()
            .filter(|t| t.status == Status::Proposed && (all || Some(&t.id) == selected.as_ref()))
            .map(|t| t.id.clone())
            .collect();
        ensure!(!ids.is_empty(), "no proposal to approve");
        let warning = approve(&mut tasks, &ids)?;
        for t in tasks.iter().filter(|t| ids.contains(&t.id)) {
            t.save(&self.state)?;
        }
        sched::run(&self.repo)?;
        self.info = Some(match warning {
            Some(w) => format!("approved; {w}"),
            None if ids.len() == 1 => "approved".into(),
            None => format!("approved {} tasks", ids.len()),
        });
        Ok(())
    }

    /// Sends `text` to the selected proposal's lead, which refiles its revised plan.
    /// `t` on a failed task: resumes its session in its slot on `model` (empty keeps its own),
    /// or queues it afresh when it never got a session.
    fn retry_task(&mut self, model: &str) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        ensure!(
            t.status == Status::Failed,
            "only a failed task can be retried"
        );
        let mut t = t.clone();
        if !model.is_empty() {
            t.model = Some(model.into());
        }
        t.nudges = 0;
        if t.sessions.is_empty() {
            // nothing to resume, so a fresh checkout loses nothing
            (t.status, t.slot) = (Status::Approved, None);
            t.save(&self.state)?;
            sched::run(&self.repo)?;
        } else {
            t.save(&self.state)?;
            let why = t.summary.as_deref().unwrap_or("unknown");
            let prompt = format!(
                "yogan stopped this task: {why}\n\nThe human retried it. Carry on from where you \
                 left off; if the same problem comes back, stop and explain it."
            );
            worker::spawn(&self.repo, &t.id, &["--reply", &prompt])?;
        }
        self.info = Some("retrying".into());
        Ok(())
    }

    /// The worker's commits in the selected task's slot, newest first, as `sha subject`.
    fn worker_commits(&self) -> Result<Vec<String>> {
        let dir = self.slot_dir().context("this task has no worktree")?;
        let range = format!("{}..HEAD", self.base()?);
        let log = git(&dir, &["log", "--format=%h %s", &range])?;
        Ok(log.lines().map(String::from).collect())
    }

    /// `w`: resets the slot to the picked commit and resumes the worker with `reason`.
    fn rewind(&mut self, reason: &str) -> Result<()> {
        ensure!(!reason.is_empty(), "write a reason first");
        let (t, _) = self.task().context("no task selected")?;
        ensure!(
            t.status == Status::Review,
            "rewind a task once it's in Review"
        );
        let id = t.id.clone();
        let dir = self.slot_dir().context("this task has no worktree")?;
        let target = self.commits.get(self.commit).context("no commit picked")?;
        let sha = target.split(' ').next().unwrap_or_default();
        git(&dir, &["reset", "--quiet", "--hard", sha])?;
        let dropped = match self.commit {
            0 => "keeping every commit".to_string(),
            n => format!(
                "dropping these later commits:\n{}",
                self.commits[..n].join("\n")
            ),
        };
        let prompt = format!(
            "The human rewound this branch to {target}, {dropped}\n\nTheir reason: {reason}\n\n\
             Carry on from there."
        );
        worker::spawn(&self.repo, &id, &["--reply", &prompt])?;
        self.info = Some(format!("rewound to {sha}; the worker is resuming"));
        Ok(())
    }

    fn send_reply(&mut self, text: &str) -> Result<()> {
        match self.reply_for.take() {
            Some('t') => return self.retry_task(text.trim()),
            Some('w') => return self.rewind(text.trim()),
            Some(act) => return self.act_on_finding(act, text.trim()),
            None => {}
        }
        ensure!(!text.trim().is_empty(), "write a reply first");
        if let Some(r) = self.request() {
            let id = r.id.clone();
            lead::follow_up(&self.repo, &self.state, &id, text.trim())?;
            lead::spawn(&self.repo, &self.state, &id, Some(text.trim()))?;
            self.info = Some("the lead is answering the follow-up".into());
            return Ok(());
        }
        let (t, _) = self.task().context("no task selected")?;
        if t.status == Status::Review {
            let prompt = format!(
                "Feedback from the human reviewing this change:\n\n{}",
                text.trim()
            );
            worker::spawn(&self.repo, &t.id, &["--reply", &prompt])?;
            self.info = Some("sent to the worker".into());
            return Ok(());
        }
        ensure!(
            t.status == Status::Proposed && !t.plan.is_empty(),
            "this task has no lead to reply to"
        );
        let prompt = lead::reply(&self.state, &t.plan, text.trim())?;
        lead::spawn(&self.repo, &self.state, &t.plan, Some(&prompt))?;
        self.info = Some("the lead is revising the plan".into());
        Ok(())
    }

    /// Hands the composed request to a lead, whose proposals arrive as `Proposed` tasks.
    fn submit(&mut self) -> Result<()> {
        let c = self.compose.as_ref().context("not composing")?;
        let request = c.request.lines().join("\n");
        ensure!(!request.trim().is_empty(), "write a request first");
        let ticket = c.ticket.lines().join("").trim().to_string();
        let ticket = (!ticket.is_empty()).then_some(ticket);
        let mode = c.mode;
        lead::submit(&self.repo, &self.state, request.trim(), ticket, mode)?;
        self.compose = None;
        self.info = Some(match mode {
            Mode::Ask => "the lead is answering".into(),
            _ => "the lead is on it; proposals or an answer will show up here".into(),
        });
        Ok(())
    }
}

/// The Activity tab's tool name for a worker's nudge.
const NUDGE: &str = "↻";
/// The Activity tab's tool name for a worker's handoff to a fresh session.
const HANDOFF: &str = "⇢";

/// Tool calls, nudges and handoffs from the tail of a Claude stream log, as (tool, target), with `slot/`
/// paths made relative.
fn activity(log: &Path, slot: &Path) -> Vec<(String, String)> {
    // ponytail: only the last 256 KiB, so redrawing a long session stays cheap
    let bytes = tail(log, 256 << 10);
    let prefix = format!("{}/", slot.display());
    let text = String::from_utf8_lossy(&bytes);
    let events = text
        .lines()
        .filter_map(|l| serde_json::from_str::<stream::Event>(l).ok());
    let line = |c| match c {
        Content::ToolUse { name, input, .. } => {
            let key = match name.as_str() {
                "Bash" => "command",
                "Grep" | "Glob" => "pattern",
                _ => "file_path",
            };
            let target = input[key]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");
            Some((name, target.replace(&prefix, "")))
        }
        _ => None,
    };
    events
        .flat_map(|e| match e {
            stream::Event::Assistant { message } => {
                message.content.into_iter().filter_map(line).collect()
            }
            stream::Event::Nudge { reason } => vec![(NUDGE.into(), reason)],
            stream::Event::Handoff { reason } => vec![(HANDOFF.into(), reason)],
            _ => Vec::new(),
        })
        .collect()
}

/// Up to the last `max` bytes of `file`; empty if it can't be read.
fn tail(file: &Path, max: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Ok(mut f) = File::open(file) {
        let len = f.metadata().map_or(0, |m| m.len());
        let read = f.seek(SeekFrom::Start(len.saturating_sub(max)));
        if read.and_then(|_| f.read_to_end(&mut bytes)).is_err() {
            bytes.clear();
        }
    }
    bytes
}

/// Draws `p` scrolled down by the detail pane's offset, clamped to its last page.
fn scrolled(f: &mut Frame, area: Rect, p: Paragraph, scroll: &Cell<u16>) {
    let max = p
        .line_count(area.width)
        .saturating_sub(area.height as usize);
    scroll.set(scroll.get().min(max as u16));
    f.render_widget(p.scroll((scroll.get(), 0)), area);
}

/// For a view that follows its tail: how many of `len` lines to hide below, clamped so a full
/// page of `height` stays in view.
fn from_tail(len: usize, height: u16, scroll: &Cell<u16>) -> usize {
    let max = len.saturating_sub(height as usize);
    scroll.set(scroll.get().min(max as u16));
    scroll.get() as usize
}

/// `git diff --numstat` against the base, as (path, added, deleted); binaries count 0.
fn diffstat(slot: &Path, base: &str) -> Vec<(String, u64, u64)> {
    let out = git(slot, &["diff", "--numstat", &format!("{base}...HEAD")]).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (add, del, path) = (parts.next()?, parts.next()?, parts.next()?);
            Some((
                path.into(),
                add.parse().unwrap_or(0),
                del.parse().unwrap_or(0),
            ))
        })
        .collect()
}

/// Below this many rows, the header and footer share one line, the list drops its group headings
/// and the detail pane shows only the active tab's name.
const COMPACT: u16 = 24;

fn draw(f: &mut Frame, app: &App, theme: &Theme, tick: usize, now: SystemTime) {
    *app.hits.borrow_mut() = Hits::default();
    let compact = f.area().height < COMPACT;
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(if compact { 0 } else { 1 }),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    f.render_widget(header_line(app, theme), header);
    let selected = app.task().map(|(t, _)| t);
    let failed = app.request().is_some_and(|r| r.status == Phase::Failed);
    let answered = app.request().is_some_and(|r| r.status == Phase::Done);
    let draft = selected.and_then(|t| t.pr_draft.as_ref());
    if let Some(c) = &app.compose {
        compose(f, body, c, theme);
    } else if let Some(s) = &app.settings {
        settings(f, body, s, theme);
    } else if app.preview
        && let Some(d) = draft
    {
        preview(f, body, d, app.instruction.as_ref(), theme);
    } else if body.width >= 100 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).areas(body);
        list(f, left, app, theme, !app.detail, tick, now);
        detail(f, right, app, theme, app.detail);
    } else if app.detail {
        detail(f, body, app, theme, true);
    } else {
        list(f, body, app, theme, true, tick, now);
    }
    let keys: Vec<(&str, &str)> = if let Some(s) = &app.settings {
        let other = if s.global {
            "project file"
        } else {
            "global file"
        };
        let keys = [("j/k", "field"), ("←→", "change"), ("g", other)];
        [&keys[..], &[("ctrl-s", "save"), ("esc", "close")]].concat()
    } else if app.compose.is_some() {
        vec![("tab", "field"), ("ctrl-s", "submit"), ("esc", "cancel")]
    } else if app.reply.is_some() {
        vec![("ctrl-s", "send"), ("esc", "cancel")]
    } else if app.on_findings() {
        let mut keys = vec![("j/k", "finding")];
        keys.extend((app.findings.is_disputed(app.finding)).then_some(("r", "uphold")));
        keys.extend((app.findings.actionable().count() > 0).then_some(("x", "waive")));
        keys.extend([
            ("o", "open"),
            ("tab", "pane"),
            ("1-6", "tabs"),
            ("?", "more"),
        ]);
        keys
    } else if app.on_diff() {
        let keys = [("j/k", "file"), ("enter", "file diff"), ("d", "full diff")];
        [
            &keys[..],
            &[("tab", "pane"), ("1-6", "tabs"), ("?", "more")],
        ]
        .concat()
    } else if app.instruction.is_some() {
        vec![("enter", "redraft"), ("esc", "cancel")]
    } else if app.preview {
        let push = ("enter", "push and open PR");
        vec![push, ("g", "regenerate"), ("e", "edit"), ("esc", "back")]
    } else {
        // up to six keys that do something for the selection, the one that moves it on first
        let valid = |k: &str| match k {
            "d" | "o" => selected.is_some_and(|t| t.slot.is_some()),
            "x" => selected.is_some() || failed || answered,
            "t" => failed || selected.is_some_and(|t| t.status == Status::Failed),
            "c" => selected.is_some_and(|t| matches!(t.status, Status::Review | Status::Failed)),
            "w" => selected.is_some_and(|t| t.status == Status::Review),
            "p" | "y" => answered,
            "1-6" => selected.is_some(),
            "R" => {
                selected.is_some_and(|t| t.status == Status::Review || app.serving.contains(&t.id))
            }
            "m" => selected.is_some_and(|t| t.status == Status::Review && gate_passed(t)),
            "r" => {
                answered
                    || selected
                        .is_some_and(|t| matches!(t.status, Status::Proposed | Status::Review))
            }
            "a" | "e" => selected.is_some_and(|t| t.status == Status::Proposed),
            "A" => app.tasks.iter().any(|(t, _)| t.status == Status::Proposed),
            _ => true,
        };
        let first: &[&str] = match selected.map(|t| t.status) {
            Some(Status::Review) => &["m", "r", "d", "x", "c", "w", "R", "o"],
            Some(Status::Failed) => &["t", "c", "x", "d", "o"],
            Some(Status::Proposed) => &["a", "A", "e", "r", "x"],
            _ if answered => &["p", "r", "y", "x"],
            _ => &["t", "d", "o", "R", "x"],
        };
        let mut keys: Vec<_> = [first, &["n", "1-6", "j/k", "tab", "q"]]
            .concat()
            .into_iter()
            .filter(|k| valid(k))
            .filter_map(|k| KEYS.into_iter().find(|(key, _)| *key == k))
            .take(6)
            .collect();
        keys.push(("?", "more"));
        keys
    };
    let chip = |(key, label): &(&str, &str)| {
        [
            Span::styled(format!(" {key} "), theme.accent)
                .bold()
                .bg(theme.key),
            Span::raw(format!(" {label} ")).dim(),
        ]
    };
    let prefix = if compact {
        header_line(app, theme).spans
    } else {
        Vec::new()
    };
    let width = |keys: &[(&str, &str)]| {
        let spans = prefix.iter().cloned().chain(keys.iter().flat_map(chip));
        Line::from(spans.collect::<Vec<_>>()).width()
    };
    // a `? more` footer drops keys from before it until the line fits, keeping the first
    let mut keys = keys;
    let more = keys.last() == Some(&("?", "more"));
    while more && keys.len() > 2 && width(&keys) > footer.width as usize {
        keys.remove(keys.len() - 2);
    }
    let mut line = match (&app.notice, &app.info) {
        (Some(notice), _) => vec![Span::styled(format!(" {notice}"), theme.red)],
        (None, Some(info)) => vec![Span::styled(format!(" {info}"), theme.accent)],
        (None, None) if app.editing.is_some() => {
            vec![Span::styled(
                " editing in VS Code; close its tab to apply",
                theme.accent,
            )]
        }
        (None, None) => {
            let mut x = footer.x + Line::from(prefix.clone()).width() as u16;
            for k in &keys {
                let w = Line::from(chip(k).to_vec()).width() as u16;
                let rect = Rect::new(x, footer.y, w, 1).intersection(footer);
                let target = key_of(k.0).map(|key| (rect, Target::Key(key)));
                app.hits.borrow_mut().targets.extend(target);
                x = x.saturating_add(w);
            }
            keys.iter().flat_map(chip).collect()
        }
    };
    line.splice(0..0, prefix);
    f.render_widget(Line::from(line), footer);
    if app.help || app.confirm || app.reply.is_some() {
        // only an open modal's own targets respond
        app.hits.borrow_mut().targets.clear();
    }
    if app.help {
        help(f, theme);
    }
    let confirm = match (app.request(), selected) {
        (Some(r), _) => Some((
            "Dismiss",
            r.text.lines().next().unwrap_or_default(),
            "dismiss it",
        )),
        (None, Some(t)) => Some(("Discard", t.title.as_str(), "discard and free its slot")),
        _ => None,
    };
    if app.confirm
        && let Some((verb, title, does)) = confirm
    {
        // a modal over the dimmed screen
        let all = f.area();
        f.buffer_mut().set_style(all, Style::new().dim());
        let area = centered(f.area(), 52, 6);
        let block = pane(verb, true, theme);
        let [text, buttons] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(block.inner(area));
        let lines = vec![
            Line::raw(format!("{verb} “{title}”?")),
            Line::raw(does).dim(),
        ];
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text);
        let mut x = buttons.x;
        let mut row = Vec::new();
        for (key, label, code) in [
            ("y", verb, KeyCode::Char('y')),
            ("esc", "Keep it", KeyCode::Esc),
        ] {
            let button = [
                Span::styled(format!(" {key} "), theme.accent)
                    .bold()
                    .bg(theme.key),
                Span::raw(format!(" {label} ")).bg(theme.key),
                Span::raw("   "),
            ];
            let w = Line::from(button[..2].to_vec()).width() as u16;
            let rect = Rect::new(x, buttons.y, w, 1).intersection(buttons);
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            app.hits.borrow_mut().targets.push((rect, Target::Key(key)));
            x = x.saturating_add(w + 3);
            row.extend(button);
        }
        f.render_widget(Line::from(row), buttons);
    }
    if let Some(input) = &app.reply {
        let all = f.area();
        f.buffer_mut().set_style(all, Style::new().dim());
        let picks = match app.reply_for {
            Some('w') => app.commits.len() as u16 + 1,
            _ => 0,
        };
        let area = centered(all, 64, 8 + picks);
        let title = match app.reply_for {
            Some('x') => "Waive the finding",
            Some('t') => "Retry the task",
            Some('w') => "Rewind to a commit (↑↓)",
            Some(_) => "Uphold the finding",
            None if app.task().is_some_and(|(t, _)| t.status == Status::Review) => {
                "Reply to the worker"
            }
            None => "Reply to the lead",
        };
        let block = pane(title, true, theme);
        let [list, field] = Layout::vertical([Constraint::Length(picks), Constraint::Fill(1)])
            .areas(block.inner(area));
        let commits = app
            .commits
            .iter()
            .enumerate()
            .map(|(i, c)| match i == app.commit {
                true => Line::from(vec![
                    Span::styled(theme.bar, theme.accent),
                    Span::raw(c.clone()),
                ]),
                false => Line::raw(format!(" {c}")).dim(),
            });
        f.render_widget(Clear, area);
        if picks > 0 {
            f.render_widget(Paragraph::new(commits.collect::<Vec<_>>()), list);
        }
        f.render_widget(input, field);
        f.render_widget(block, area);
    }
}

fn gate_passed(t: &Task) -> bool {
    t.gate.as_ref().is_some_and(|g| g.iter().all(|c| c.passed))
}

/// The PR draft in place of both panes, with the `g` instruction input below when open.
fn preview(f: &mut Frame, area: Rect, d: &pr::Draft, input: Option<&TextArea>, theme: &Theme) {
    let [text, ask] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(if input.is_some() { 3 } else { 0 }),
    ])
    .areas(area);
    let mut lines = vec![Line::raw(d.title.clone()).bold()];
    if let Some(problem) = &d.problem {
        lines.push(Line::styled(format!("{} {problem}", theme.fail), theme.red));
    }
    lines.push(Line::raw(""));
    lines.extend(d.body.lines().map(|l| Line::raw(l.to_string())));
    let block = pane("PR preview", input.is_none(), theme);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        text,
    );
    if let Some(input) = input {
        let block = pane("Regenerate with", true, theme);
        f.render_widget(input, block.inner(ask));
        f.render_widget(block, ask);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect {
        x: (area.width - width) / 2,
        y: (area.height - height) / 2,
        width,
        height,
    }
}

/// Each role's model and effort, then the watch thresholds; `•` marks an unsaved change.
fn settings(f: &mut Frame, area: Rect, s: &Settings, theme: &Theme) {
    let file = if s.global { "global" } else { "project" };
    let mut lines = vec![
        Line::raw(s.path.clone()).dim(),
        Line::raw(""),
        Line::raw("Models").dim(),
    ];
    let mut selected = 0;
    for (i, ((_, key, label, _), v)) in SETTINGS.iter().zip(&s.values).enumerate() {
        if i == WATCH {
            lines.extend([Line::raw(""), Line::raw("Watch").dim()]);
        }
        let sel = i == s.row;
        let value = match v {
            toml::Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let value = match sel {
            true => Span::raw(format!("‹ {value} ›")).bold(),
            false => Span::raw(value),
        };
        let name = match i < WATCH {
            true => format!("{label:<11}{key:<8}"),
            false => format!("{label:<19}"),
        };
        let mut row = vec![
            Span::styled(if sel { theme.bar } else { " " }, theme.accent),
            Span::raw(" "),
            Span::raw(name).dim(),
            value,
        ];
        row.extend((*v != s.loaded[i]).then(|| Span::styled(" •", theme.amber)));
        if sel {
            selected = lines.len() as u16;
        }
        lines.push(Line::from(row));
    }
    let block = pane(&format!("Settings · {file} file"), true, theme);
    // a short panel scrolls just enough to keep the selected row in view
    let top = (selected + 1).saturating_sub(block.inner(area).height);
    f.render_widget(Paragraph::new(lines).block(block).scroll((top, 0)), area);
}

fn compose(f: &mut Frame, area: Rect, c: &Compose, theme: &Theme) {
    let [mode, request, ticket, submit] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let button = match c.focus == 3 {
        true => Span::styled(" [ Submit ] ", theme.accent).bold(),
        false => Span::raw(" [ Submit ] ").dim(),
    };
    f.render_widget(Line::from(button), submit);
    let modes = MODES.iter().flat_map(|(m, name)| {
        let name = match *m == c.mode {
            true => Span::styled(*name, theme.accent).bold().underlined(),
            false => Span::raw(*name).dim(),
        };
        [name, Span::raw("  ")]
    });
    let block = pane("Mode", c.focus == 2, theme);
    f.render_widget(Line::from(modes.collect::<Vec<_>>()), block.inner(mode));
    f.render_widget(block, mode);
    for (field, area, title, focused) in [
        (&c.request, request, "Request", c.focus == 0),
        (&c.ticket, ticket, "Ticket", c.focus == 1),
    ] {
        let block = pane(title, focused, theme);
        f.render_widget(field, block.inner(area));
        f.render_widget(block, area);
    }
}

fn header_line(app: &App, theme: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(" yogan", theme.accent).bold(),
        Span::raw(format!(" · {}  ", app.name)).dim(),
    ];
    for (status, _) in GROUPS {
        let n = app.tasks.iter().filter(|(t, _)| t.status == status).count();
        if n > 0 {
            spans.extend([theme.glyph(status, false, 0), Span::raw(format!(" {n}  "))]);
        }
    }
    Line::from(spans)
}

fn pane(title: &str, focused: bool, theme: &Theme) -> Block<'static> {
    let border = if focused {
        Style::new().fg(theme.accent)
    } else {
        Style::new().dim()
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::raw(format!(" {title} ")).dim())
        .padding(Padding::horizontal(1))
}

fn list(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: &Theme,
    focused: bool,
    tick: usize,
    now: SystemTime,
) {
    let block = pane("Tasks", focused, theme);
    let inner = block.inner(area);
    let width = inner.width as usize;
    // compact drops headings and the blank lines between groups; the glyphs carry the status
    let compact = f.area().height < COMPACT;
    let (mut items, mut selected, mut i) = (Vec::new(), None, 0);
    // the row index of each item, for the mouse; None for headings and blanks
    let mut rows = Vec::new();
    let heading = |items: &mut Vec<ListItem>, rows: &mut Vec<_>, label| {
        if compact {
            return;
        }
        if !items.is_empty() {
            items.push(ListItem::new(""));
            rows.push(None);
        }
        items.push(ListItem::new(Line::raw(label).dim()));
        rows.push(None);
    };
    let mut group = None;
    for r in &app.requests {
        let question = r.status == Phase::Done;
        if group != Some(question) {
            let label = if question { "Questions" } else { "Planning" };
            heading(&mut items, &mut rows, label);
            group = Some(question);
        }
        let sel = i == app.selected;
        if sel {
            selected = Some(items.len());
        }
        rows.push(Some(i));
        let (glyph, right) = match r.status {
            Phase::Failed => (Span::styled(theme.fail, theme.red), "failed"),
            Phase::Done => (Span::styled(theme.pass, theme.green), "answered"),
            _ => {
                let spin = theme.spinner[tick % theme.spinner.len()];
                let doing = if r.mode == Mode::Ask {
                    "answering"
                } else {
                    "planning"
                };
                (Span::styled(spin, theme.accent), doing)
            }
        };
        let title = r.text.lines().next().unwrap_or_default();
        items.push(ListItem::new(row_line(
            glyph, title, right, sel, width, theme,
        )));
        i += 1;
    }
    for (status, label) in GROUPS {
        let group: Vec<_> = app
            .tasks
            .iter()
            .filter(|(t, _)| t.status == status)
            .collect();
        if group.is_empty() {
            continue;
        }
        heading(&mut items, &mut rows, label);
        for (t, since) in group {
            let sel = i == app.selected;
            if sel {
                selected = Some(items.len());
            }
            rows.push(Some(i));
            let age = since.and_then(|s| now.duration_since(s).ok());
            let age = age.map(short).unwrap_or_default();
            // ponytail: the step is the status; T16's Activity knows the real one (testing, editing)
            let right = match t.status {
                Status::Running => format!("working · {age}"),
                Status::Checking => format!("gate · {age}"),
                _ => {
                    let tags = [("disputed", &app.disputed), ("serving", &app.serving)];
                    let tags = tags.iter().filter(|(_, ids)| ids.contains(&t.id));
                    tags.map(|(tag, _)| format!("{tag} · ")).collect::<String>() + &age
                }
            };
            let depth = lineage(t, &app.tasks).len() - 1;
            items.push(ListItem::new(row(
                t, depth, &right, sel, width, theme, tick,
            )));
            i += 1;
        }
    }
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(List::new(items).block(block), area, &mut state);
    let mut hits = app.hits.borrow_mut();
    hits.list = area;
    let shown = rows
        .into_iter()
        .skip(state.offset())
        .zip(inner.y..inner.bottom());
    let rows = shown.filter_map(|(i, y)| {
        Some((
            Rect {
                y,
                height: 1,
                ..area
            },
            Target::Row(i?),
        ))
    });
    hits.targets.extend(rows);
}

/// `▌ ⠋ title…          working · 4m`: never wraps, the title gives way.
fn row(
    t: &Task,
    depth: usize,
    right: &str,
    sel: bool,
    width: usize,
    theme: &Theme,
    tick: usize,
) -> Line<'static> {
    let title = if t.title.is_empty() { &t.id } else { &t.title };
    let title = match depth {
        0 => title.clone(),
        d => format!("{}{}{title}", "  ".repeat(d - 1), theme.tree),
    };
    let gate_failed = t.gate.iter().flatten().any(|c| !c.passed);
    let glyph = theme.glyph(t.status, gate_failed, tick);
    row_line(glyph, &title, right, sel, width, theme)
}

/// A list row from its parts: selection bar, glyph, title and dim right-hand text.
fn row_line(
    glyph: Span<'static>,
    title: &str,
    right: &str,
    sel: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let title = truncate(
        title,
        width.saturating_sub(5 + right.width()),
        theme.ellipsis,
    );
    let pad = width.saturating_sub(4 + title.width() + right.width());
    let bar = if sel { theme.bar } else { " " };
    Line::from(vec![
        Span::styled(bar, theme.accent),
        Span::raw(" "),
        glyph,
        Span::raw(" "),
        Span::styled(
            title,
            if sel {
                Style::new().bold()
            } else {
                Style::new()
            },
        ),
        Span::raw(" ".repeat(pad)),
        Span::raw(right.to_string()).dim(),
    ])
}

fn detail(f: &mut Frame, area: Rect, app: &App, theme: &Theme, focused: bool) {
    app.hits.borrow_mut().detail = area;
    let compact = f.area().height < COMPACT;
    let block = pane("Task", focused, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some((t, _)) = app.task() else {
        match app.request() {
            Some(r) => request(f, inner, r, app.answer.as_ref(), theme, &app.scroll),
            None => f.render_widget(Line::raw("No tasks yet.").dim(), inner),
        }
        return;
    };
    let [tabs, _, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if compact { 0 } else { 1 }),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let tabs_line = TABS.iter().enumerate().flat_map(|(i, name)| {
        let name = if i == app.tab {
            Span::raw(*name).bold().underlined()
        } else {
            Span::raw(*name).dim()
        };
        [name, Span::raw("  ")]
    });
    let tabs_line = match compact {
        true => Line::from(vec![Span::raw(TABS[app.tab]).bold(), Span::raw(" ▾").dim()]),
        false => Line::from(tabs_line.collect::<Vec<_>>()),
    };
    // each tab's name presses its digit; compact's one name presses the next tab's
    let digit = |i: usize| {
        Target::Key(KeyEvent::new(
            KeyCode::Char((b'1' + i as u8) as char),
            KeyModifiers::NONE,
        ))
    };
    let mut hits = app.hits.borrow_mut();
    if compact {
        let rect = Rect {
            width: tabs_line.width() as u16,
            ..tabs
        }
        .intersection(tabs);
        hits.targets.push((rect, digit((app.tab + 1) % TABS.len())));
    } else {
        let mut x = tabs.x;
        for (i, name) in TABS.iter().enumerate() {
            let w = name.width() as u16;
            hits.targets
                .push((Rect::new(x, tabs.y, w, 1).intersection(tabs), digit(i)));
            x = x.saturating_add(w + 2);
        }
    }
    drop(hits);
    f.render_widget(tabs_line, tabs);
    match app.tab {
        0 => {
            let parent = t.parent.as_ref().map(|p| {
                let shown = app.tasks.iter().find(|(o, _)| &o.id == p);
                shown.map_or(p.clone(), |(o, _)| o.title.clone())
            });
            summary(f, body, t, parent, app.sessions, &app.scroll)
        }
        1 => activity_tab(f, body, &app.activity, theme, &app.scroll),
        2 => gate_tab(f, body, t, &app.gate_log, theme, &app.scroll),
        FINDINGS => {
            let cursor = app.on_findings().then_some(app.finding);
            findings_tab(f, body, &app.findings, cursor, theme, &app.scroll)
        }
        RUN => run_tab(f, body, t, &app.run, theme, &app.scroll),
        _ => {
            let cursor = app.on_diff().then_some(app.diff_file);
            diff_tab(f, body, &app.diff, cursor, theme, &app.scroll)
        }
    }
}

/// The slot and its ports, each `[scripts]` entry's status, then the run script's output tail.
fn run_tab(f: &mut Frame, area: Rect, t: &Task, run: &RunTab, theme: &Theme, scroll: &Cell<u16>) {
    let mut head = t.slot.map_or(Vec::new(), |n| vec![format!("slot {n}")]);
    head.extend(run.ports.map(|(p, n)| format!("ports {p}-{}", p + n - 1)));
    let scripts = run.scripts.iter().flat_map(|&name| {
        let glyph = match name {
            "run" if run.pid.is_some() => Span::styled(theme.checking, theme.accent),
            // setup runs before Claude does, so a failed task with no session failed there
            "setup" if t.status == Status::Failed && t.sessions.is_empty() => {
                Span::styled(theme.fail, theme.red)
            }
            "setup" => Span::styled(theme.pass, theme.green),
            _ => Span::raw(theme.queued).dim(),
        };
        let state = match (name, run.pid) {
            ("run", Some(_)) => " running",
            ("run", None) => " stopped",
            ("teardown", _) => " when freed",
            _ => "",
        };
        [
            glyph,
            Span::raw(format!(" {name}")),
            Span::raw(format!("{state}  ")).dim(),
        ]
    });
    let [top, list, _, output] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Line::raw(head.join(" · ")).dim(), top);
    f.render_widget(Line::from(scripts.collect::<Vec<_>>()), list);
    let empty = match run.scripts.contains(&"run") {
        false => Some("No [scripts] run in the project file."),
        true if run.log.is_empty() && run.pid.is_none() => Some("R starts the run script."),
        true => None,
    };
    if let Some(text) = empty {
        f.render_widget(Line::raw(text).dim(), output);
        return;
    }
    let mut lines: Vec<Line> = run.log.lines().map(|l| Line::raw(l.to_string())).collect();
    lines.truncate(lines.len() - from_tail(lines.len(), output.height, scroll));
    let skip = lines.len().saturating_sub(output.height as usize);
    f.render_widget(Paragraph::new(lines.split_off(skip)), output);
}

/// `parent` is the parent task's title; `sessions` the most a task gets, shown once it hands off.
fn summary(
    f: &mut Frame,
    area: Rect,
    t: &Task,
    parent: Option<String>,
    sessions: u32,
    scroll: &Cell<u16>,
) {
    let label = GROUPS
        .iter()
        .find(|(s, _)| *s == t.status)
        .map_or("", |g| g.1);
    let mut meta = vec![label.to_string()];
    meta.extend((!t.branch.is_empty()).then(|| t.branch.clone()));
    meta.extend(t.slot.map(|n| format!("slot {n}")));
    let n = t.sessions.len();
    meta.extend((n > 1).then(|| format!("session {n}/{sessions}")));
    meta.extend((t.spent() > 0.0).then(|| format!("${:.2}", t.spent())));
    meta.extend(t.model.as_ref().map(|m| match &t.effort {
        Some(e) => format!("{m}/{e}"),
        None => m.clone(),
    }));
    meta.extend(t.ticket.clone());
    meta.extend((!t.crates.is_empty()).then(|| t.crates.join(", ")));
    meta.extend(parent.map(|p| format!("after “{p}”")));
    meta.extend(t.pr_url.clone());
    let mut lines = vec![
        Line::raw(t.title.clone()).bold(),
        Line::raw(meta.join(" · ")).dim(),
        Line::raw(""),
    ];
    if let Some(summary) = &t.summary {
        lines.extend(summary.lines().map(|l| Line::raw(l.to_string())));
        lines.push(Line::raw(""));
    }
    lines.extend(t.body.lines().map(|l| Line::raw(l.to_string()).dim()));
    if !t.acceptance.is_empty() {
        lines.extend([Line::raw(""), Line::raw("Done when").bold()]);
        lines.extend(t.acceptance.iter().map(|a| Line::raw(format!("- {a}"))));
    }
    scrolled(
        f,
        area,
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        scroll,
    );
}

/// A request: its first line, status and ticket, why its lead failed, then the full request;
/// for a question, the answer and the files it cites instead.
fn request(
    f: &mut Frame,
    area: Rect,
    r: &Request,
    answer: Option<&(String, String, Vec<String>)>,
    theme: &Theme,
    scroll: &Cell<u16>,
) {
    let failed = r.status == Phase::Failed;
    let status = match r.status {
        Phase::Failed => "Failed",
        Phase::Done => "Question",
        _ => "Planning",
    };
    let mut meta = vec![status.to_string()];
    meta.extend(r.ticket.clone());
    let mut lines = vec![
        Line::raw(r.text.lines().next().unwrap_or_default().to_string()).bold(),
        Line::raw(meta.join(" · ")).dim(),
        Line::raw(""),
    ];
    if failed && let Some(why) = &r.summary {
        lines.extend(why.lines().map(|l| Line::styled(l.to_string(), theme.red)));
        lines.push(Line::raw(""));
    }
    match answer {
        Some((_, text, cited)) => {
            lines.extend(markdown(text, theme));
            if !cited.is_empty() {
                lines.extend([Line::raw(""), Line::raw("Cites").bold()]);
                lines.extend(cited.iter().map(|c| Line::raw(format!("- {c}")).dim()));
            }
        }
        None => lines.extend(r.text.lines().map(|l| Line::raw(l.to_string()).dim())),
    }
    scrolled(
        f,
        area,
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        scroll,
    );
}

/// Markdown, lightly: headings bold, fenced code in the shell hue, the rest as written.
fn markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut code = false;
    let mut lines = Vec::new();
    for l in text.lines() {
        if l.trim_start().starts_with("```") {
            code = !code;
        } else if code {
            lines.push(Line::styled(format!("  {l}"), theme.shell));
        } else if l.starts_with('#') {
            lines.push(Line::raw(l.trim_start_matches('#').trim().to_string()).bold());
        } else {
            lines.push(Line::raw(l.to_string()));
        }
    }
    lines
}

/// The repo files `text` cites as `path` or `path:line`, once each, in order.
fn cites(repo: &Path, text: &str) -> Vec<String> {
    let re = Regex::new(r"[\w./-]+\.\w+(?::\d+(?:-\d+)?)?").expect("valid regex");
    let mut found: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let cite = m.as_str().trim_start_matches("./");
        let path = cite.split(':').next().unwrap_or(cite);
        if repo.join(path).is_file() && !found.iter().any(|f| f == cite) {
            found.push(cite.into());
        }
    }
    found
}

/// Copies `text` with OSC 52, which reaches the local clipboard even over SSH, and with the
/// native clipboard tool when there is one.
fn copy(text: &str) -> Result<()> {
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()?;
    let tools: [(&str, &[&str]); 3] = [
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    for (tool, args) in tools {
        let Ok(mut child) = Command::new(tool)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        child
            .stdin
            .take()
            .context("clipboard stdin")?
            .write_all(text.as_bytes())?;
        child.wait()?;
        break;
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk.iter().enumerate()).fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            let c = ABC[(n >> (18 - 6 * i) & 63) as usize] as char;
            out.push(if i <= chunk.len() { c } else { '=' });
        }
    }
    out
}

/// The critic's findings by what became of them, each with its evidence and the worker's or the
/// human's reply; `cursor` marks the selected one of the actionable (open, disputed, optional).
fn findings_tab(
    f: &mut Frame,
    area: Rect,
    fs: &Findings,
    cursor: Option<usize>,
    theme: &Theme,
    scroll: &Cell<u16>,
) {
    let mut lines = Vec::new();
    if let Some(e) = &fs.error {
        lines.push(Line::styled(
            format!("The critic didn't finish: {e}"),
            theme.red,
        ));
    }
    let sections = [
        ("Open", &fs.findings, true, "worker"),
        ("Disputed", &fs.disputed, true, "worker"),
        ("Optional, unproven", &fs.optional, true, "worker"),
        ("Fixed", &fs.fixed, false, "worker"),
        ("Waived", &fs.waived, false, "reason"),
    ];
    let mut i = 0;
    for (label, list, actionable, who) in sections {
        if list.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(label).dim());
        for x in list {
            let sel = actionable && cursor == Some(i);
            i += usize::from(actionable);
            let glyph = match x.severity {
                Severity::Blocker => Span::styled(theme.fail, theme.red),
                Severity::Major => Span::styled(theme.checking, theme.amber),
                Severity::Minor => Span::raw(theme.queued).dim(),
            };
            let claim = Span::raw(x.claim.clone());
            lines.push(Line::from(vec![
                Span::styled(if sel { theme.bar } else { " " }, theme.accent),
                Span::raw(" "),
                glyph,
                Span::raw(format!(" {} ", x.location)).dim(),
                if sel { claim.bold() } else { claim },
            ]));
            if !x.evidence.is_empty() {
                lines.push(Line::raw(format!("    {}", x.evidence)).dim());
            }
            if let Some(reply) = &x.reply {
                lines.push(Line::raw(format!("    {who}: {reply}")).dim());
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::raw("No findings yet.").dim());
    }
    scrolled(
        f,
        area,
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        scroll,
    );
}

/// One line per tool call, following the tail: edits in the accent, shell commands in a
/// second hue, reads dim, nudges amber.
fn activity_tab(
    f: &mut Frame,
    area: Rect,
    calls: &[(String, String)],
    theme: &Theme,
    scroll: &Cell<u16>,
) {
    if calls.is_empty() {
        f.render_widget(Line::raw("No tool calls yet.").dim(), area);
        return;
    }
    let width = area.width as usize;
    let end = calls.len() - from_tail(calls.len(), area.height, scroll);
    let shown = &calls[end.saturating_sub(area.height as usize)..end];
    let lines = shown.iter().map(|(tool, target)| {
        let (verb, style) = match tool.as_str() {
            "Edit" | "NotebookEdit" => ("edit", Style::new().fg(theme.accent)),
            "Write" => ("write", Style::new().fg(theme.accent)),
            "Bash" => ("run", Style::new().fg(theme.shell)),
            "Read" => ("read", Style::new().dim()),
            "Grep" | "Glob" => ("search", Style::new().dim()),
            NUDGE => ("↻ nudged", Style::new().fg(theme.amber)),
            HANDOFF => ("⇢ handoff", Style::new()),
            other => (other, Style::new()),
        };
        let target = truncate(
            target,
            width.saturating_sub(verb.width().max(7) + 1),
            theme.ellipsis,
        );
        let target = match tool.as_str() {
            NUDGE => Span::styled(target, style),
            _ => Span::raw(target).dim(),
        };
        Line::from(vec![Span::styled(format!("{verb:<7} "), style), target])
    });
    f.render_widget(Paragraph::new(lines.collect::<Vec<_>>()), area);
}

/// The checks as a table, then the tail of each failing step's output from the gate log.
fn gate_tab(f: &mut Frame, area: Rect, t: &Task, log: &str, theme: &Theme, scroll: &Cell<u16>) {
    let Some(checks) = &t.gate else {
        f.render_widget(Line::raw("The gate hasn't run yet.").dim(), area);
        return;
    };
    let rows = checks.iter().map(|c| match c.passed {
        true => Line::from(vec![
            Span::styled(theme.pass, theme.green),
            Span::raw(format!("  {}", c.name)),
        ]),
        false => Line::styled(format!("{}  {}", theme.fail, c.name), theme.red),
    });
    let [table, _, output] = Layout::vertical([
        Constraint::Length(checks.len() as u16),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(rows.collect::<Vec<_>>()), table);
    // sections are `== <name>: ok|FAILED` followed by the step's output
    let mut lines = Vec::new();
    for section in log.split("== ").filter(|s| !s.is_empty()) {
        let (head, body) = section.split_once('\n').unwrap_or((section, ""));
        if let Some(name) = head.strip_suffix(": FAILED") {
            lines.push(Line::styled(
                name.to_string(),
                Style::new().fg(theme.red).bold(),
            ));
            lines.extend(body.lines().map(|l| Line::raw(l.to_string())));
        }
    }
    lines.truncate(lines.len() - from_tail(lines.len(), output.height, scroll));
    let skip = lines.len().saturating_sub(output.height as usize);
    f.render_widget(Paragraph::new(lines.split_off(skip)), output);
}

/// git-style `+`/`-` counts with bars scaled to the largest change; `cursor` marks the file
/// `enter` opens, and stays in view.
fn diff_tab(
    f: &mut Frame,
    area: Rect,
    files: &[(String, u64, u64)],
    cursor: Option<usize>,
    theme: &Theme,
    scroll: &Cell<u16>,
) {
    if files.is_empty() {
        f.render_widget(Line::raw("No changes yet.").dim(), area);
        return;
    }
    let (added, deleted) = files.iter().fold((0, 0), |(a, d), f| (a + f.1, d + f.2));
    let max = files.iter().map(|f| f.1 + f.2).max().unwrap_or(1).max(1);
    let bar_width = 20u64.min(max);
    let bar = if cursor.is_some() { 2 } else { 0 };
    let path_width = (area.width as usize).saturating_sub(13 + bar + bar_width as usize);
    let scale = |n: u64| ((n * bar_width).div_ceil(max)) as usize;
    if let Some(c) = cursor.map(|c| c as u16) {
        let top = scroll.get().max((c + 1).saturating_sub(area.height));
        scroll.set(top.min(c));
    }
    let mut lines: Vec<Line> = files
        .iter()
        .enumerate()
        .map(|(i, (path, a, d))| {
            let path = truncate(path, path_width, theme.ellipsis);
            let sel = cursor == Some(i);
            let mark = match cursor {
                Some(_) => format!("{} ", if sel { theme.bar } else { " " }),
                None => String::new(),
            };
            let path = Span::raw(format!("{path:<path_width$} "));
            Line::from(vec![
                Span::styled(mark, theme.accent),
                if sel { path.bold() } else { path },
                Span::styled(format!("{:>5}", format!("+{a}")), theme.green),
                Span::styled(format!("{:>6} ", format!("-{d}")), theme.red),
                Span::styled("+".repeat(scale(*a)), theme.green),
                Span::styled("-".repeat(scale(*d)), theme.red),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    let total = format!("{} files changed, +{added} -{deleted}", files.len());
    lines.push(Line::raw(total).dim());
    scrolled(f, area, Paragraph::new(lines), scroll);
}

fn help(f: &mut Frame, theme: &Theme) {
    let area = centered(f.area(), 34, KEYS.len() as u16 + 2);
    let lines = KEYS.iter().map(|(key, label)| {
        Line::from(vec![
            Span::styled(format!("{key:<8}"), theme.accent),
            Span::raw(*label),
        ])
    });
    f.render_widget(Clear, area);
    let block = pane("Keys", true, theme);
    f.render_widget(Paragraph::new(lines.collect::<Vec<_>>()).block(block), area);
}

fn short(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Cuts `s` to `max` columns, ending in `ellipsis` when it had to cut.
fn truncate(s: &str, max: usize, ellipsis: &str) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = ellipsis.width();
    for c in s.chars() {
        used += c.width().unwrap_or(0);
        if used > max {
            break;
        }
        out.push(c);
    }
    out + ellipsis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Check;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> (App, SystemTime) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |m: u64| Some(now - Duration::from_secs(m * 60));
        let task = |id: &str, title: &str, status| Task {
            id: id.into(),
            title: title.into(),
            status,
            ..Default::default()
        };
        let mut review = task("t1", "Reject negative max_delay", Status::Review);
        review.branch = "u/reject-negative".into();
        review.slot = Some(1);
        review.model = Some("opus".into());
        review.effort = Some("high".into());
        review.summary = Some("max_delay below zero now fails at parse time.".into());
        review.gate = Some(vec![Check {
            name: "test".into(),
            passed: true,
        }]);
        let tasks = vec![
            (review, ago(4)),
            (task("t2", "Bump sqlx to 0.9", Status::Failed), ago(90)),
            (task("t3", "Retry webhook sends", Status::Running), ago(12)),
            (
                task(
                    "t4",
                    "Split the ledger reconciliation job into per-account batches",
                    Status::Approved,
                ),
                ago(2),
            ),
        ];
        let app = App {
            repo: PathBuf::new(),
            state: PathBuf::new(),
            name: "fuse-os".into(),
            requests: Vec::new(),
            tasks,
            selected: 0,
            detail: false,
            help: false,
            compose: None,
            notice: None,
            info: None,
            opening: false,
            tab: 0,
            activity: Vec::new(),
            gate_log: String::new(),
            diff: Vec::new(),
            loaded: None,
            confirm: false,
            pager: None,
            preview: false,
            awaiting: None,
            instruction: None,
            edit: false,
            reply: None,
            answer: None,
            reply_for: None,
            commits: Vec::new(),
            commit: 0,
            interactive: None,
            scroll: Cell::new(0),
            findings: Findings::default(),
            finding: 0,
            disputed: Vec::new(),
            sessions: 3,
            run: RunTab::default(),
            serving: Vec::new(),
            settings: None,
            vscode: false,
            editing: None,
            open_at: None,
            diff_file: 0,
            hits: RefCell::default(),
        };
        (app, now)
    }

    /// The screen's text, one string per row; styles aren't compared.
    fn screen(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        let rows = buf.content.chunks(buf.area.width as usize);
        rows.map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    /// The detail pane alone (narrow layout) on `tab`.
    fn tab_screen(mut app: App, tab: usize) -> Vec<String> {
        app.detail = true;
        app.tab = tab;
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        screen(&term)
    }

    #[test]
    fn summary_tab() {
        let (mut app, _) = app();
        app.tasks[0].0.ticket = Some("CC-687".into());
        app.tasks[0].0.body = "A negative value panics in the retry loop.".into();
        app.tasks[0].0.sessions = vec!["s-1".into(), "s-2".into()];
        assert_eq!(
            tab_screen(app, 0),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary ▾                                                │",
                "│ Reject negative max_delay                                │",
                "│ Review · u/reject-negative · slot 1 · session 2/3 ·      │",
                "│ opus/high · CC-687                                       │",
                "│                                                          │",
                "│ max_delay below zero now fails at parse time.            │",
                "│                                                          │",
                "│ A negative value panics in the retry loop.               │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   m  open PR  ?  more  ",
            ]
        );
    }

    #[test]
    fn scrolling_the_detail_pane() {
        let (mut app, _) = app();
        let body: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        app.tasks[0].0.summary = Some(body.join("\n"));
        app.detail = true;
        let draw_rows = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
            let theme = Theme::new(false, false);
            term.draw(|f| draw(f, app, &theme, 0, SystemTime::UNIX_EPOCH))
                .unwrap();
            screen(&term)
        };
        let shows =
            |rows: &[String], text: &str| rows.iter().any(|r| r.contains(&format!("│ {text} ")));
        let press = |app: &mut App, code, mods| app.key(KeyEvent::new(code, mods));
        assert!(shows(&draw_rows(&app), "line 1"));

        // page down, then far past the end: the last page stays in view
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        let rows = draw_rows(&app);
        assert!(
            !shows(&rows, "line 1") && shows(&rows, "line 10"),
            "{rows:#?}"
        );
        for _ in 0..9 {
            press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        }
        let rows = draw_rows(&app);
        assert!(shows(&rows, "line 30"), "{rows:#?}");
        // clamped, so one step back moves at once
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(!shows(&draw_rows(&app), "line 30"));

        // Activity counts from its tail: scrolling up shows older calls
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(app.scroll.get(), 0, "a new tab starts at the top");
        app.activity = (1..=30)
            .map(|i| ("Read".into(), format!("f{i}.rs")))
            .collect();
        let newest = |rows: &[String]| rows.iter().any(|r| r.contains("f30.rs"));
        assert!(newest(&draw_rows(&app)));
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let rows = draw_rows(&app);
        assert!(
            !newest(&rows) && rows.iter().any(|r| r.contains("f20.rs")),
            "{rows:#?}"
        );
    }

    #[test]
    fn activity_tab_follows_tail() {
        let (mut app, _) = app();
        let call = |tool: &str, target: &str| (tool.to_string(), target.to_string());
        app.activity = vec![
            call("Read", "src/old.rs"),
            call("Read", "src/config.rs"),
            call("Grep", "max_delay"),
            call("Edit", "src/config.rs"),
            call("Bash", "cargo test -p ledger"),
            call(
                "Write",
                "crates/ledger/src/limits/negative_delay_regression.rs",
            ),
            call("TodoWrite", ""),
        ];
        assert_eq!(
            tab_screen(app, 1),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Activity ▾                                               │",
                "│ read    src/old.rs                                       │",
                "│ read    src/config.rs                                    │",
                "│ search  max_delay                                        │",
                "│ edit    src/config.rs                                    │",
                "│ run     cargo test -p ledger                             │",
                "│ write   crates/ledger/src/limits/negative_delay_regress… │",
                "│ TodoWrite                                                │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   m  open PR  ?  more  ",
            ]
        );
    }

    #[test]
    fn gate_tab_shows_failure() {
        let (mut app, _) = app();
        let check = |name: &str, passed| Check {
            name: name.into(),
            passed,
        };
        app.tasks[0].0.gate = Some(vec![
            check("clean tree", true),
            check("fmt", true),
            check("test", false),
        ]);
        app.gate_log = "== clean tree: ok\n\n== fmt: ok\n$ cargo fmt -p ledger\n\n\
                        == test: FAILED\n$ cargo test -p ledger\n\
                        thread 'parse' panicked at src/config.rs:40:9:\n\
                        assertion failed: delay >= 0\n"
            .into();
        assert_eq!(
            tab_screen(app, 2),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Gate ▾                                                   │",
                "│ ✓  clean tree                                            │",
                "│ ✓  fmt                                                   │",
                "│ ✗  test                                                  │",
                "│                                                          │",
                "│ test                                                     │",
                "│ $ cargo test -p ledger                                   │",
                "│ thread 'parse' panicked at src/config.rs:40:9:           │",
                "│ assertion failed: delay >= 0                             │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   r  reply  ?  more    ",
            ]
        );
    }

    #[test]
    fn run_tab_shows_ports_scripts_and_output() {
        let (mut app, _) = app();
        app.run = RunTab {
            loaded: Some(("t1".into(), Some(1))),
            ports: Some((1160, 80)),
            scripts: vec!["setup", "run", "teardown"],
            pid: Some(4242),
            log: "> vite\nready on http://localhost:1160\n".into(),
        };
        app.serving = vec!["t1".into()];
        assert_eq!(
            tab_screen(app, RUN),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Run ▾                                                    │",
                "│ slot 1 · ports 1160-1239                                 │",
                "│ ✓ setup  ◆ run running  ○ teardown when freed            │",
                "│                                                          │",
                "│ > vite                                                   │",
                "│ ready on http://localhost:1160                           │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   m  open PR  ?  more  ",
            ]
        );
    }

    #[test]
    fn settings_save_and_reload() {
        let root = std::env::temp_dir().join(format!("yogan-tui-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (home, checkout) = (root.join("home"), root.join("work/trader"));
        fs::create_dir_all(&checkout).unwrap();
        let mut s = Settings::load(&checkout, &home, false).unwrap();
        let err = s.save(&checkout, &home).unwrap_err().to_string();
        assert_eq!(err, "nothing changed");
        s.row = 7;
        s.cycle(true); // workers' effort: high to xhigh
        s.row = WATCH;
        s.cycle(true); // stall after: 15m to 20m
        s.row = 14;
        s.cycle(false); // handoff at: 0.8 to 0.75
        s.row = 4;
        s.cycle(true); // critic: fable wraps round to opus

        let (mut app, _) = app();
        app.settings = Some(s);
        let mut term = Terminal::new(TestBackend::new(60, 26)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Settings · project file ─────────────────────────────────╮",
                "│ ~/.config/yogan/projects/trader.toml                     │",
                "│                                                          │",
                "│ Models                                                   │",
                "│   lead       model   claude-opus-5-5                     │",
                "│              effort  xhigh                               │",
                "│   questions  model   claude-opus-5-5                     │",
                "│              effort  high                                │",
                "│ ▌ critic     model   ‹ claude-opus-5-5 › •               │",
                "│              effort  max                                 │",
                "│   workers    model   claude-opus-5-5                     │",
                "│              effort  xhigh •                             │",
                "│   PR drafts  model   claude-opus-5-5                     │",
                "│              effort  medium                              │",
                "│                                                          │",
                "│ Watch                                                    │",
                "│   stall after        20m •                               │",
                "│   nudges             1                                   │",
                "│   loop repeats       4                                   │",
                "│   autocompact        200000                              │",
                "│   handoff at         0.75 •                              │",
                "│   max handoffs       2                                   │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " j/k  field  ←→  change  g  global file  ctrl-s  save  esc  ",
            ]
        );

        // a short panel scrolls to keep the selected row in view
        app.settings.as_mut().unwrap().row = SETTINGS.len() - 1;
        let mut term = Terminal::new(TestBackend::new(60, 16)).unwrap();
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        let rows = screen(&term);
        assert!(
            rows.iter()
                .any(|r| r.contains("max handoffs") && r.contains("‹ 2 ›")),
            "{rows:#?}"
        );

        let mut s = app.settings.take().unwrap();
        let path = s.save(&checkout, &home).unwrap();
        let again = Settings::load(&checkout, &home, false).unwrap();
        assert_eq!(again.values, s.values);
        assert_eq!(again.loaded, again.values);
        // only the changed keys are written
        let written: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(written["worker"].as_table().unwrap().len(), 1);
        assert!(written.get("lead").is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn diff_tab_bars() {
        let (mut app, _) = app();
        app.diff = vec![
            ("src/config.rs".into(), 12, 3),
            ("crates/ledger/src/limits.rs".into(), 40, 0),
            ("assets/logo.png".into(), 0, 0),
        ];
        assert_eq!(
            tab_screen(app, 4),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Diff ▾                                                   │",
                "│ ▌ src/config.rs           +12    -3 ++++++--             │",
                "│   crates/ledger/src/li…   +40    -0 ++++++++++++++++++++ │",
                "│   assets/logo.png          +0    -0                      │",
                "│                                                          │",
                "│ 3 files changed, +52 -3                                  │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   j/k  file  ?  more   ",
            ]
        );
    }

    #[test]
    fn awaiting_a_pr_draft() {
        let (mut app, _) = app();
        let draft = pr::Draft {
            title: "feat: x [CC-1]".into(),
            body: String::new(),
            head: "abc".into(),
            problem: None,
        };
        app.awaiting = Some(("t1".into(), None, 1));
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && !app.preview, "still drafting");
        let err = app.draft(None).unwrap_err().to_string();
        assert_eq!(err, "a PR draft is on its way");

        // a new draft opens the preview on its task, once no modal would act on it
        app.selected = 2;
        app.tasks[0].0.pr_draft = Some(draft.clone());
        app.confirm = true;
        app.check_awaiting(false);
        assert!(app.awaiting.is_some() && !app.preview && app.selected == 2);
        app.confirm = false;
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.preview && app.selected == 0);

        // the worker exited without a new draft: its error is shown
        let state = std::env::temp_dir().join(format!("yogan-awaiting-{}", std::process::id()));
        fs::create_dir_all(state.join("logs")).unwrap();
        fs::write(state.join("logs/t1.pr.log"), "claude exited with 1\n").unwrap();
        app.state = state.clone();
        app.awaiting = Some(("t1".into(), Some(draft.clone()), 1));
        app.check_awaiting(true);
        assert!(app.awaiting.is_none());
        assert_eq!(
            app.notice.take().as_deref(),
            Some("no PR draft: claude exited with 1")
        );
        fs::remove_dir_all(&state).unwrap();

        // a failed task or gate gives up; so does a task that's gone
        app.awaiting = Some(("t2".into(), None, 1));
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.notice.take().is_some());
        app.tasks[0].0.gate.as_mut().unwrap()[0].passed = false;
        app.awaiting = Some(("t1".into(), Some(draft), 1));
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.notice.take().is_some());
        app.awaiting = Some(("gone".into(), None, 1));
        app.check_awaiting(false);
        assert!(app.awaiting.is_none() && app.notice.is_none());
    }

    #[test]
    fn approving_proposals() {
        let task = |id: &str, status, parent: Option<&str>, krate: &str| Task {
            id: id.into(),
            title: id.into(),
            status,
            parent: parent.map(Into::into),
            crates: vec![krate.into()],
            ..Default::default()
        };
        let mut tasks = vec![
            task("run", Status::Running, None, "ledger"),
            task("p1", Status::Proposed, None, "config"),
            task("p2", Status::Proposed, Some("p1"), "cli"),
            task("p3", Status::Proposed, None, "ledger"),
        ];
        let err = approve(&mut tasks.clone(), &["p2".into()]).unwrap_err();
        assert_eq!(err.to_string(), "approve the parent of “p2” first");
        let warning = approve(&mut tasks, &["p1".into(), "p2".into()]).unwrap();
        assert_eq!(warning, None);
        assert_eq!(
            (tasks[1].status, tasks[2].status),
            (Status::Approved, Status::Approved)
        );
        let warning = approve(&mut tasks, &["p3".into()]).unwrap();
        assert_eq!(
            warning.as_deref(),
            Some("“p3” shares a crate with “run”, so expect a rebase")
        );
    }

    #[test]
    fn editing_a_proposal() {
        let t = Task {
            id: "p1".into(),
            title: "Old".into(),
            status: Status::Proposed,
            branch: "u/old".into(),
            acceptance: vec!["a".into()],
            ..Default::default()
        };
        let text = "title = \"New\"\nbody = \"\"\nacceptance = [\"a\", \"b\"]\ncrates = []\n\
                    model = \"sonnet\"\nstatus = \"approved\"\nbranch = \"x\"\n";
        let e = edited(&t, text).unwrap();
        assert_eq!(
            (e.title.as_str(), e.acceptance.len(), e.model.as_deref()),
            ("New", 2, Some("sonnet"))
        );
        // yogan's own fields ignore the editor
        assert_eq!(
            (e.id.as_str(), e.status, e.branch.as_str()),
            ("p1", Status::Proposed, "u/old")
        );
        assert!(edited(&t, "title = [").is_err());
        let err = edited(&t, "title = \"New\"").unwrap_err().to_string();
        assert!(err.contains("missing field"), "{err}");
    }

    #[test]
    fn planning_screen() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-planning-{}", std::process::id()));
        let proposal = |id: &str, title: &str, parent: Option<&str>, crates: &[&str]| Task {
            id: id.into(),
            title: title.into(),
            status: Status::Proposed,
            plan: "r1".into(),
            ticket: Some("CC-687".into()),
            parent: parent.map(Into::into),
            crates: crates.iter().map(|c| c.to_string()).collect(),
            ..Default::default()
        };
        let mut child = proposal(
            "p3",
            "Reject negative max_delay in the CLI",
            Some("p1"),
            &["cli", "config"],
        );
        child.body = "Reuse the config check in the CLI.".into();
        child.acceptance = vec![
            "`yogan --max-delay -1` exits 2".into(),
            "the error names the flag".into(),
        ];
        // ids sort p1, p2, p3, but the child p3 goes right under its parent p1
        let tasks = [
            app.tasks[0].0.clone(),
            proposal("p1", "Validate max_delay at parse time", None, &["config"]),
            proposal("p2", "Bump sqlx to 0.9", None, &["db"]),
            child,
        ];
        for t in &tasks {
            t.save(&state).unwrap();
        }
        app.reload(&state).unwrap();
        fs::remove_dir_all(&state).unwrap();
        app.selected = 2;
        let mut term = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let theme = Theme::new(false, false);
        // file ages are later than this `now`, so no ages show
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│   ✓ Reject negative max_delay             ││ Summary ▾                                           │",
                "│   ○ Validate max_delay at parse time      ││ Reject negative max_delay in the CLI                │",
                "│ ▌ ○ └ Reject negative max_delay in the …  ││ Proposed · CC-687 · cli, config · after “Validate   │",
                "│   ○ Bump sqlx to 0.9                      ││ max_delay at parse time”                            │",
                "│                                           ││                                                     │",
                "│                                           ││ Reuse the config check in the CLI.                  │",
                "│                                           ││                                                     │",
                "│                                           ││ Done when                                           │",
                "│                                           ││ - `yogan --max-delay -1` exits 2                    │",
                "│                                           ││ - the error names the flag                          │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ○ 3   a  approve  A  approve all  e  edit  r  reply  x  discard  ?  more     ",
            ]
        );
    }

    #[test]
    fn requests_screen() {
        let (mut app, now) = app();
        app.requests = vec![
            Request {
                id: "r1".into(),
                text: "Split the ledger job into per-account batches".into(),
                ..Default::default()
            },
            Request {
                id: "r2".into(),
                text: "Reject a negative max_delay\nIt panics in the retry loop.".into(),
                ticket: Some("CC-687".into()),
                status: Phase::Failed,
                summary: Some("claude exited with exit status: 1".into()),
                ..Default::default()
            },
        ];
        app.selected = 1;
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│   ⠋ Split the ledger job into p… planning ││ Reject a negative max_delay                         │",
                "│ ▌ ✗ Reject a negative max_delay    failed ││ Failed · CC-687                                     │",
                "│   ✓ Reject negative max_delay          4m ││                                                     │",
                "│   ✗ Bump sqlx to 0.9                   1h ││ claude exited with exit status: 1                   │",
                "│   ⠋ Retry webhook sends     working · 12m ││                                                     │",
                "│   ○ Split the ledger reconciliation j… 2m ││ Reject a negative max_delay                         │",
                "│                                           ││ It panics in the retry loop.                        │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   t  retry  x  discard  n  new task  j/k  move  ?  more        ",
            ]
        );
        assert!(app.task().is_none());
        app.selected = 2;
        assert_eq!(app.task().unwrap().0.id, "t1");
    }

    #[test]
    fn dismissing_a_failed_request() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-dismiss-{}", std::process::id()));
        let failed = Request {
            id: "r2".into(),
            text: "Reject a negative max_delay".into(),
            status: Phase::Failed,
            ..Default::default()
        };
        failed.save(&state).unwrap();
        app.state = state.clone();
        app.reload(&state).unwrap();
        app.selected = 0;
        let press = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert!(app.key(press('x')) && app.confirm);
        assert!(app.key(press('y')) && app.notice.is_none());
        assert_eq!(lead::load(&state, "r2").unwrap().status, Phase::Dismissed);
        app.reload(&state).unwrap();
        assert!(app.requests.is_empty());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn question_screen() {
        let (mut app, now) = app();
        app.requests = vec![Request {
            id: "r5".into(),
            text: "How are tasks saved?".into(),
            mode: Mode::Ask,
            status: Phase::Done,
            ..Default::default()
        }];
        let answer = "## Atomically\nA tmp file, then a rename:\n```\nfs::rename(&tmp, &path)\n```";
        let cited = vec!["src/task.rs:72".to_string()];
        app.answer = Some(("r5".into(), answer.into(), cited));
        let mut term = Terminal::new(TestBackend::new(100, 14)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ ▌ ✓ How are tasks saved?         answered ││ How are tasks saved?                                │",
                "│   ✓ Reject negative max_delay          4m ││ Question                                            │",
                "│   ✗ Bump sqlx to 0.9                   1h ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││ Atomically                                          │",
                "│   ○ Split the ledger reconciliation j… 2m ││ A tmp file, then a rename:                          │",
                "│                                           ││   fs::rename(&tmp, &path)                           │",
                "│                                           ││                                                     │",
                "│                                           ││ Cites                                               │",
                "│                                           ││ - src/task.rs:72                                    │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   p  plan it  r  reply  y  copy  x  discard  ?  more           ",
            ]
        );

        // p: a plan built on the answer
        let press = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        app.key(press('p'));
        let c = app.compose.as_ref().unwrap();
        assert_eq!(c.mode, Mode::Plan);
        let text = c.request.lines().join("\n");
        assert!(
            text.starts_with("How are tasks saved?\n\nThe answer to build on:\n\n## Atomically")
        );
    }

    #[test]
    fn finding_locations() {
        assert_eq!(path_line("src/a.rs:41"), ("src/a.rs".into(), Some(41)));
        assert_eq!(path_line("src/a.rs:41-45"), ("src/a.rs".into(), Some(41)));
        assert_eq!(path_line("src/a.rs"), ("src/a.rs".into(), None));
    }

    #[test]
    fn cites_and_base64() {
        let repo = std::env::temp_dir().join(format!("yogan-cites-{}", std::process::id()));
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "").unwrap();
        let text = "See `src/lib.rs:3`, ./src/lib.rs and gone.rs. Also src/lib.rs:3 again.";
        assert_eq!(cites(&repo, text), ["src/lib.rs:3", "src/lib.rs"]);
        fs::remove_dir_all(&repo).unwrap();
        assert_eq!(
            [base64(b"Man"), base64(b"Ma"), base64(b"M")],
            ["TWFu", "TWE=", "TQ=="]
        );
    }

    #[test]
    fn child_of_a_discarded_parent_keeps_its_base() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-base-{}", std::process::id()));
        let parent = Task {
            id: "p1".into(),
            branch: "u/parent".into(),
            status: Status::Discarded,
            ..Default::default()
        };
        let child = Task {
            id: "c1".into(),
            parent: Some("p1".into()),
            status: Status::Review,
            ..Default::default()
        };
        parent.save(&state).unwrap();
        child.save(&state).unwrap();
        app.state = state.clone();
        app.reload(&state).unwrap();
        assert_eq!(app.task().unwrap().0.id, "c1", "the parent isn't listed");
        assert_eq!(app.base().unwrap(), "origin/u/parent");
        fs::remove_dir_all(&state).unwrap();
    }

    fn sample_findings() -> Findings {
        use crate::critic::Finding;
        let finding =
            |severity, location: &str, claim: &str, evidence: &str, reply: Option<&str>| Finding {
                severity,
                location: location.into(),
                claim: claim.into(),
                evidence: evidence.into(),
                reply: reply.map(Into::into),
            };
        Findings {
            findings: vec![finding(
                Severity::Blocker,
                "src/config.rs:41",
                "-1 still parses",
                "cargo test negative fails",
                None,
            )],
            disputed: vec![finding(
                Severity::Major,
                "src/retry.rs:9",
                "no test for zero",
                "no test calls it with 0",
                Some("0 is covered by the default"),
            )],
            optional: vec![finding(
                Severity::Major,
                "src/lib.rs:3",
                "may overflow",
                "",
                None,
            )],
            waived: vec![finding(
                Severity::Minor,
                "src/config.rs:40",
                "rename it",
                "",
                Some("matches the API"),
            )],
            ..Default::default()
        }
    }

    #[test]
    fn findings_tab() {
        let (mut app, _) = app();
        app.findings = sample_findings();
        (app.finding, app.detail, app.tab) = (1, true, FINDINGS);
        let mut term = Terminal::new(TestBackend::new(60, 22)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Findings ▾                                               │",
                "│ Open                                                     │",
                "│   ✗ src/config.rs:41 -1 still parses                     │",
                "│     cargo test negative fails                            │",
                "│                                                          │",
                "│ Disputed                                                 │",
                "│ ▌ ◆ src/retry.rs:9 no test for zero                      │",
                "│     no test calls it with 0                              │",
                "│     worker: 0 is covered by the default                  │",
                "│                                                          │",
                "│ Optional, unproven                                       │",
                "│   ◆ src/lib.rs:3 may overflow                            │",
                "│                                                          │",
                "│ Waived                                                   │",
                "│   ○ src/config.rs:40 rename it                           │",
                "│     reason: matches the API                              │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   j/k  finding  ?  more",
            ]
        );
    }

    #[test]
    fn rewind_picks_a_worker_commit_and_retry_asks_for_a_model() {
        let (mut app, now) = app();
        let state = std::env::temp_dir().join(format!("yogan-rewind-{}", std::process::id()));
        let slot = state.join("slots/1");
        fs::create_dir_all(&slot).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&slot)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["commit", "-q", "--allow-empty", "-m", "feat: one"]);
        git(&["commit", "-q", "--allow-empty", "-m", "fix: two"]);
        app.state = state.clone();
        app.tasks[0].0.save(&state).unwrap();
        let press = |app: &mut App, code| app.key(KeyEvent::new(code, KeyModifiers::NONE));

        // w on the task in Review lists the worker's commits, newest first
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.reply_for, Some('w'), "{:?}", app.notice);
        let subjects: Vec<_> = app
            .commits
            .iter()
            .map(|c| c.split_once(' ').unwrap().1)
            .collect();
        assert_eq!(subjects, ["fix: two", "feat: one"]);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.commit, 1);
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        let rows = screen(&term);
        assert!(
            rows.iter().any(|r| r.contains("Rewind to a commit")),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("▌") && r.contains("feat: one")),
            "{rows:#?}"
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.reply_for, None);

        // r on a task in Review replies to its worker
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(app.reply_for, None);
        let hint = app.reply.as_ref().unwrap().placeholder_text().to_string();
        assert_eq!(hint, "What should the worker change?");
        press(&mut app, KeyCode::Esc);

        // c needs a session to continue
        press(&mut app, KeyCode::Char('c'));
        assert!(app.interactive.is_none());
        app.tasks[0].0.sessions = vec!["s-1".into()];
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.interactive, Some((slot.clone(), "s-1".into())));

        // t on the failed task asks for a model
        app.interactive = None;
        app.selected = 1;
        app.tasks[1].0.model = Some("claude-opus-5-5".into());
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.reply_for, Some('t'));
        let hint = app.reply.as_ref().unwrap().placeholder_text().to_string();
        assert_eq!(hint, "Model to retry on; empty keeps claude-opus-5-5");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn applying_an_edited_pr_draft() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-edit-draft-{}", std::process::id()));
        let slot = state.join("slots/1");
        fs::create_dir_all(&slot).unwrap();
        for args in [
            &["init", "-q"][..],
            &["commit", "-q", "--allow-empty", "-m", "base"],
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        ] {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&slot)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        // a checkout with no origin, so no project file applies
        (app.state, app.repo, app.preview) = (state.clone(), slot, true);
        app.tasks[0].0.pr_draft = Some(pr::Draft {
            title: "old".into(),
            body: "old body".into(),
            head: "abc".into(),
            problem: None,
        });
        app.tasks[0].0.save(&state).unwrap();

        let edit = app.edit_file().unwrap();
        assert!(edit.1, "the PR draft");
        assert_eq!(fs::read_to_string(&edit.2).unwrap(), "old\n\nold body\n");
        fs::write(&edit.2, "feat: reject — negative [CC-1]\n\nNew body.\n").unwrap();
        app.apply_edit(edit).unwrap();
        let saved = task::load_all(&state).unwrap().remove(0);
        let draft = saved.pr_draft.unwrap();
        assert_eq!(draft.title, "feat: reject, negative [CC-1]");
        assert_eq!(draft.body, "New body.");
        assert_eq!(draft.head, "abc");
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn waiving_a_finding() {
        let (mut app, _) = app();
        let state = std::env::temp_dir().join(format!("yogan-waive-{}", std::process::id()));
        sample_findings().save(&state, "t1").unwrap();
        app.state = state.clone();
        (app.detail, app.tab) = (true, FINDINGS);
        app.load_tab();
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.reply_for, Some('x'));
        app.reply.as_mut().unwrap().insert_str("0 is fine here");
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.notice.is_none(), "{:?}", app.notice);
        let saved = Findings::load(&state, "t1").unwrap();
        assert!(saved.disputed.is_empty());
        assert_eq!(saved.waived[1].claim, "no test for zero");
        assert_eq!(saved.waived[1].reply.as_deref(), Some("0 is fine here"));
        // the tab shows it too, so the cursor can't act on a stale row
        app.load_tab();
        assert_eq!(app.findings, saved);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn reaps_dead_leads() {
        let state = std::env::temp_dir().join(format!("yogan-reap-{}", std::process::id()));
        let mut child = Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let request = |id: &str, pid| Request {
            id: id.into(),
            pid,
            ..Default::default()
        };
        request("dead", Some(child.id())).save(&state).unwrap();
        request("starting", None).save(&state).unwrap();
        request("never-started", None).save(&state).unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        File::options()
            .write(true)
            .open(state.join("requests/never-started.toml"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        request("alive", Some(std::process::id()))
            .save(&state)
            .unwrap();
        reap(&state).unwrap();
        let status = |id: &str| lead::load(&state, id).unwrap();
        let dead = status("dead");
        assert_eq!(dead.status, Phase::Failed);
        assert_eq!(
            dead.summary.as_deref(),
            Some("the lead exited without finishing")
        );
        assert_eq!(status("starting").status, Phase::Planning);
        assert_eq!(status("never-started").status, Phase::Failed);
        assert_eq!(status("alive").status, Phase::Planning);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn pr_preview_with_title_problem() {
        let (mut app, _) = app();
        app.tasks[0].0.pr_draft = Some(pr::Draft {
            title: "feat(config): reject negative max_delay".into(),
            body: "max_delay below zero now fails at parse time.\n\n- adds a check".into(),
            head: "abc".into(),
            problem: Some("expected `type(scope): description [TICKET-1]`".into()),
        });
        app.preview = true;
        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ PR preview ──────────────────────────────────────────────╮",
                "│ feat(config): reject negative max_delay                  │",
                "│ ✗ expected `type(scope): description [TICKET-1]`         │",
                "│                                                          │",
                "│ max_delay below zero now fails at parse time.            │",
                "│                                                          │",
                "│ - adds a check                                           │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   enter  push and open ",
            ]
        );
    }

    #[test]
    fn activity_from_stream() {
        let log = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stream.jsonl");
        let calls = activity(&log, Path::new("/tmp/fixture"));
        assert_eq!(
            calls,
            [
                ("Read".into(), "note.txt".into()),
                ("Write".into(), "out.txt".into())
            ]
        );
        let log = std::env::temp_dir().join(format!("yogan-nudge-{}.jsonl", std::process::id()));
        fs::write(
            &log,
            "{\"type\":\"nudge\",\"reason\":\"stalled for 15m\"}\n\
             {\"type\":\"handoff\",\"reason\":\"context at 160000 of 200000 tokens\"}\n",
        )
        .unwrap();
        let calls = activity(&log, Path::new("/tmp"));
        assert_eq!(
            calls,
            [
                (NUDGE.into(), "stalled for 15m".into()),
                (HANDOFF.into(), "context at 160000 of 200000 tokens".into())
            ]
        );
        fs::remove_file(&log).unwrap();
    }

    #[test]
    fn main_screen() {
        let (app, now) = app();
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                                                                ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ Review                                    ││ Summary  Activity  Gate  Findings  Diff  Run        │",
                "│ ▌ ✓ Reject negative max_delay          4m ││                                                     │",
                "│                                           ││ Reject negative max_delay                           │",
                "│ Failed                                    ││ Review · u/reject-negative · slot 1 · opus/high     │",
                "│   ✗ Bump sqlx to 0.9                   1h ││                                                     │",
                "│                                           ││ max_delay below zero now fails at parse time.       │",
                "│ Running                                   ││                                                     │",
                "│   ⠋ Retry webhook sends     working · 12m ││                                                     │",
                "│                                           ││                                                     │",
                "│ Queued                                    ││                                                     │",
                "│   ○ Split the ledger reconciliation j… 2m ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " m  open PR  r  reply  d  diff  x  discard  c  continue  w  rewind  ?  more                         ",
            ]
        );
    }

    #[test]
    fn footer_fits_at_80_and_100_columns() {
        let footer = |app: &App, now, w| {
            let mut term = Terminal::new(TestBackend::new(w, 24)).unwrap();
            let theme = Theme::new(false, false);
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            screen(&term).pop().unwrap()
        };
        let (mut app, now) = app();
        let mut states = Vec::new();
        states.push(("review", footer(&app, now, 80), footer(&app, now, 100)));
        app.selected = 1;
        states.push(("failed", footer(&app, now, 80), footer(&app, now, 100)));
        app.tasks[1].0.status = Status::Proposed;
        states.push(("proposed", footer(&app, now, 80), footer(&app, now, 100)));
        app.requests = vec![Request {
            id: "r5".into(),
            text: "How are tasks saved?".into(),
            mode: Mode::Ask,
            status: Phase::Done,
            ..Default::default()
        }];
        app.selected = 0;
        states.push(("question", footer(&app, now, 80), footer(&app, now, 100)));
        // six keys at most, then ? more; at 80 columns the lowest-priority key gives way
        assert_eq!(
            states,
            [
                (
                    "review",
                    " m  open PR  r  reply  d  diff  x  discard  c  continue  w  rewind  ?  more     ".into(),
                    " m  open PR  r  reply  d  diff  x  discard  c  continue  w  rewind  ?  more                         ".into(),
                ),
                (
                    "failed",
                    " t  retry  c  continue  x  discard  n  new task  1-6  tabs  j/k  move  ?  more  ".into(),
                    " t  retry  c  continue  x  discard  n  new task  1-6  tabs  j/k  move  ?  more                      ".into(),
                ),
                (
                    "proposed",
                    " a  approve  A  approve all  e  edit  r  reply  x  discard  ?  more             ".into(),
                    " a  approve  A  approve all  e  edit  r  reply  x  discard  n  new task  ?  more                    ".into(),
                ),
                (
                    "question",
                    " p  plan it  r  reply  y  copy  x  discard  n  new task  j/k  move  ?  more     ".into(),
                    " p  plan it  r  reply  y  copy  x  discard  n  new task  j/k  move  ?  more                         ".into(),
                ),
            ]
        );
    }

    #[test]
    fn compact_layout_below_24_rows() {
        let (mut app, now) = app();
        app.tab = 1;
        app.activity = vec![
            ("Read".into(), "src/config.rs".into()),
            ("Bash".into(), "cargo test -p ledger".into()),
        ];
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ ▌ ✓ Reject negative max_delay          4m ││ Activity ▾                                          │",
                "│   ✗ Bump sqlx to 0.9                   1h ││ read    src/config.rs                               │",
                "│   ⠋ Retry webhook sends     working · 12m ││ run     cargo test -p ledger                        │",
                "│   ○ Split the ledger reconciliation j… 2m ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1   m  open PR  r  reply  d  diff  x  discard  ?  more           ",
            ]
        );

        // a click selects the row it lands on; the wheel over the detail pane scrolls it
        let at = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let failed = screen(&term)
            .iter()
            .position(|r| r.contains("Bump sqlx"))
            .unwrap();
        app.mouse(at(failed as u16));
        assert_eq!(app.task().unwrap().0.id, "t2");
        app.mouse(at(0)); // the border: nothing
        assert_eq!(app.task().unwrap().0.id, "t2");
    }

    #[test]
    fn a_click_is_a_key() {
        let (mut app, now) = app();
        let theme = Theme::new(false, false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // draws, then clicks the first cell of `text`
        let mut click = |app: &mut App, text: &str| {
            term.draw(|f| draw(f, app, &theme, 0, now)).unwrap();
            let rows = screen(&term);
            let hit = rows.iter().enumerate().find(|(_, r)| r.contains(text));
            let (y, row) = hit.unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"));
            let x = row[..row.find(text).unwrap()].chars().count();
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x as u16,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(click(&mut app, "Gate  Findings"));
        assert_eq!(app.tab, 2);

        // a footer key opens the confirm; its esc button keeps the task, its y button dismisses
        let state = std::env::temp_dir().join(format!("yogan-click-{}", std::process::id()));
        let failed = Request {
            id: "r2".into(),
            text: "Reject a negative max_delay".into(),
            status: Phase::Failed,
            ..Default::default()
        };
        failed.save(&state).unwrap();
        app.state = state.clone();
        app.reload(&state).unwrap();
        app.selected = 0;
        assert!(click(&mut app, " x  discard") && app.confirm);
        assert!(click(&mut app, " esc  Keep it") && !app.confirm);
        assert!(click(&mut app, " x  discard") && app.confirm);
        assert!(click(&mut app, " y  Dismiss") && !app.confirm && app.notice.is_none());
        assert_eq!(lead::load(&state, "r2").unwrap().status, Phase::Dismissed);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn narrow_ascii_one_pane() {
        let (mut app, now) = app();
        app.selected = 2;
        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let theme = Theme::new(false, true);
        term.draw(|f| draw(f, &app, &theme, 1, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                "╭ Tasks ───────────────────────────────────────────────────╮",
                "│   + Reject negative max_delay                         4m │",
                "│   x Bump sqlx to 0.9                                  1h │",
                "│ > / Retry webhook sends                    working · 12m │",
                "│   o Split the ledger reconciliation job into per-acc~ 2m │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " yogan · fuse-os  + 1  x 1  | 1  o 1   x  discard  ?  more  ",
            ]
        );
    }
}
