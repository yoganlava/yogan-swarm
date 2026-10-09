//! The TUI: tasks grouped by status on the left, the selected task on the right. It only reads
//! the state directory; workers run detached, so closing it changes nothing.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use regex::Regex;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
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

const TABS: [&str; 5] = ["Summary", "Activity", "Gate", "Findings", "Diff"];
const FINDINGS: usize = 3;

const KEYS: [(&str, &str); 16] = [
    ("n", "new task"),
    ("j/k", "move"),
    ("tab", "pane"),
    ("1-5", "tabs"),
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
];

/// Every color and glyph, so a light-terminal or ASCII variant is one swap. Nothing paints a
/// background: the terminal's own theme shows through.
pub struct Theme {
    accent: Color,
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
    /// Truecolor when `COLORTERM` says so, ASCII glyphs under `YOGAN_ASCII=1`.
    pub fn detect() -> Theme {
        let env = |k: &str, vals: &[&str]| std::env::var(k).is_ok_and(|v| vals.contains(&&*v));
        Theme::new(
            env("COLORTERM", &["truecolor", "24bit"]),
            env("YOGAN_ASCII", &["1"]),
        )
    }

    fn new(truecolor: bool, ascii: bool) -> Theme {
        let rgb = |r, g, b, ansi| if truecolor { Color::Rgb(r, g, b) } else { ansi };
        let glyphs = |fancy, plain| if ascii { plain } else { fancy };
        Theme {
            accent: rgb(122, 162, 247, Color::Blue),
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
    /// A slot and its base, whose full diff to page once the TUI is suspended.
    pager: Option<(PathBuf, String)>,
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
    /// `x` waives it.
    on_finding: Option<char>,
    /// The selected task's findings, and the cursor over their actionable ones.
    findings: Findings,
    finding: usize,
    /// Tasks in Review with a disputed finding.
    disputed: Vec<String>,
    /// The selected question's id, answer and the repo files the answer cites.
    answer: Option<(String, String, Vec<String>)>,
}

/// The `n` screen: a request for the lead, and an optional ticket.
struct Compose {
    request: TextArea<'static>,
    ticket: TextArea<'static>,
    mode: Mode,
    /// The focused field: request, ticket, then mode.
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
        on_finding: None,
        findings: Findings::default(),
        finding: 0,
        disputed: Vec::new(),
        sessions: config::load(repo).map_or(0, |c| c.watch.max_handoffs + 1),
    };
    let theme = Theme::detect();
    let mut terminal = ratatui::init();
    let start = Instant::now();
    let res = (|| -> Result<()> {
        loop {
            // checked before the reload, so an exited worker's last save is already loaded
            let exited = app.awaiting.as_ref().is_some_and(|a| !alive(a.2));
            app.reload(&state)?;
            app.load_tab();
            app.check_awaiting(exited);
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
            if event::poll(wait)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !app.key(key)
            {
                return Ok(());
            }
            if let Some((dir, base)) = app.pager.take() {
                ratatui::restore();
                let diff = format!("{base}...HEAD");
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["diff", &diff])
                    .status()?;
                terminal = ratatui::init();
            }
            if std::mem::take(&mut app.edit) {
                ratatui::restore();
                let edited = if app.preview {
                    app.edit_draft()
                } else {
                    app.edit_task()
                };
                terminal = ratatui::init();
                if let Err(e) = edited {
                    app.notice = Some(format!("{e:#}"));
                }
            }
        }
    })();
    ratatui::restore();
    res
}

fn alive(pid: u32) -> bool {
    Pid::from_raw(pid as i32).is_some_and(|p| test_kill_process(p) != Err(Errno::SRCH))
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

/// Opens `file` in `$EDITOR` (default `vi`) and waits for it to exit.
fn editor(file: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let edited = Command::new("sh")
        .args(["-c", &format!("{editor} \"$1\""), "sh"])
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
        if running(&t) && !t.pid.is_some_and(alive) {
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
        if r.status == Phase::Planning && (r.pid.is_some_and(|p| !alive(p)) || never_started(&r)) {
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
        Ok(())
    }

    /// Whether `j/k`, `r` and `x` act on the Findings tab's findings.
    fn on_findings(&self) -> bool {
        self.detail && self.tab == FINDINGS && self.task().is_some()
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
            match key.code {
                KeyCode::Esc => self.compose = None,
                KeyCode::Tab => c.focus = (c.focus + 1) % 3,
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
                _ if c.focus == 2 => {}
                KeyCode::Enter if c.focus == 1 => {}
                _ if c.focus == 1 => _ = c.ticket.input(key),
                _ => _ = c.request.input(key),
            }
            return true;
        }
        if let Some(input) = &mut self.reply {
            match key.code {
                KeyCode::Esc => (self.reply, self.on_finding) = (None, None),
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
                    self.on_finding = Some('x');
                    return true;
                }
                KeyCode::Char('r') if disputed => {
                    self.reply = Some(field("Why the critic is right, for the worker"));
                    self.on_finding = Some('r');
                    return true;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('?') => self.help = true,
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
            KeyCode::Char(c @ '1'..='5') => self.tab = c as usize - '1' as usize,
            KeyCode::Char('d') => match self.slot_dir() {
                Some(dir) => self.pager = Some((dir, self.base().unwrap_or_default())),
                None => self.notice = Some("this task has no worktree".into()),
            },
            KeyCode::Char('x') => self.confirm = self.task().is_some() || failed || answered,
            KeyCode::Char('t') if failed => {
                if let Err(e) = self.retry() {
                    self.notice = Some(format!("{e:#}"));
                }
            }
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
            KeyCode::Down | KeyCode::Char('j') => {
                let rows = self.requests.len() + self.tasks.len();
                self.selected = (self.selected + 1).min(rows.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Tab => self.detail = !self.detail,
            _ => {}
        }
        true
    }
}

impl App {
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
        let key = Some((id.clone(), since));
        if self.tab >= 2 && self.loaded != key {
            let log = self.state.join(format!("logs/{id}.gate.log"));
            self.gate_log = fs::read_to_string(log).unwrap_or_default();
            let base = self.base().unwrap_or_default();
            self.diff = self
                .slot_dir()
                .map(|d| diffstat(&d, &base))
                .unwrap_or_default();
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

    /// Runs `[scripts] teardown` in the task's slot, then saves it with the slot freed.
    fn free_slot(&mut self, mut t: Task, done: &str) -> Result<()> {
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

    /// `e` in the preview: the draft as `title`, blank line, body in `$EDITOR`.
    fn edit_draft(&mut self) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
        let mut t = t.clone();
        let dir = self.slot_dir().context("this task has no worktree")?;
        let draft = t.pr_draft.as_mut().context("no PR draft")?;
        let file = self.state.join(format!("logs/{}.pr.md", t.id));
        fs::write(&file, format!("{}\n\n{}\n", draft.title, draft.body))?;
        editor(&file)?;
        let text = fs::read_to_string(&file)?;
        let (title, body) = text.trim().split_once('\n').unwrap_or((text.trim(), ""));
        (draft.title, draft.body) = (pr::clean(title.trim()), pr::clean(body.trim()));
        let cfg = config::load(&self.repo)?;
        let migration = pr::migration(&dir, &self.base()?)?;
        draft.problem = pr::check_title(&draft.title, &cfg.pr.types, migration).err();
        t.save(&self.state)
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

    /// `e` on a proposal: its editable fields as TOML in `$EDITOR`, checked like a new proposal.
    fn edit_task(&mut self) -> Result<()> {
        let (t, _) = self.task().context("no task selected")?;
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
        fs::create_dir_all(self.state.join("logs"))?;
        let header = format!("# Editable: {}\n", EDITABLE.join(", "));
        fs::write(&file, format!("{header}{}", toml::to_string(&shown)?))?;
        editor(&file)?;
        let t = edited(t, &fs::read_to_string(&file)?)?;
        task::check_proposal(&t, &task::load_all(&self.state)?)?;
        t.save(&self.state)
    }

    /// Sends `text` to the selected proposal's lead, which refiles its revised plan.
    fn send_reply(&mut self, text: &str) -> Result<()> {
        ensure!(!text.trim().is_empty(), "write a reply first");
        if let Some(act) = self.on_finding.take() {
            return self.act_on_finding(act, text.trim());
        }
        if let Some(r) = self.request() {
            let id = r.id.clone();
            lead::follow_up(&self.repo, &self.state, &id, text.trim())?;
            lead::spawn(&self.repo, &self.state, &id, Some(text.trim()))?;
            self.info = Some("the lead is answering the follow-up".into());
            return Ok(());
        }
        let (t, _) = self.task().context("no task selected")?;
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
    let Ok(mut file) = File::open(log) else {
        return Vec::new();
    };
    // ponytail: only the last 256 KiB, so redrawing a long session stays cheap
    let len = file.metadata().map_or(0, |m| m.len());
    let mut bytes = Vec::new();
    if file
        .seek(SeekFrom::Start(len.saturating_sub(256 << 10)))
        .is_err()
        || file.read_to_end(&mut bytes).is_err()
    {
        return Vec::new();
    }
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

fn draw(f: &mut Frame, app: &App, theme: &Theme, tick: usize, now: SystemTime) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
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
    let keys: Vec<(&str, &str)> = if app.compose.is_some() {
        vec![("tab", "field"), ("ctrl-s", "submit"), ("esc", "cancel")]
    } else if app.reply.is_some() {
        vec![("ctrl-s", "send"), ("esc", "cancel")]
    } else if app.on_findings() {
        let mut keys = vec![("j/k", "finding")];
        keys.extend((app.findings.is_disputed(app.finding)).then_some(("r", "uphold")));
        keys.extend((app.findings.actionable().count() > 0).then_some(("x", "waive")));
        keys.extend([("tab", "pane"), ("1-5", "tabs"), ("?", "help")]);
        keys
    } else if app.instruction.is_some() {
        vec![("enter", "redraft"), ("esc", "cancel")]
    } else if app.preview {
        let push = ("enter", "push and open PR");
        vec![push, ("g", "regenerate"), ("e", "edit"), ("esc", "back")]
    } else {
        // only the keys that do something for the selected task
        KEYS.into_iter()
            .filter(|(k, _)| match *k {
                "d" => selected.is_some_and(|t| t.slot.is_some()),
                "x" => selected.is_some() || failed || answered,
                "t" => failed,
                "p" | "y" => answered,
                "1-5" => selected.is_some(),
                "m" => selected.is_some_and(|t| t.status == Status::Review && gate_passed(t)),
                "r" => answered || selected.is_some_and(|t| t.status == Status::Proposed),
                "a" | "e" => selected.is_some_and(|t| t.status == Status::Proposed),
                "A" => app.tasks.iter().any(|(t, _)| t.status == Status::Proposed),
                _ => true,
            })
            .collect()
    };
    let keys = keys.iter().flat_map(|(key, label)| {
        [
            Span::styled(format!(" {key} "), theme.accent),
            Span::raw(format!("{label} ")).dim(),
        ]
    });
    let line = match (&app.notice, &app.info) {
        (Some(notice), _) => Line::styled(format!(" {notice}"), theme.red),
        (None, Some(info)) => Line::styled(format!(" {info}"), theme.accent),
        (None, None) => Line::from(keys.collect::<Vec<_>>()),
    };
    f.render_widget(line, footer);
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
        let area = centered(f.area(), 52, 5);
        let text = vec![
            Line::raw(format!("{verb} “{title}”?")),
            Line::from(vec![
                Span::styled("y ", theme.accent),
                Span::raw(format!("{does}   ")).dim(),
                Span::styled("any key ", theme.accent),
                Span::raw("cancel").dim(),
            ]),
        ];
        f.render_widget(Clear, area);
        let text = Paragraph::new(text).wrap(Wrap { trim: true });
        f.render_widget(text.block(pane(verb, true, theme)), area);
    }
    if let Some(input) = &app.reply {
        let all = f.area();
        f.buffer_mut().set_style(all, Style::new().dim());
        let area = centered(all, 64, 8);
        let title = match app.on_finding {
            Some('x') => "Waive the finding",
            Some(_) => "Uphold the finding",
            None => "Reply to the lead",
        };
        let block = pane(title, true, theme);
        f.render_widget(Clear, area);
        f.render_widget(input, block.inner(area));
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

fn compose(f: &mut Frame, area: Rect, c: &Compose, theme: &Theme) {
    let [mode, request, ticket] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .areas(area);
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
    let width = block.inner(area).width as usize;
    let (mut items, mut selected, mut i) = (Vec::new(), None, 0);
    let mut group = None;
    for r in &app.requests {
        let question = r.status == Phase::Done;
        if group != Some(question) {
            if !items.is_empty() {
                items.push(ListItem::new(""));
            }
            let label = if question { "Questions" } else { "Planning" };
            items.push(ListItem::new(Line::raw(label).dim()));
            group = Some(question);
        }
        let sel = i == app.selected;
        if sel {
            selected = Some(items.len());
        }
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
        if !items.is_empty() {
            items.push(ListItem::new(""));
        }
        items.push(ListItem::new(Line::raw(label).dim()));
        for (t, since) in group {
            let sel = i == app.selected;
            if sel {
                selected = Some(items.len());
            }
            let age = since.and_then(|s| now.duration_since(s).ok());
            let age = age.map(short).unwrap_or_default();
            // ponytail: the step is the status; T16's Activity knows the real one (testing, editing)
            let right = match t.status {
                Status::Running => format!("working · {age}"),
                Status::Checking => format!("gate · {age}"),
                _ if app.disputed.contains(&t.id) => format!("disputed · {age}"),
                _ => age,
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
    let block = pane("Task", focused, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some((t, _)) = app.task() else {
        match app.request() {
            Some(r) => request(f, inner, r, app.answer.as_ref(), theme),
            None => f.render_widget(Line::raw("No tasks yet.").dim(), inner),
        }
        return;
    };
    let [tabs, _, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
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
    f.render_widget(Line::from(tabs_line.collect::<Vec<_>>()), tabs);
    match app.tab {
        0 => {
            let parent = t.parent.as_ref().map(|p| {
                let shown = app.tasks.iter().find(|(o, _)| &o.id == p);
                shown.map_or(p.clone(), |(o, _)| o.title.clone())
            });
            summary(f, body, t, parent, app.sessions)
        }
        1 => activity_tab(f, body, &app.activity, theme),
        2 => gate_tab(f, body, t, &app.gate_log, theme),
        FINDINGS => {
            let cursor = app.on_findings().then_some(app.finding);
            findings_tab(f, body, &app.findings, cursor, theme)
        }
        _ => diff_tab(f, body, &app.diff, theme),
    }
}

/// `parent` is the parent task's title; `sessions` the most a task gets, shown once it hands off.
fn summary(f: &mut Frame, area: Rect, t: &Task, parent: Option<String>, sessions: u32) {
    let label = GROUPS
        .iter()
        .find(|(s, _)| *s == t.status)
        .map_or("", |g| g.1);
    let mut meta = vec![label.to_string()];
    meta.extend((!t.branch.is_empty()).then(|| t.branch.clone()));
    meta.extend(t.slot.map(|n| format!("slot {n}")));
    let n = t.sessions.len();
    meta.extend((n > 1).then(|| format!("session {n}/{sessions}")));
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
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// A request: its first line, status and ticket, why its lead failed, then the full request;
/// for a question, the answer and the files it cites instead.
fn request(
    f: &mut Frame,
    area: Rect,
    r: &Request,
    answer: Option<&(String, String, Vec<String>)>,
    theme: &Theme,
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
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
fn findings_tab(f: &mut Frame, area: Rect, fs: &Findings, cursor: Option<usize>, theme: &Theme) {
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
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One line per tool call, following the tail: edits in the accent, shell commands in a
/// second hue, reads dim, nudges amber.
fn activity_tab(f: &mut Frame, area: Rect, calls: &[(String, String)], theme: &Theme) {
    if calls.is_empty() {
        f.render_widget(Line::raw("No tool calls yet.").dim(), area);
        return;
    }
    let width = area.width as usize;
    let shown = &calls[calls.len().saturating_sub(area.height as usize)..];
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
fn gate_tab(f: &mut Frame, area: Rect, t: &Task, log: &str, theme: &Theme) {
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
    let skip = lines.len().saturating_sub(output.height as usize);
    f.render_widget(Paragraph::new(lines.split_off(skip)), output);
}

/// git-style `+`/`-` counts with bars scaled to the largest change.
fn diff_tab(f: &mut Frame, area: Rect, files: &[(String, u64, u64)], theme: &Theme) {
    if files.is_empty() {
        f.render_widget(Line::raw("No changes yet.").dim(), area);
        return;
    }
    let (added, deleted) = files.iter().fold((0, 0), |(a, d), f| (a + f.1, d + f.2));
    let max = files.iter().map(|f| f.1 + f.2).max().unwrap_or(1).max(1);
    let bar_width = 20u64.min(max);
    let path_width = (area.width as usize).saturating_sub(13 + bar_width as usize);
    let scale = |n: u64| ((n * bar_width).div_ceil(max)) as usize;
    let mut lines: Vec<Line> = files
        .iter()
        .map(|(path, a, d)| {
            let path = truncate(path, path_width, theme.ellipsis);
            Line::from(vec![
                Span::raw(format!("{path:<path_width$} ")),
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
    f.render_widget(Paragraph::new(lines), area);
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
            on_finding: None,
            findings: Findings::default(),
            finding: 0,
            disputed: Vec::new(),
            sessions: 3,
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Findings  Diff                  │",
                "│                                                          │",
                "│ Reject negative max_delay                                │",
                "│ Review · u/reject-negative · slot 1 · session 2/3 ·      │",
                "│ opus/high · CC-687                                       │",
                "│                                                          │",
                "│ max_delay below zero now fails at parse time.            │",
                "│                                                          │",
                "│ A negative value panics in the retry loop.               │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  d diff  m open PR",
            ]
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Findings  Diff                  │",
                "│                                                          │",
                "│ read    src/old.rs                                       │",
                "│ read    src/config.rs                                    │",
                "│ search  max_delay                                        │",
                "│ edit    src/config.rs                                    │",
                "│ run     cargo test -p ledger                             │",
                "│ write   crates/ledger/src/limits/negative_delay_regress… │",
                "│ TodoWrite                                                │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  d diff  m open PR",
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Findings  Diff                  │",
                "│                                                          │",
                "│ ✓  clean tree                                            │",
                "│ ✓  fmt                                                   │",
                "│ ✗  test                                                  │",
                "│                                                          │",
                "│ test                                                     │",
                "│ $ cargo test -p ledger                                   │",
                "│ thread 'parse' panicked at src/config.rs:40:9:           │",
                "│ assertion failed: delay >= 0                             │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  d diff  x discard",
            ]
        );
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Findings  Diff                  │",
                "│                                                          │",
                "│ src/config.rs             +12    -3 ++++++--             │",
                "│ crates/ledger/src/limi…   +40    -0 ++++++++++++++++++++ │",
                "│ assets/logo.png            +0    -0                      │",
                "│                                                          │",
                "│ 3 files changed, +52 -3                                  │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  d diff  m open PR",
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
                " yogan · fuse-os  ✓ 1  ○ 3                                                                          ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ Review                                    ││ Summary  Activity  Gate  Findings  Diff             │",
                "│   ✓ Reject negative max_delay             ││                                                     │",
                "│                                           ││ Reject negative max_delay in the CLI                │",
                "│ Proposed                                  ││ Proposed · CC-687 · cli, config · after “Validate   │",
                "│   ○ Validate max_delay at parse time      ││ max_delay at parse time”                            │",
                "│ ▌ ○ └ Reject negative max_delay in the …  ││                                                     │",
                "│   ○ Bump sqlx to 0.9                      ││ Reuse the config check in the CLI.                  │",
                "│                                           ││                                                     │",
                "│                                           ││ Done when                                           │",
                "│                                           ││ - `yogan --max-delay -1` exits 2                    │",
                "│                                           ││ - the error names the flag                          │",
                "│                                           ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  a approve  A approve all  e edit  r reply  x discard  ? h",
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                                                                ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ Planning                                  ││ Reject a negative max_delay                         │",
                "│   ⠋ Split the ledger job into p… planning ││ Failed · CC-687                                     │",
                "│ ▌ ✗ Reject a negative max_delay    failed ││                                                     │",
                "│                                           ││ claude exited with exit status: 1                   │",
                "│ Review                                    ││                                                     │",
                "│   ✓ Reject negative max_delay          4m ││ Reject a negative max_delay                         │",
                "│                                           ││ It panics in the retry loop.                        │",
                "│ Failed                                    ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  t retry  x discard  ? help  q quit                                 ",
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                                                                ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ Questions                                 ││ How are tasks saved?                                │",
                "│ ▌ ✓ How are tasks saved?         answered ││ Question                                            │",
                "│                                           ││                                                     │",
                "│ Review                                    ││ Atomically                                          │",
                "│   ✓ Reject negative max_delay          4m ││ A tmp file, then a rename:                          │",
                "│                                           ││   fs::rename(&tmp, &path)                           │",
                "│ Failed                                    ││                                                     │",
                "│   ✗ Bump sqlx to 0.9                   1h ││ Cites                                               │",
                "│                                           ││ - src/task.rs:72                                    │",
                "│ Running                                   ││                                                     │",
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  r reply  p plan it  y copy  x discard  ? help  q quit              ",
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Findings  Diff                  │",
                "│                                                          │",
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
                "╰──────────────────────────────────────────────────────────╯",
                " j/k finding  r uphold  x waive  tab pane  1-5 tabs  ? help ",
            ]
        );
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
        assert_eq!(app.on_finding, Some('x'));
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
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ PR preview ──────────────────────────────────────────────╮",
                "│ feat(config): reject negative max_delay                  │",
                "│ ✗ expected `type(scope): description [TICKET-1]`         │",
                "│                                                          │",
                "│ max_delay below zero now fails at parse time.            │",
                "│                                                          │",
                "│ - adds a check                                           │",
                "╰──────────────────────────────────────────────────────────╯",
                " enter push and open PR  g regenerate  e edit  esc back     ",
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
        let mut term = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let theme = Theme::new(false, false);
        term.draw(|f| draw(f, &app, &theme, 0, now)).unwrap();
        assert_eq!(
            screen(&term),
            [
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                                                                ",
                "╭ Tasks ────────────────────────────────────╮╭ Task ───────────────────────────────────────────────╮",
                "│ Review                                    ││ Summary  Activity  Gate  Findings  Diff             │",
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
                "╰───────────────────────────────────────────╯╰─────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  d diff  m open PR  x discard  ? help  q quit             ",
            ]
        );
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
                " yogan · fuse-os  + 1  x 1  | 1  o 1                        ",
                "╭ Tasks ───────────────────────────────────────────────────╮",
                "│   x Bump sqlx to 0.9                                  1h │",
                "│                                                          │",
                "│ Running                                                  │",
                "│ > / Retry webhook sends                    working · 12m │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-5 tabs  x discard  ? help",
            ]
        );
    }
}
