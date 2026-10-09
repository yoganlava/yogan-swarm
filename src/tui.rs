//! The TUI: tasks grouped by status on the left, the selected task on the right. It only reads
//! the state directory; workers run detached, so closing it changes nothing.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
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
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::stream::{self, Content};
use crate::task::{self, Status, Task};
use crate::{config, git, sched, slot, worker};

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

const TABS: [&str; 4] = ["Summary", "Activity", "Gate", "Diff"];

const KEYS: [(&str, &str); 8] = [
    ("n", "new task"),
    ("j/k", "move"),
    ("tab", "pane"),
    ("1-4", "tabs"),
    ("d", "diff"),
    ("x", "discard"),
    ("?", "help"),
    ("q", "quit"),
];

// ponytail: Diff and d compare with origin/main, like the worker; T23 adds parent branches
const BASE: &str = "origin/main";

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
    /// Shown tasks in list order, each with when its file last changed.
    tasks: Vec<(Task, Option<SystemTime>)>,
    selected: usize,
    /// Narrow layout shows the detail pane instead of the list.
    detail: bool,
    help: bool,
    compose: Option<Compose>,
    /// An error to show in the footer until the next key.
    notice: Option<String>,
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
    /// A slot whose full diff to page once the TUI is suspended.
    pager: Option<PathBuf>,
}

/// The `n` screen: a request whose first line is the title, and an optional ticket.
struct Compose {
    request: TextArea<'static>,
    ticket: TextArea<'static>,
    on_ticket: bool,
}

impl Compose {
    fn new() -> Compose {
        let field = |placeholder: &str| {
            let mut t = TextArea::default();
            t.set_cursor_line_style(Style::new());
            t.set_placeholder_text(placeholder);
            t
        };
        Compose {
            request: field("What should a worker do? The first line is the title."),
            ticket: field("e.g. CC-687"),
            on_ticket: false,
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
        tasks: Vec::new(),
        selected: 0,
        detail: false,
        help: false,
        compose: None,
        notice: None,
        tab: 0,
        activity: Vec::new(),
        gate_log: String::new(),
        diff: Vec::new(),
        loaded: None,
        confirm: false,
        pager: None,
    };
    let theme = Theme::detect();
    let mut terminal = ratatui::init();
    let start = Instant::now();
    let res = (|| -> Result<()> {
        loop {
            app.reload(&state)?;
            app.load_tab();
            let tick = (start.elapsed().as_millis() / 125) as usize; // spinner at 8 Hz
            // ratatui only writes cells that changed, so an idle screen draws nothing
            terminal.draw(|f| draw(f, &app, &theme, tick, SystemTime::now()))?;
            let running = app.tasks.iter().any(|(t, _)| running(t));
            let wait = Duration::from_millis(if running { 125 } else { 250 });
            if event::poll(wait)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !app.key(key)
            {
                return Ok(());
            }
            if let Some(dir) = app.pager.take() {
                ratatui::restore();
                let diff = format!("{BASE}...HEAD");
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["diff", &diff])
                    .status()?;
                terminal = ratatui::init();
            }
        }
    })();
    ratatui::restore();
    res
}

fn running(t: &Task) -> bool {
    matches!(t.status, Status::Running | Status::Checking)
}

/// A worker that died without recording an outcome (a crash, a reboot) fails its task.
fn reap(state: &Path) -> Result<()> {
    let alive = |pid: u32| {
        Pid::from_raw(pid as i32).is_some_and(|p| test_kill_process(p) != Err(Errno::SRCH))
    };
    for mut t in task::load_all(state)? {
        if running(&t) && !t.pid.is_some_and(alive) {
            t.status = Status::Failed;
            t.summary = Some("the worker exited without finishing".into());
            t.save(state)?;
        }
    }
    Ok(())
}

impl App {
    /// Re-reads the task files, keeping the selection on the same task.
    fn reload(&mut self, state: &Path) -> Result<()> {
        let id = self.tasks.get(self.selected).map(|(t, _)| t.id.clone());
        let rank = |s: Status| GROUPS.iter().position(|(g, _)| *g == s);
        let mut tasks: Vec<_> = task::load_all(state)?
            .into_iter()
            .filter(|t| rank(t.status).is_some())
            .map(|t| {
                let file = state.join(format!("tasks/{}.toml", t.id));
                let since = fs::metadata(file).and_then(|m| m.modified()).ok();
                (t, since)
            })
            .collect();
        tasks.sort_by_key(|(t, _)| (rank(t.status), t.id.clone()));
        self.tasks = tasks;
        let same = id.and_then(|id| self.tasks.iter().position(|(t, _)| t.id == id));
        self.selected = same.unwrap_or(self.selected.min(self.tasks.len().saturating_sub(1)));
        Ok(())
    }

    /// Returns false to quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl =
            |c| key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c);
        self.notice = None;
        if ctrl('c') {
            return false;
        }
        if let Some(c) = &mut self.compose {
            match key.code {
                KeyCode::Esc => self.compose = None,
                KeyCode::Tab => c.on_ticket = !c.on_ticket,
                _ if ctrl('s') => {
                    if let Err(e) = self.submit() {
                        self.notice = Some(format!("{e:#}"));
                    }
                }
                KeyCode::Enter if c.on_ticket => {}
                _ if c.on_ticket => _ = c.ticket.input(key),
                _ => _ = c.request.input(key),
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
        match key.code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('n') => self.compose = Some(Compose::new()),
            KeyCode::Char(c @ '1'..='4') => self.tab = c as usize - '1' as usize,
            KeyCode::Char('d') => match self.slot_dir() {
                Some(dir) => self.pager = Some(dir),
                None => self.notice = Some("this task has no worktree".into()),
            },
            KeyCode::Char('x') => self.confirm = !self.tasks.is_empty(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.tasks.len().saturating_sub(1));
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
        let (t, _) = self.tasks.get(self.selected)?;
        Some(self.state.join("slots").join(t.slot?.to_string()))
    }

    /// Reads what the current tab shows for the selected task. The gate log and diff are
    /// re-read only when the task's file changes; the activity tail every time.
    fn load_tab(&mut self) {
        let Some((t, since)) = self.tasks.get(self.selected) else {
            return;
        };
        if self.tab == 1 {
            let log = self.state.join(format!("logs/{}.jsonl", t.id));
            self.activity = activity(&log, &self.slot_dir().unwrap_or_default());
        }
        let key = Some((t.id.clone(), *since));
        if self.tab >= 2 && self.loaded != key {
            let log = self.state.join(format!("logs/{}.gate.log", t.id));
            self.gate_log = fs::read_to_string(log).unwrap_or_default();
            self.diff = self.slot_dir().map(|d| diffstat(&d)).unwrap_or_default();
            self.loaded = key;
        }
    }

    /// Stops the task's worker, runs `[scripts] teardown` in its slot and frees the slot.
    fn discard(&mut self) -> Result<()> {
        let Some((t, _)) = self.tasks.get(self.selected) else {
            return Ok(());
        };
        let mut t = t.clone();
        if running(&t)
            && let Some(pid) = t.pid
        {
            let _ = worker::stop(pid); // it may already have exited
        }
        let mut teardown = Ok(());
        if let (Some(n), Some(dir)) = (t.slot, self.slot_dir()) {
            let cfg = config::load(&self.repo)?;
            if let Some(cmd) = cfg.scripts.as_ref().and_then(|s| s.teardown.as_deref()) {
                let env = slot::env(&self.repo, &dir, n, &t, cfg.ports.as_ref());
                let log = self.state.join(format!("logs/{}.teardown.log", t.id));
                teardown = slot::run_script(cmd, &dir, &env, &log);
            }
        }
        t.status = Status::Discarded;
        t.slot = None;
        t.save(&self.state)?;
        sched::run(&self.repo)?;
        if let Err(e) = teardown {
            bail!("discarded, but teardown failed: {e:#}");
        }
        Ok(())
    }

    /// Files the composed request as an approved task and starts whatever is ready.
    fn submit(&mut self) -> Result<()> {
        let c = self.compose.as_ref().context("not composing")?;
        let request = c.request.lines().join("\n");
        let ticket = c.ticket.lines().join("").trim().to_string();
        let cfg = config::load(&self.repo)?;
        let tasks = task::load_all(&self.state)?;
        let task = new_task(&request, &ticket, &cfg.branch_prefix, &tasks, now_id())?;
        task.save(&self.state)?;
        self.compose = None;
        sched::run(&self.repo)
    }
}

/// `t<unix seconds>`, so ids sort by creation.
fn now_id() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("t{secs}")
}

/// An approved task from a typed request: the first line is the title, the rest the body.
fn new_task(request: &str, ticket: &str, prefix: &str, tasks: &[Task], id: String) -> Result<Task> {
    let request = request.trim();
    let (title, body) = request.split_once('\n').unwrap_or((request, ""));
    ensure!(!title.trim().is_empty(), "write a request first");
    let taken = |s: &str| tasks.iter().any(|t| t.id == s);
    let mut id = id;
    while taken(&id) {
        id.push('a'); // two submits in one second
    }
    let slug = slug(title);
    let branch_taken = |b: &str| tasks.iter().any(|t| t.branch == b);
    let mut branch = format!("{prefix}{slug}");
    for n in 2.. {
        if !branch_taken(&branch) {
            break;
        }
        branch = format!("{prefix}{slug}-{n}");
    }
    Ok(Task {
        id,
        title: title.trim().into(),
        body: body.trim().into(),
        ticket: (!ticket.is_empty()).then(|| ticket.into()),
        status: Status::Approved,
        branch,
        ..Default::default()
    })
}

/// `Reject negative max_delay!` → `reject-negative-max-delay`, at most 40 chars.
fn slug(title: &str) -> String {
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let slug = words.join("-"); // ASCII only, so any byte cut is a char boundary
    let slug = slug[..slug.len().min(40)].trim_end_matches('-');
    if slug.is_empty() {
        "task".into()
    } else {
        slug.into()
    }
}

/// Tool calls from the tail of a Claude stream log, as (tool, target), with `slot/` paths
/// made relative.
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
    let content = events.flat_map(|e| match e {
        stream::Event::Assistant { message } => message.content,
        _ => Vec::new(),
    });
    content
        .filter_map(|c| match c {
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
        })
        .collect()
}

/// `git diff --numstat` against the base, as (path, added, deleted); binaries count 0.
fn diffstat(slot: &Path) -> Vec<(String, u64, u64)> {
    let out = git(slot, &["diff", "--numstat", &format!("{BASE}...HEAD")]).unwrap_or_default();
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
    if let Some(c) = &app.compose {
        compose(f, body, c, theme);
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
    let selected = app.tasks.get(app.selected).map(|(t, _)| t);
    let keys: Vec<(&str, &str)> = if app.compose.is_some() {
        vec![("tab", "field"), ("ctrl-s", "submit"), ("esc", "cancel")]
    } else {
        // only the keys that do something for the selected task
        KEYS.into_iter()
            .filter(|(k, _)| match *k {
                "d" => selected.is_some_and(|t| t.slot.is_some()),
                "x" | "1-4" => selected.is_some(),
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
    let line = match &app.notice {
        Some(notice) => Line::styled(format!(" {notice}"), theme.red),
        None => Line::from(keys.collect::<Vec<_>>()),
    };
    f.render_widget(line, footer);
    if app.help {
        help(f, theme);
    }
    if app.confirm
        && let Some(t) = selected
    {
        // a modal over the dimmed screen
        let all = f.area();
        f.buffer_mut().set_style(all, Style::new().dim());
        let area = centered(f.area(), 52, 5);
        let text = vec![
            Line::raw(format!("Discard “{}”?", t.title)),
            Line::from(vec![
                Span::styled("y ", theme.accent),
                Span::raw("discard and free its slot   ").dim(),
                Span::styled("any key ", theme.accent),
                Span::raw("cancel").dim(),
            ]),
        ];
        f.render_widget(Clear, area);
        let text = Paragraph::new(text).wrap(Wrap { trim: true });
        f.render_widget(text.block(pane("Discard", true, theme)), area);
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
    let [request, ticket] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).areas(area);
    for (field, area, title, focused) in [
        (&c.request, request, "New task", !c.on_ticket),
        (&c.ticket, ticket, "Ticket", c.on_ticket),
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
            items.push(ListItem::new(row(t, age, sel, width, theme, tick)));
            i += 1;
        }
    }
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(List::new(items).block(block), area, &mut state);
}

/// `▌ ⠋ title…          working · 4m`: never wraps, the title gives way.
fn row(
    t: &Task,
    age: Option<Duration>,
    sel: bool,
    width: usize,
    theme: &Theme,
    tick: usize,
) -> Line<'static> {
    let age = age.map(short).unwrap_or_default();
    // ponytail: the step is the status; T16's Activity knows the real one (testing, editing)
    let right = match t.status {
        Status::Running => format!("working · {age}"),
        Status::Checking => format!("gate · {age}"),
        _ => age,
    };
    let title = if t.title.is_empty() { &t.id } else { &t.title };
    let title = truncate(
        title,
        width.saturating_sub(5 + right.width()),
        theme.ellipsis,
    );
    let pad = width.saturating_sub(4 + title.width() + right.width());
    let gate_failed = t.gate.iter().flatten().any(|c| !c.passed);
    let bar = if sel { theme.bar } else { " " };
    Line::from(vec![
        Span::styled(bar, theme.accent),
        Span::raw(" "),
        theme.glyph(t.status, gate_failed, tick),
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
        Span::raw(right).dim(),
    ])
}

fn detail(f: &mut Frame, area: Rect, app: &App, theme: &Theme, focused: bool) {
    let block = pane("Task", focused, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some((t, _)) = app.tasks.get(app.selected) else {
        f.render_widget(Line::raw("No tasks yet.").dim(), inner);
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
        0 => summary(f, body, t),
        1 => activity_tab(f, body, &app.activity, theme),
        2 => gate_tab(f, body, t, &app.gate_log, theme),
        _ => diff_tab(f, body, &app.diff, theme),
    }
}

fn summary(f: &mut Frame, area: Rect, t: &Task) {
    let label = GROUPS
        .iter()
        .find(|(s, _)| *s == t.status)
        .map_or("", |g| g.1);
    let mut meta = vec![label.to_string()];
    meta.extend((!t.branch.is_empty()).then(|| t.branch.clone()));
    meta.extend(t.slot.map(|n| format!("slot {n}")));
    meta.extend(t.model.as_ref().map(|m| match &t.effort {
        Some(e) => format!("{m}/{e}"),
        None => m.clone(),
    }));
    meta.extend(t.ticket.clone());
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
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One line per tool call, following the tail: edits in the accent, shell commands in a
/// second hue, reads dim.
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
            other => (other, Style::new()),
        };
        let target = truncate(target, width.saturating_sub(8), theme.ellipsis);
        Line::from(vec![
            Span::styled(format!("{verb:<7} "), style),
            Span::raw(target).dim(),
        ])
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

/// Formats a duration as `45s`, `4m`, `2h` or `3d`, using the largest whole unit.
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
            tasks,
            selected: 0,
            detail: false,
            help: false,
            compose: None,
            notice: None,
            tab: 0,
            activity: Vec::new(),
            gate_log: String::new(),
            diff: Vec::new(),
            loaded: None,
            confirm: false,
            pager: None,
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
        assert_eq!(
            tab_screen(app, 0),
            [
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Diff                            │",
                "│                                                          │",
                "│ Reject negative max_delay                                │",
                "│ Review · u/reject-negative · slot 1 · opus/high · CC-687 │",
                "│                                                          │",
                "│ max_delay below zero now fails at parse time.            │",
                "│                                                          │",
                "│ A negative value panics in the retry loop.               │",
                "│                                                          │",
                "│                                                          │",
                "╰──────────────────────────────────────────────────────────╯",
                " n new task  j/k move  tab pane  1-4 tabs  d diff  x discard",
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
                "│ Summary  Activity  Gate  Diff                            │",
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
                " n new task  j/k move  tab pane  1-4 tabs  d diff  x discard",
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
                "│ Summary  Activity  Gate  Diff                            │",
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
                " n new task  j/k move  tab pane  1-4 tabs  d diff  x discard",
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
            tab_screen(app, 3),
            [
                " yogan · fuse-os  ✓ 1  ✗ 1  ⠋ 1  ○ 1                        ",
                "╭ Task ────────────────────────────────────────────────────╮",
                "│ Summary  Activity  Gate  Diff                            │",
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
                " n new task  j/k move  tab pane  1-4 tabs  d diff  x discard",
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
    }

    #[test]
    fn composed_task() {
        let other = Task {
            id: "t5".into(),
            branch: "u/reject-negative-max-delay".into(),
            ..Default::default()
        };
        let request = "\n  Reject negative max_delay!\nIt panics later.\n\n";
        let t = new_task(request, "CC-687", "u/", &[other], "t5".into()).unwrap();
        assert_eq!(
            (t.id.as_str(), t.title.as_str()),
            ("t5a", "Reject negative max_delay!")
        );
        assert_eq!(
            (t.body.as_str(), t.ticket.as_deref()),
            ("It panics later.", Some("CC-687"))
        );
        assert_eq!(
            (t.status, t.branch.as_str()),
            (Status::Approved, "u/reject-negative-max-delay-2")
        );
        assert!(new_task(" \n", "", "", &[], "t6".into()).is_err());
        assert_eq!(slug("Ünïcode & ...!"), "n-code");
        assert_eq!(
            slug("Split the ledger reconciliation job into per-account batches"),
            "split-the-ledger-reconciliation-job-into"
        );
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
                "│ Review                                    ││ Summary  Activity  Gate  Diff                       │",
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
                " n new task  j/k move  tab pane  1-4 tabs  d diff  x discard  ? help  q quit                        ",
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
                " n new task  j/k move  tab pane  1-4 tabs  x discard  ? help",
            ]
        );
    }
}
